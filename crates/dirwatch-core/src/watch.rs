//! Watches ONE directory for files matching a [`GlobMatcher`]. Ported from the .NET
//! `WatchService` (behavioral spec, build 2026-07-14.9).
//!
//! SWEEP-IS-AUTHORITY (the core design, do NOT change it): a periodic poll diffs the current
//! directory listing against last-known (length, mtime, exists) stamps and raises
//! Discovered/Activity/Missing/Reappeared. In the full app an FS watcher (the `notify` crate, to
//! be wired with the runtime event loop — DECISIONS R8) will only SCHEDULE debounced sweeps; it
//! never decides state. This is why network shares work even when ReadDirectoryChangesW misses
//! events (DECISIONS entry 10): the poll catches everything. Files are keyed by full path, so
//! same-named files in different subdirectories stay distinct.
//!
//! This module IS the authority half — the sweep — and is fully unit-tested here. The scheduling
//! half (notify + a poll timer calling `sweep()`) is a thin layer added with the GUI runtime.
//!
//! Depth 0 = top level only; Depth N descends N subdirectory levels. The FS watcher can only do
//! 0-or-unlimited recursion, so the finite depth is enforced by the manual walk here.
//!
//! Events are delivered through caller-registered callbacks (the Rust analogue of the .NET
//! `event Action<string>`). Callbacks are invoked OUTSIDE the internal lock, after the sweep
//! computes the full batch — same ordering guarantee the .NET version gives.

use crate::fskey::fs_key;
use crate::glob::GlobMatcher;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Upper clamp for watch depth — a safety ceiling far above typical use.
pub const MAX_DEPTH: i32 = 10;

/// How many consecutive sweeps a file may stay Missing before its bookkeeping is dropped (review
/// B4, DECISIONS R97). `known` used to remember every path ever seen, forever — a directory with
/// rotating dated names grew it without bound and the "gone" scan walked all of it every sweep.
/// Since review #4 (finding 1, DECISIONS R128) the pruned stamp is kept in a bounded side map, so a
/// pruned file that returns UNCHANGED is `Reappeared` (no Activity) like any short blip, and one
/// that returns changed is `Reappeared` + `Activity`; only a never-seen path is `Discovered`. Nothing
/// is pruned while the root itself is unreadable. The GUI keeps its own tile state, so pruning here
/// does not change what the user sees.
pub const MISSING_PRUNE_SWEEPS: u32 = 20;

/// Upper bound on the side map of PRUNED stamps (review #4 finding 1, DECISIONS R128). Pruning
/// (above) keeps `known` small, but forgetting a file's last stamp meant that when it came back
/// UNCHANGED after a long outage — the root or a subdirectory unreadable for more than
/// `MISSING_PRUNE_SWEEPS` sweeps (≈ 5 s at the default poll: a laptop sleep, a share re-mount) —
/// the sweep reported it as a brand-new `Discovered` + `Activity`, and the GUI auto-opened every
/// closed tile: the R111 storm, back for any outage longer than the prune window. The pruned stamp
/// is now remembered here so a returning file with an identical `(length, mtime)` raises
/// `Reappeared` (no Activity) exactly like a short blip. Bounded so a tree of forever-rotating names
/// cannot grow it without limit; at the bound the map is cleared (the worst case is then one
/// Discovered+Activity per pruned file that returns unchanged — today's behaviour — never a leak).
pub const PRUNED_STAMPS_MAX: usize = 10_000;

#[derive(Clone, Copy)]
struct Stamp {
    length: u64,
    write: Option<SystemTime>,
    exists: bool,
    /// Consecutive sweeps this entry has been Missing (0 while present). Drives pruning.
    missing_sweeps: u32,
}

impl Stamp {
    fn present(length: u64, write: Option<SystemTime>) -> Stamp {
        Stamp {
            length,
            write,
            exists: true,
            missing_sweeps: 0,
        }
    }
}

/// Which event a raised action is (used to batch then dispatch after the lock is released).
enum Event {
    Discovered(PathBuf),
    Activity(PathBuf),
    Missing(PathBuf),
    Reappeared(PathBuf),
    /// The watched directory itself could not be read (missing, permission denied, share gone).
    /// Carries a human-readable reason. Edge-triggered: raised once per failure episode.
    Error(String),
    /// The watched directory reads again after an `Error` episode (review #2 finding 2, DECISIONS
    /// R111). Edge-triggered: raised once, on the first successful sweep after a failure, so the
    /// UI can clear the error it showed.
    Recovered,
}

/// What the last sweep's directory walk cost (review #6, DECISIONS R140): matching files found,
/// directories walked (root included; a symlinked directory skipped as an alias is not counted), and
/// the wall time of the walk. The runtime traces it under `DIRWATCH_DEBUG` so a hardware run's
/// artifacts carry the per-sweep cost — the number review #6 finding 1 could not measure off-hardware.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SweepStats {
    pub files: usize,
    pub dirs: usize,
    pub millis: u64,
}

type Callback = Box<dyn FnMut(&Path) + Send>;
type ErrorCallback = Box<dyn FnMut(&str) + Send>;
type RecoveredCallback = Box<dyn FnMut() + Send>;

/// Watches one directory; the sweep is the authority.
pub struct WatchService {
    directory: PathBuf,
    matcher: GlobMatcher,
    poll_interval_ms: u32,
    depth: i32,

    known: HashMap<String, Stamp>, // keyed by lowercased full path (OrdinalIgnoreCase analogue)
    /// Original-cased path for each key, so a Missing event can carry the REAL path instead of the
    /// lowercased key (the key is lowercased for case-insensitive matching; raising it would hand
    /// callers a wrong-cased string — every other event carries the on-disk path).
    orig_paths: HashMap<String, PathBuf>,
    /// Last-known stamps of entries PRUNED from `known` (review #4 finding 1, DECISIONS R128): a
    /// path that returns with an identical stamp is `Reappeared`, not `Discovered`+`Activity`.
    /// Bounded by `PRUNED_STAMPS_MAX`; a pruned path that returns is removed again.
    pruned: HashMap<String, Stamp>,
    started: bool,
    /// Cost of the last sweep's walk (R140); see [`SweepStats`].
    last_sweep: SweepStats,
    /// True while the root directory is unreadable — so the error is raised ONCE per episode
    /// (review B2, DECISIONS R97), not on every poll; cleared when a sweep reads the root again.
    root_error_reported: bool,

    on_discovered: Option<Callback>,
    on_activity: Option<Callback>,
    on_missing: Option<Callback>,
    on_reappeared: Option<Callback>,
    on_error: Option<ErrorCallback>,
    on_recovered: Option<RecoveredCallback>,
}

impl WatchService {
    /// `poll_interval_ms` is clamped to the shared [`crate::POLL_INTERVAL_FLOOR_MS`] floor (R118);
    /// `depth` to `0..=MAX_DEPTH`.
    pub fn new(
        directory: impl AsRef<Path>,
        matcher: GlobMatcher,
        poll_interval_ms: u32,
        depth: i32,
    ) -> Self {
        WatchService {
            directory: directory.as_ref().to_path_buf(),
            matcher,
            poll_interval_ms: poll_interval_ms.max(crate::POLL_INTERVAL_FLOOR_MS),
            depth: depth.clamp(0, MAX_DEPTH),
            known: HashMap::new(),
            orig_paths: HashMap::new(),
            pruned: HashMap::new(),
            started: false,
            last_sweep: SweepStats::default(),
            root_error_reported: false,
            on_discovered: None,
            on_activity: None,
            on_missing: None,
            on_reappeared: None,
            on_error: None,
            on_recovered: None,
        }
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }
    pub fn poll_interval_ms(&self) -> u32 {
        self.poll_interval_ms
    }
    pub fn depth(&self) -> i32 {
        self.depth
    }

    pub fn on_discovered(&mut self, cb: impl FnMut(&Path) + Send + 'static) {
        self.on_discovered = Some(Box::new(cb));
    }
    pub fn on_activity(&mut self, cb: impl FnMut(&Path) + Send + 'static) {
        self.on_activity = Some(Box::new(cb));
    }
    pub fn on_missing(&mut self, cb: impl FnMut(&Path) + Send + 'static) {
        self.on_missing = Some(Box::new(cb));
    }
    pub fn on_reappeared(&mut self, cb: impl FnMut(&Path) + Send + 'static) {
        self.on_reappeared = Some(Box::new(cb));
    }
    /// The watched directory could not be read (raised once per failure episode, review B2).
    pub fn on_error(&mut self, cb: impl FnMut(&str) + Send + 'static) {
        self.on_error = Some(Box::new(cb));
    }
    /// The watched directory reads again after an error episode (raised once per recovery,
    /// review #2 finding 2).
    pub fn on_recovered(&mut self, cb: impl FnMut() + Send + 'static) {
        self.on_recovered = Some(Box::new(cb));
    }

    /// Initial sweep: discovery only, no activity (dormant buttons), then mark started so future
    /// new files also raise Activity. (In the full app, a poll timer + FS watcher drive subsequent
    /// sweeps; those are a thin scheduling layer over this same method — the sweep is authority.)
    pub fn start(&mut self) {
        self.sweep();
        self.started = true;
    }

    /// Matching files within the depth limit, each with the (length, mtime) stamp taken from the
    /// directory listing itself. Returns `Err` only when the ROOT directory cannot be read (a
    /// subdirectory that fails to read is skipped, as before). Also counts the directories walked
    /// (into `dirs`) for the per-sweep diagnostics.
    ///
    /// ONE STAT PER FILE (review B4, DECISIONS R97; restored at `.i48`, review #6 finding 1, R140):
    /// for a REAL file the stamp comes from `DirEntry::metadata()`, not a second
    /// `fs::metadata(&path)` after the walk. On Windows that metadata is already in the
    /// `FindNextFile` record (zero extra syscalls); on Linux/macOS it is one `lstat` instead of two.
    /// On a network share each stat is a round trip, so this halves (Windows: removes) the per-poll
    /// cost that made large trees unsustainable at the 250 ms default. Only a SYMLINK pays one
    /// `fs::metadata` (which follows) — `.i47` paid it for every file, which on Windows is a
    /// `CreateFile` + `GetFileInformationByHandle` + `CloseHandle` per file per poll (std
    /// `sys/fs/windows.rs`), the exact cost B4 removed.
    fn enumerate_matching(
        &self,
        dirs: &mut usize,
    ) -> Result<Vec<(PathBuf, u64, Option<SystemTime>)>, String> {
        let mut acc = Vec::new();
        let entries = std::fs::read_dir(&self.directory)
            .map_err(|e| format!("cannot read {}: {e}", self.directory.display()))?;
        // The per-sweep set of directories already walked, by CANONICAL path (review #6 finding 2,
        // DECISIONS R141): a symlinked directory that resolves to one already visited — a link to
        // itself, to an ancestor, or to a sibling already walked — is skipped, so a cycle costs one
        // visit per real directory instead of k^depth, and an alias shows its files ONCE. The root
        // is canonicalised once per sweep; a failure (a root that vanished mid-sweep, an exotic
        // mount) falls back to the path as typed, which still dedupes links that resolve to the
        // same canonical form as each other.
        let root_canon =
            std::fs::canonicalize(&self.directory).unwrap_or_else(|_| self.directory.clone());
        let mut visited: HashSet<PathBuf> = HashSet::new();
        visited.insert(root_canon.clone());
        *dirs = 1;
        self.walk_entries(entries, &root_canon, 0, &mut visited, dirs, &mut acc);
        Ok(acc)
    }

    /// Walk one directory. `canon` is its canonical path (the visited-set key).
    #[allow(clippy::too_many_arguments)]
    fn walk(
        &self,
        dir: &Path,
        canon: &Path,
        level: i32,
        visited: &mut HashSet<PathBuf>,
        dirs: &mut usize,
        acc: &mut Vec<(PathBuf, u64, Option<SystemTime>)>,
    ) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            *dirs += 1;
            self.walk_entries(entries, canon, level, visited, dirs, acc);
        }
    }

    fn walk_entries(
        &self,
        entries: std::fs::ReadDir,
        canon: &Path,
        level: i32,
        visited: &mut HashSet<PathBuf>,
        dirs: &mut usize,
        acc: &mut Vec<(PathBuf, u64, Option<SystemTime>)>,
    ) {
        // Real subdirectories are walked BEFORE symlinked ones at each level, so when a link
        // aliases a real directory at the same level the REAL name is the one that wins the
        // visited set (R141: "an alias shows once, under the real name"; a link that reaches a
        // real directory the walk has not met yet — e.g. a link at level 1 to a real directory at
        // level 3 — wins by first visit instead, which is the documented residual).
        let mut real_dirs: Vec<PathBuf> = Vec::new();
        let mut link_dirs: Vec<PathBuf> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            // `entry.file_type()` does NOT follow symlinks, so a symlinked file or a
            // symlinked/junction directory reports neither is_file() nor is_dir() and would be
            // silently dropped (review 2026-09-16 finding 2, DECISIONS R137). A stable
            // `current.log -> today.log` pointer, or a junction on Windows, is a common logging
            // idiom, so we follow: resolve the target's type with ONE `fs::metadata` (which DOES
            // follow) and classify on that. A broken/dangling link errors here and is skipped
            // (no phantom file). That same metadata is the link's stamp below: for a symlinked
            // file `entry.metadata()` would return the LINK's own stamp (length 0), so appends to
            // the target would never register (R137). A real entry never pays this call (B4).
            let (ft, link_meta) = if ft.is_symlink() {
                match std::fs::metadata(&path) {
                    Ok(m) => (m.file_type(), Some(m)),
                    Err(_) => continue,
                }
            } else {
                (ft, None)
            };
            if ft.is_dir() {
                if link_meta.is_some() {
                    link_dirs.push(path);
                } else {
                    real_dirs.push(path);
                }
            } else if ft.is_file() {
                // Match on the LOSSY name (review #6 finding 5, R143): `to_str()` returned `None`
                // for a non-UTF-8 name on Linux, so such a file silently never matched. The lossy
                // form keeps the `.log` suffix intact; the GUI opens the file by its real `PathBuf`.
                let name = match path.file_name() {
                    Some(n) => n.to_string_lossy().into_owned(),
                    None => continue,
                };
                if self.matcher.is_match(&name) {
                    let m = match link_meta {
                        Some(m) => Ok(m),
                        None => entry.metadata(),
                    };
                    if let Ok(m) = m {
                        acc.push((path, m.len(), m.modified().ok()));
                    }
                }
            }
        }
        if level >= self.depth {
            return;
        }
        for s in real_dirs {
            // A real directory's canonical path is its parent's plus its own name (no syscall).
            let c = match s.file_name() {
                Some(n) => canon.join(n),
                None => continue,
            };
            if visited.insert(c.clone()) {
                self.walk(&s, &c, level + 1, visited, dirs, acc);
            }
        }
        for s in link_dirs {
            // One `canonicalize` per symlinked directory per sweep: resolves the target so a link
            // to `.`, `..`, or an already-walked directory is recognised and skipped (R141). A
            // link whose target cannot be resolved is skipped like a dangling one.
            let c = match std::fs::canonicalize(&s) {
                Ok(c) => c,
                Err(_) => continue,
            };
            if visited.insert(c.clone()) {
                self.walk(&s, &c, level + 1, visited, dirs, acc);
            }
        }
    }

    /// Diff the directory against last-known stamps and raise the resulting events. Public so a
    /// "poll now" / manual refresh / timer / notify callback can drive it directly.
    pub fn sweep(&mut self) {
        let mut events: Vec<Event> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        let started = std::time::Instant::now();
        let mut dirs_walked = 0usize;
        let files = match self.enumerate_matching(&mut dirs_walked) {
            Ok(f) => {
                if self.root_error_reported {
                    // The episode is over: say so ONCE so the UI can clear its error note.
                    self.root_error_reported = false;
                    events.push(Event::Recovered);
                }
                f
            }
            Err(msg) => {
                // Root unreadable: report ONCE per episode, then treat the listing as empty so
                // every known file goes Missing (the truthful state: we cannot see them).
                if !self.root_error_reported {
                    self.root_error_reported = true;
                    events.push(Event::Error(msg));
                }
                Vec::new()
            }
        };
        self.last_sweep = SweepStats {
            files: files.len(),
            dirs: dirs_walked,
            millis: started.elapsed().as_millis() as u64,
        };
        for (path, len, write) in files {
            let key = fs_key(&path);
            seen.insert(key.clone());
            // Remember the original-cased path for this key so a later Missing event carries the
            // real path, not the folded key. Refresh it each sweep in case the on-disk casing
            // changed (rename-in-place).
            self.orig_paths.insert(key.clone(), path.clone());

            match self.known.get(&key).copied() {
                Some(prev) => {
                    if !prev.exists {
                        // Reappeared. Activity ONLY if the stamp actually changed (review #2
                        // finding 2, DECISIONS R111): a root that was unreadable for one poll (a
                        // share blip, a re-mounted drive) marks every file Missing, and on return
                        // the files are byte-for-byte and mtime-identical — reporting them all as
                        // written made every closed tile auto-open. A genuinely recreated file
                        // has a new stamp and is still Activity.
                        let changed = len != prev.length || write != prev.write;
                        self.known.insert(key, Stamp::present(len, write));
                        events.push(Event::Reappeared(path.clone()));
                        if changed {
                            events.push(Event::Activity(path));
                        }
                    } else if len != prev.length || write != prev.write {
                        self.known.insert(key, Stamp::present(len, write));
                        events.push(Event::Activity(path));
                    }
                }
                None => {
                    self.known.insert(key.clone(), Stamp::present(len, write));
                    match self.pruned.remove(&key) {
                        // A path pruned during a LONG absence (review #4 finding 1): its
                        // bookkeeping was dropped, not the file's identity. Unchanged stamp =>
                        // the same file came back (root or subdirectory readable again) =>
                        // `Reappeared` only, exactly as a short blip (R111). A changed stamp is
                        // a genuine write and stays Activity.
                        Some(prev) => {
                            let changed = len != prev.length || write != prev.write;
                            events.push(Event::Reappeared(path.clone()));
                            if changed {
                                events.push(Event::Activity(path));
                            }
                        }
                        None => {
                            let started = self.started;
                            events.push(Event::Discovered(path.clone()));
                            if started {
                                events.push(Event::Activity(path));
                            }
                        }
                    }
                }
            }
        }

        // Anything previously present but not seen this sweep is now missing; anything ALREADY
        // missing ages toward pruning (review B4).
        let mut newly_gone: Vec<String> = Vec::new();
        let mut prune: Vec<(String, Stamp)> = Vec::new();
        // While the ROOT is unreadable nothing is pruned (review #4 finding 1): "we cannot see the
        // directory" is not "the file is gone", and pruning during the outage is what turned every
        // returning file into Activity. Missing counts still age, so a file that is really gone
        // once the root reads again is pruned on the next sweep.
        let root_down = self.root_error_reported;
        for (k, v) in self.known.iter_mut() {
            if seen.contains(k) {
                continue;
            }
            if v.exists {
                v.exists = false;
                v.missing_sweeps = 1;
                newly_gone.push(k.clone());
            } else {
                v.missing_sweeps = v.missing_sweeps.saturating_add(1);
                if v.missing_sweeps > MISSING_PRUNE_SWEEPS && !root_down {
                    prune.push((k.clone(), *v));
                }
            }
        }
        for (k, stamp) in prune {
            self.known.remove(&k);
            self.orig_paths.remove(&k);
            if self.pruned.len() >= PRUNED_STAMPS_MAX {
                self.pruned.clear();
            }
            self.pruned.insert(k, stamp);
        }
        for k in newly_gone {
            // Raise the ORIGINAL-cased path (the .NET version raised the full-cased dictionary key).
            // Fall back to the key only if we somehow never recorded the original.
            let path = self
                .orig_paths
                .get(&k)
                .cloned()
                .unwrap_or_else(|| PathBuf::from(&k));
            events.push(Event::Missing(path));
        }

        // Dispatch outside the (conceptual) lock, in order.
        self.dispatch(events);
    }

    fn dispatch(&mut self, events: Vec<Event>) {
        for ev in events {
            match ev {
                Event::Discovered(p) => {
                    if let Some(cb) = self.on_discovered.as_mut() {
                        cb(&p);
                    }
                }
                Event::Activity(p) => {
                    if let Some(cb) = self.on_activity.as_mut() {
                        cb(&p);
                    }
                }
                Event::Missing(p) => {
                    if let Some(cb) = self.on_missing.as_mut() {
                        cb(&p);
                    }
                }
                Event::Reappeared(p) => {
                    if let Some(cb) = self.on_reappeared.as_mut() {
                        cb(&p);
                    }
                }
                Event::Error(msg) => {
                    if let Some(cb) = self.on_error.as_mut() {
                        cb(&msg);
                    }
                }
                Event::Recovered => {
                    if let Some(cb) = self.on_recovered.as_mut() {
                        cb();
                    }
                }
            }
        }
    }

    /// What the last sweep's walk cost (R140): files matched, directories walked, milliseconds.
    pub fn last_sweep(&self) -> SweepStats {
        self.last_sweep
    }

    /// Number of tracked (present or recently-missing) entries — exposed for the pruning test.
    pub fn tracked_count(&self) -> usize {
        self.known.len()
    }

    /// Number of remembered PRUNED stamps (review #4 finding 1) — exposed for its tests.
    pub fn pruned_count(&self) -> usize {
        self.pruned.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::unique;
    use std::fs;

    fn file_name(p: &Path) -> String {
        p.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string()
    }

    /// Event-collecting harness mirroring WatchServiceTests.cs. Callbacks require Send, so the
    /// collectors are Arc<Mutex<Vec<String>>>; writes bump a counter to force a distinct,
    /// increasing mtime on coarse filesystems (the .NET tests do the same via SetLastWriteTimeUtc).
    struct W {
        dir: PathBuf,
        disc: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        act: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        miss: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        reap: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        errs: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        recovered: std::sync::Arc<std::sync::atomic::AtomicU32>,
        ctr: u64,
    }

    impl W {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("dww_{}", unique()));
            fs::create_dir_all(&dir).unwrap();
            W {
                dir,
                disc: Default::default(),
                act: Default::default(),
                miss: Default::default(),
                reap: Default::default(),
                errs: Default::default(),
                recovered: Default::default(),
                ctr: 0,
            }
        }
        fn svc(&self, depth: i32) -> WatchService {
            let mut s = WatchService::new(&self.dir, GlobMatcher::new(None), 100, depth);
            let d = self.disc.clone();
            s.on_discovered(move |p| d.lock().unwrap().push(file_name(p)));
            let a = self.act.clone();
            s.on_activity(move |p| a.lock().unwrap().push(file_name(p)));
            let m = self.miss.clone();
            s.on_missing(move |p| m.lock().unwrap().push(file_name(p)));
            let r = self.reap.clone();
            s.on_reappeared(move |p| r.lock().unwrap().push(file_name(p)));
            let e = self.errs.clone();
            s.on_error(move |m| e.lock().unwrap().push(m.to_string()));
            let rc = self.recovered.clone();
            s.on_recovered(move || {
                rc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            });
            s
        }
        fn write(&mut self, name: &str, content: &str) {
            let p = self.dir.join(name);
            fs::write(&p, content).unwrap();
            self.ctr += 1;
            let mt =
                SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_752_496_800 + self.ctr);
            filetime::set_file_mtime(&p, filetime::FileTime::from_system_time(mt)).ok();
        }
        fn has(v: &std::sync::Arc<std::sync::Mutex<Vec<String>>>, name: &str) -> bool {
            v.lock().unwrap().iter().any(|x| x == name)
        }
        fn empty(v: &std::sync::Arc<std::sync::Mutex<Vec<String>>>) -> bool {
            v.lock().unwrap().is_empty()
        }
        fn clear(v: &std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
            v.lock().unwrap().clear();
        }
    }

    impl Drop for W {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    // Ported from WatchServiceTests.cs (parity oracle).

    #[test]
    fn start_on_empty_dir_raises_nothing() {
        let h = W::new();
        let mut s = h.svc(0);
        s.start();
        assert!(W::empty(&h.disc));
        assert!(W::empty(&h.act));
    }

    #[test]
    fn existing_files_at_start_are_dormant_discovered_not_active() {
        let mut h = W::new();
        h.write("old.log", "x\n");
        let mut s = h.svc(0);
        s.start();
        assert!(W::has(&h.disc, "old.log"));
        assert!(!W::has(&h.act, "old.log"));
    }

    #[test]
    fn new_file_after_start_is_discovered_and_active() {
        let mut h = W::new();
        let mut s = h.svc(0);
        s.start();
        h.write("a.log", "x\n");
        s.sweep();
        assert!(W::has(&h.disc, "a.log"));
        assert!(W::has(&h.act, "a.log"));
    }

    #[test]
    fn append_raises_activity() {
        let mut h = W::new();
        let mut s = h.svc(0);
        s.start();
        h.write("a.log", "x\n");
        s.sweep();
        W::clear(&h.act);
        h.write("a.log", "x\nmore\n");
        s.sweep();
        assert!(W::has(&h.act, "a.log"));
    }

    #[test]
    fn delete_raises_missing_then_reappear() {
        let mut h = W::new();
        let mut s = h.svc(0);
        s.start();
        h.write("a.log", "x\n");
        s.sweep();
        fs::remove_file(h.dir.join("a.log")).unwrap();
        s.sweep();
        assert!(W::has(&h.miss, "a.log"));

        h.write("a.log", "back\n");
        s.sweep();
        assert!(W::has(&h.reap, "a.log"));
        assert!(W::has(&h.act, "a.log"));
    }

    #[test]
    fn missing_event_carries_original_cased_path_not_lowercased() {
        // The dictionary key is lowercased for case-insensitive matching, but a Missing event must
        // deliver the REAL on-disk path (original casing), like every other event — not the
        // lowercased key. A mixed-case name would come back all-lowercase if the old
        // reconstruct-from-key behavior were still in place.
        let mut h = W::new();
        let mut s = h.svc(0);
        s.start();
        h.write("MixedCase.LOG", "x\n");
        s.sweep();
        std::fs::remove_file(h.dir.join("MixedCase.LOG")).unwrap();
        s.sweep();
        // file_name() of the delivered path preserves casing; a lowercased path would yield
        // "mixedcase.log" and fail this.
        assert!(W::has(&h.miss, "MixedCase.LOG"));
        assert!(!W::has(&h.miss, "mixedcase.log"));
    }

    #[test]
    fn non_matching_files_ignored() {
        let mut h = W::new();
        let mut s = h.svc(0);
        s.start();
        h.write("readme.md", "x\n");
        s.sweep();
        assert!(W::empty(&h.disc));
        assert!(W::empty(&h.act));
    }

    #[test]
    fn subdirectory_files_ignored_at_depth_zero() {
        let h = W::new();
        let sub = h.dir.join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("deep.log"), "x\n").unwrap();
        let mut s = h.svc(0);
        s.start();
        assert!(!W::has(&h.disc, "deep.log"));
    }

    #[test]
    fn subdirectory_files_discovered_at_depth_one() {
        let h = W::new();
        let sub = h.dir.join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("deep.log"), "x\n").unwrap();
        let mut s = h.svc(1);
        s.start();
        assert!(W::has(&h.disc, "deep.log"));
    }

    // --- review 2026-09-16 finding 2 (DECISIONS R137): follow symlinks/junctions ---

    #[test]
    #[cfg(unix)]
    fn a_symlinked_matching_file_is_discovered_with_the_targets_length() {
        // A `current.log -> real/today.log` pointer (a common logging idiom) must be watched,
        // and its stamp must be the TARGET's, so an append to the target registers as Activity.
        use std::os::unix::fs::symlink;
        let h = W::new();
        let real = h.dir.join("real");
        fs::create_dir_all(&real).unwrap();
        fs::write(real.join("target.log"), "hello\n").unwrap();
        symlink(real.join("target.log"), h.dir.join("current.log")).unwrap();
        let mut s = h.svc(0); // depth 0: the target lives in real/ (level 1) and must NOT be
                              // reached directly, so only the link at the top level is discovered.
        s.start();
        assert!(W::has(&h.disc, "current.log"));
        // The link carries the target's non-zero length: appending to the target is Activity.
        W::clear(&h.act);
        fs::write(real.join("target.log"), "hello\nmore\n").unwrap();
        let mt = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_752_500_000);
        filetime::set_file_mtime(
            real.join("target.log"),
            filetime::FileTime::from_system_time(mt),
        )
        .ok();
        s.sweep();
        assert!(W::has(&h.act, "current.log"));
    }

    #[test]
    #[cfg(unix)]
    fn files_under_a_symlinked_directory_are_discovered() {
        // A junction / symlinked directory (`linkdir -> /elsewhere/real`, OUTSIDE the watched
        // tree — the R137 use case) must be descended like a real one.
        use std::os::unix::fs::symlink;
        let h = W::new();
        let elsewhere = std::env::temp_dir().join(format!("dww_out_{}", unique()));
        fs::create_dir_all(&elsewhere).unwrap();
        fs::write(elsewhere.join("deep.log"), "x\n").unwrap();
        symlink(&elsewhere, h.dir.join("linkdir")).unwrap();
        let mut s = h.svc(1);
        s.start();
        assert!(W::has(&h.disc, "deep.log"));
        let _ = fs::remove_dir_all(&elsewhere);
    }

    // --- .i48 (review #6 findings 1, 2, 5; DECISIONS R140/R141/R143) ---

    /// Collect the full discovered PATHS (not just names) for the alias/cycle tests.
    #[cfg(unix)]
    fn discovered_paths(dir: &Path, depth: i32) -> Vec<String> {
        let out: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let mut s = WatchService::new(dir, GlobMatcher::new(None), 100, depth);
        let o = out.clone();
        s.on_discovered(move |p| o.lock().unwrap().push(p.to_string_lossy().to_string()));
        s.start();
        let mut v = out.lock().unwrap().clone();
        v.sort();
        v
    }

    #[test]
    #[cfg(unix)]
    fn a_symlink_cycle_costs_one_visit_per_real_directory() {
        // REGRESSION (review #6 finding 2, R141): two self-links at depth 10 produced 2047 paths
        // for ONE file (k^depth); the visited set makes a cycle cost one visit per real directory.
        use std::os::unix::fs::symlink;
        let h = W::new();
        fs::write(h.dir.join("a.log"), "x\n").unwrap();
        symlink(".", h.dir.join("l1")).unwrap();
        symlink(".", h.dir.join("l2")).unwrap();
        let sub = h.dir.join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("s.log"), "x\n").unwrap();
        symlink("..", sub.join("up")).unwrap(); // a link to the parent
        let paths = discovered_paths(&h.dir, 10);
        assert_eq!(
            paths,
            vec![
                h.dir.join("a.log").to_string_lossy().to_string(),
                sub.join("s.log").to_string_lossy().to_string(),
            ],
            "exactly one path per real file: {paths:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_link_that_aliases_a_sibling_directory_shows_its_files_once_under_the_real_name() {
        // review #6 finding 2 (W2): `current -> 2026-09-16` at the same level. The real directory
        // is walked first and wins the visited set; the link is skipped (R141, the user's 3-A).
        use std::os::unix::fs::symlink;
        let h = W::new();
        let real = h.dir.join("2026-09-16");
        fs::create_dir_all(&real).unwrap();
        fs::write(real.join("app.log"), "x\n").unwrap();
        symlink("2026-09-16", h.dir.join("current")).unwrap();
        let paths = discovered_paths(&h.dir, 1);
        assert_eq!(
            paths,
            vec![real.join("app.log").to_string_lossy().to_string()],
            "one file, one path, the real name: {paths:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_symlinked_file_next_to_its_target_is_still_two_tiles() {
        // Files are NOT deduplicated (R141): `current.log -> app.log` in the same directory is the
        // idiom R137 exists for, and both names are legitimately watched.
        use std::os::unix::fs::symlink;
        let h = W::new();
        fs::write(h.dir.join("app.log"), "x\n").unwrap();
        symlink("app.log", h.dir.join("current.log")).unwrap();
        let paths = discovered_paths(&h.dir, 0);
        assert_eq!(paths.len(), 2, "{paths:?}");
    }

    #[test]
    #[cfg(unix)]
    fn a_real_files_stamp_comes_from_the_listing_and_a_links_from_its_target() {
        // review #6 finding 1 (R140): the two stamp sources must agree with what each entry kind
        // reports — `entry.metadata()` (lstat, the listing) for a real file, `fs::metadata` (the
        // target) for a link. A link's OWN lstat length (the target path's byte length) must never
        // be what the sweep records.
        use std::os::unix::fs::symlink;
        let h = W::new();
        let real = h.dir.join("real");
        fs::create_dir_all(&real).unwrap();
        fs::write(real.join("target.log"), "0123456789\n").unwrap(); // 11 bytes
        symlink(real.join("target.log"), h.dir.join("current.log")).unwrap();
        fs::write(h.dir.join("plain.log"), "abc\n").unwrap(); // 4 bytes
        let s = h.svc(0);
        let mut dirs = 0;
        let mut got: Vec<(String, u64)> = s
            .enumerate_matching(&mut dirs)
            .unwrap()
            .into_iter()
            .map(|(p, len, _)| (file_name(&p), len))
            .collect();
        got.sort();
        assert_eq!(
            got,
            vec![
                ("current.log".to_string(), 11),
                ("plain.log".to_string(), 4)
            ]
        );
        assert_eq!(dirs, 1, "root only at depth 0");
    }

    #[test]
    #[cfg(unix)]
    fn a_non_utf8_file_name_still_matches_its_pattern() {
        // review #6 finding 5 (R143): `caf\xE9.log` (Latin-1) used to fall through `to_str()` and
        // never match `*.log`.
        use std::os::unix::ffi::OsStrExt;
        let h = W::new();
        let bad = h.dir.join(std::ffi::OsStr::from_bytes(b"caf\xe9.log"));
        // Some filesystems refuse a non-UTF-8 name at creation: APFS (macOS) rejects it with EILSEQ
        // (raw_os_error 92 on macOS, 84 on Linux). The `to_string_lossy`/real-path matching under
        // test can only be exercised on a filesystem that will HOLD such a name — every ext4/Btrfs
        // Linux host does — so where the write itself is refused, SKIP loudly rather than panic on
        // the fixture (R152, the root-skip pattern; the fix is verified on a Linux host that keeps
        // the name). Everywhere the name is accepted the assertions below run unconditionally.
        if let Err(e) = fs::write(&bad, "x\n") {
            eprintln!(
                "SKIPPED a_non_utf8_file_name_still_matches_its_pattern: this filesystem refuses \
                 the non-UTF-8 name caf\\xe9.log ({e}) — the pattern-match path is proved on a \
                 Linux host whose filesystem keeps the name"
            );
            return;
        }
        let s = h.svc(0);
        let mut dirs = 0;
        let got = s.enumerate_matching(&mut dirs).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(
            got[0].0, bad,
            "the REAL (non-UTF-8) path is what the sweep reports"
        );
    }

    #[test]
    fn sweep_stats_count_files_and_directories_walked() {
        // R140: the per-sweep diagnostics the runtime traces under DIRWATCH_DEBUG.
        let mut h = W::new();
        fs::create_dir_all(h.dir.join("a").join("b")).unwrap();
        h.write("one.log", "x\n");
        h.write("a/two.log", "x\n");
        h.write("a/b/three.log", "x\n");
        fs::write(h.dir.join("a/skip.bin"), "x").unwrap();
        let mut s = h.svc(1);
        s.start();
        let st = s.last_sweep();
        assert_eq!(
            (st.files, st.dirs),
            (2, 2),
            "depth 1: root + a walked (a/b is below the depth limit): {st:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_dangling_symlink_is_skipped_not_reported() {
        use std::os::unix::fs::symlink;
        let h = W::new();
        symlink(h.dir.join("does_not_exist.log"), h.dir.join("broken.log")).unwrap();
        let mut s = h.svc(0);
        s.start();
        assert!(!W::has(&h.disc, "broken.log"));
        assert!(W::empty(&h.errs));
    }

    #[test]
    fn below_the_depth_limit_is_not_discovered() {
        let h = W::new();
        let l2 = h.dir.join("a").join("b");
        fs::create_dir_all(&l2).unwrap();
        fs::write(l2.join("deep.log"), "x\n").unwrap(); // 2 levels down
        let mut s = h.svc(1);
        s.start();
        assert!(!W::has(&h.disc, "deep.log"));
    }

    // --- .i29 (review A1 / B2 / B4, DECISIONS R97) ---

    #[test]
    #[cfg(not(any(windows, target_os = "macos")))]
    fn case_variant_names_are_distinct_files_on_case_sensitive_fs() {
        // REGRESSION (review A1): `a.log` and `A.log` are two files on ext4. Before R97 both folded
        // onto one lowercased key: only one was Discovered, and every idle sweep raised two phantom
        // Activity events (each file's stamp differed from the OTHER file's stamp in the shared
        // slot) — a permanently "active" tile. Now: both Discovered, and an idle sweep is silent.
        let mut h = W::new();
        h.write("a.log", "1\n");
        h.write("A.log", "22222\n");
        let mut s = h.svc(0);
        s.start();
        assert!(W::has(&h.disc, "a.log"));
        assert!(W::has(&h.disc, "A.log"));
        for _ in 0..3 {
            s.sweep();
        }
        assert!(
            W::empty(&h.act),
            "idle sweeps must not raise Activity: {:?}",
            h.act.lock().unwrap()
        );
    }

    #[test]
    #[cfg(any(windows, target_os = "macos"))]
    fn case_variant_names_are_one_file_on_case_insensitive_fs() {
        // On a case-insensitive FS a rename-in-place `a.log` -> `A.log` is the SAME file: one
        // Discovered, no Missing/Reappeared churn.
        let mut h = W::new();
        h.write("a.log", "1\n");
        let mut s = h.svc(0);
        s.start();
        fs::rename(h.dir.join("a.log"), h.dir.join("A.log")).unwrap();
        s.sweep();
        assert!(W::empty(&h.miss));
        assert_eq!(h.disc.lock().unwrap().len(), 1);
    }

    #[test]
    fn unreadable_root_raises_error_once_then_missing_for_known_files() {
        // review B2: a vanished/unreadable root used to be indistinguishable from an empty one.
        let mut h = W::new();
        h.write("a.log", "x\n");
        let mut s = h.svc(0);
        s.start();
        fs::remove_dir_all(&h.dir).unwrap();
        s.sweep();
        s.sweep();
        s.sweep();
        assert_eq!(
            h.errs.lock().unwrap().len(),
            1,
            "error is edge-triggered (once per episode)"
        );
        assert!(h.errs.lock().unwrap()[0].contains("cannot read"));
        assert!(
            W::has(&h.miss, "a.log"),
            "known files go Missing when the root is unreadable"
        );
        // Root comes back: the episode ends and a later failure would report again.
        fs::create_dir_all(&h.dir).unwrap();
        s.sweep();
        fs::remove_dir_all(&h.dir).unwrap();
        s.sweep();
        assert_eq!(h.errs.lock().unwrap().len(), 2);
        fs::create_dir_all(&h.dir).unwrap(); // so Drop's cleanup has something to remove
    }

    #[test]
    fn long_missing_entries_are_pruned_and_rediscovered_on_return() {
        // review B4: `known` no longer grows forever with rotated-away names.
        let mut h = W::new();
        let mut s = h.svc(0);
        s.start();
        h.write("old.log", "x\n");
        s.sweep();
        assert_eq!(s.tracked_count(), 1);
        fs::remove_file(h.dir.join("old.log")).unwrap();
        for _ in 0..(MISSING_PRUNE_SWEEPS + 2) {
            s.sweep();
        }
        assert_eq!(s.tracked_count(), 0, "pruned after MISSING_PRUNE_SWEEPS");
        assert_eq!(
            h.miss.lock().unwrap().len(),
            1,
            "Missing raised exactly once"
        );
        // Comes back after pruning, CHANGED (a new mtime from `write`): since review #4 finding 1
        // (R128, amending B4) the pruned stamp is remembered, so this is a Reappeared + Activity —
        // the same pair a short absence produces — not a second Discovered.
        W::clear(&h.disc);
        h.write("old.log", "y\n");
        s.sweep();
        assert!(
            W::has(&h.reap, "old.log"),
            "a pruned file that returns is Reappeared"
        );
        assert!(
            W::has(&h.act, "old.log"),
            "…and Activity, because its stamp changed"
        );
        assert!(W::empty(&h.disc), "not a second Discovered");
        assert_eq!(s.pruned_count(), 0, "its pruned stamp was consumed");
    }

    // --- .i40 (review #4 finding 1, DECISIONS R128) ---

    #[test]
    fn long_root_outage_past_the_prune_window_reappears_unchanged_files_without_activity() {
        // REGRESSION (review #4 finding 1): an outage LONGER than MISSING_PRUNE_SWEEPS sweeps used to
        // prune every file, so on return the unchanged files came back Discovered+Activity — the
        // R111 auto-open storm for any outage > ~5 s. On .i39: 5 Activity events. Now: 5 Reappeared,
        // 0 Activity, and nothing is pruned while the root is unreadable.
        let mut h = W::new();
        for i in 0..5 {
            h.write(&format!("f{i}.log"), "x\n");
        }
        let mut s = h.svc(0);
        s.start();
        s.sweep();
        assert!(W::empty(&h.act), "idle: no activity");
        let away = h.dir.with_extension("away");
        fs::rename(&h.dir, &away).unwrap();
        for _ in 0..(MISSING_PRUNE_SWEEPS + 2) {
            s.sweep();
        }
        assert_eq!(
            s.tracked_count(),
            5,
            "nothing is pruned while the root is unreadable"
        );
        fs::rename(&away, &h.dir).unwrap();
        s.sweep();
        assert_eq!(h.reap.lock().unwrap().len(), 5, "all five Reappear");
        assert!(
            W::empty(&h.act),
            "unchanged files must NOT be Activity after a long outage: {:?}",
            h.act.lock().unwrap()
        );
        assert_eq!(h.recovered.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn long_subdirectory_outage_reappears_unchanged_files_without_activity() {
        // The subdirectory variant of finding 1: a subdir renamed away is NOT a root error (the walk
        // just skips it), so its files ARE pruned after the window — and must come back via the
        // pruned-stamp side map as Reappeared (no Activity), not Discovered+Activity.
        let h = W::new();
        let sub = h.dir.join("sub");
        fs::create_dir_all(&sub).unwrap();
        for i in 0..3 {
            fs::write(sub.join(format!("s{i}.log")), "x\n").unwrap();
        }
        let mut s = h.svc(1);
        s.start();
        s.sweep();
        assert!(W::empty(&h.act));
        // Move it OUTSIDE the watched root (a sibling of the root), not to another name inside it.
        let away = h.dir.with_extension("subaway");
        fs::rename(&sub, &away).unwrap();
        for _ in 0..(MISSING_PRUNE_SWEEPS + 2) {
            s.sweep();
        }
        assert_eq!(
            s.tracked_count(),
            0,
            "subdir files are pruned (no root error)"
        );
        assert_eq!(s.pruned_count(), 3, "…but their stamps are remembered");
        fs::rename(&away, &sub).unwrap();
        s.sweep();
        assert_eq!(h.reap.lock().unwrap().len(), 3, "all three Reappear");
        assert!(
            W::empty(&h.act),
            "unchanged subdir files must NOT be Activity: {:?}",
            h.act.lock().unwrap()
        );
        assert_eq!(s.pruned_count(), 0, "consumed on return");
        // A genuinely NEW file in that subdir is still Discovered + Activity.
        fs::write(sub.join("new.log"), "n\n").unwrap();
        s.sweep();
        assert!(W::has(&h.disc, "new.log"));
        assert!(W::has(&h.act, "new.log"));
        let _ = fs::remove_dir_all(&away);
    }

    #[test]
    fn pruned_stamp_map_is_bounded() {
        // The side map must never grow without limit (the B4 concern it must not reintroduce): past
        // PRUNED_STAMPS_MAX it is cleared, never larger.
        let mut h = W::new();
        let mut s = h.svc(0);
        s.start();
        // Fill it well past the bound via many prune cycles of the same handful of names.
        let mut n = 0usize;
        while n <= PRUNED_STAMPS_MAX + 5 {
            for j in 0..8 {
                h.write(&format!("r{n}_{j}.log"), "x\n");
            }
            s.sweep();
            for j in 0..8 {
                let _ = fs::remove_file(h.dir.join(format!("r{n}_{j}.log")));
            }
            for _ in 0..(MISSING_PRUNE_SWEEPS + 2) {
                s.sweep();
            }
            n += 8;
            assert!(
                s.pruned_count() <= PRUNED_STAMPS_MAX,
                "bounded at every step"
            );
        }
        assert!(s.pruned_count() <= PRUNED_STAMPS_MAX);
    }

    #[test]
    fn sweep_uses_the_listing_stamp_so_a_size_change_is_still_activity() {
        // The stamp now comes from DirEntry::metadata (review B4); make sure it still diffs.
        let mut h = W::new();
        let mut s = h.svc(0);
        s.start();
        h.write("a.log", "x\n");
        s.sweep();
        W::clear(&h.act);
        h.write("a.log", "x\nlonger\n");
        s.sweep();
        assert!(W::has(&h.act, "a.log"));
    }

    // --- .i32 (review #2 finding 2, DECISIONS R111) ---

    #[test]
    fn root_blip_reappears_files_without_activity_and_reports_recovery_once() {
        // REGRESSION (review #2 finding 2): a one-sweep root outage marked every file Missing and
        // the next sweep raised Reappeared + Activity for every unchanged file -> every closed
        // tile auto-opened in the GUI. On .i31 the activity list read ["f2.log", "f0.log", ...].
        let mut h = W::new();
        for i in 0..5 {
            h.write(&format!("f{i}.log"), "x\n");
        }
        let mut s = h.svc(0);
        s.start();
        s.sweep();
        assert!(W::empty(&h.act), "idle: no activity");
        let away = h.dir.with_extension("away");
        fs::rename(&h.dir, &away).unwrap(); // root unreadable for one sweep
        s.sweep();
        assert_eq!(h.errs.lock().unwrap().len(), 1);
        assert_eq!(h.miss.lock().unwrap().len(), 5, "all five go Missing");
        fs::rename(&away, &h.dir).unwrap(); // root back; files untouched
        s.sweep();
        assert_eq!(h.reap.lock().unwrap().len(), 5, "all five Reappear");
        assert!(
            W::empty(&h.act),
            "unchanged files must NOT be Activity: {:?}",
            h.act.lock().unwrap()
        );
        assert_eq!(
            h.recovered.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "Recovered raised once"
        );
        s.sweep();
        assert_eq!(
            h.recovered.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "Recovered is edge-triggered"
        );
        // A file that really changed during the outage IS activity on return.
        fs::rename(&h.dir, &away).unwrap();
        s.sweep();
        fs::write(away.join("f0.log"), "x\nmore\n").unwrap();
        fs::rename(&away, &h.dir).unwrap();
        s.sweep();
        assert!(W::has(&h.act, "f0.log"));
        assert_eq!(h.act.lock().unwrap().len(), 1, "only the changed file");
    }
}
