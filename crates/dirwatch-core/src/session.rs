//! UI-independent state machine for the main window's file buttons and tail windows. Ported
//! from the .NET `SessionModel` (behavioral spec, build 2026-07-14.9).
//!
//! Owns: button-state precedence, active-timeout decay, the closed-stays-closed rule, and the
//! max-open-windows cap accounting. It makes the decisions (auto-open? blocked by cap?); the UI
//! executes them and renders [`ButtonVis`]. Time is passed in explicitly (`now`) so the logic is
//! deterministic and unit-testable off any clock.

use crate::fskey::fs_key_str;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Upper ceiling for the max-open-windows setting (DECISIONS R29). A hard cap so `--max-windows`
/// or the Settings dialog can't request an unreasonable number of tail windows. The floor is 1.
pub const MAX_WINDOWS_CEILING: i32 = 50;

/// Hard cap on the number of DISTINCT DIRECTORIES that may contain matching files — i.e. the number
/// of directory BOXES the main window can show (DECISIONS R122). NOT directories WALKED: an empty or
/// non-matching folder the sweep crosses costs nothing here; only a directory that actually holds a
/// matched file becomes a box and counts. Pairs with the `--depth` clamp (`watch::MAX_DEPTH`) to
/// bound a foolish root/depth. NON-OVERRIDABLE by design (no CLI/Settings knob, the user 2026-09-13): a
/// legitimate watch is in the low tens of boxes and 200 boxes is already unreadable in the fixed
/// window, so hitting it is a misconfiguration signal, surfaced in the status strip. Once the cap is
/// reached, files in ALREADY-KNOWN directories keep being tracked; files in a NEW directory are
/// dropped and `dir_overflow` latches true (the watch stays useful on what it accepted — recoverable,
/// the user's choice A). Cleared by `reset()` (Restart re-scans fresh).
pub const MAX_DIR_BOXES: usize = 200;

/// Hard cap on the number of tracked WATCHED files — i.e. the number of file TILES the main window
/// can hold (REVIEW-2026-09-17 finding 1). Only glob-matched files (`*.log`/`*.txt` by default, or
/// the user's patterns) ever reach the model, so this caps matched files, not every file on disk.
/// Pairs with [`MAX_DIR_BOXES`]: the per-frame render cost is linear in tiles, and the model never
/// pruned an entry, so a directory of years of dated logs grew without bound and idle CPU with it
/// (40 %+ of a core at 5 000 tiles on Windows, `.i49`). Unlike the directory cap, this one does NOT
/// simply refuse the excess: refusing "the first 1 000 in filesystem order" would drop the NEWEST,
/// most-active logs (a fresh `app-2026-09-17.log` created after the cap fills). Instead, when full, a
/// new file EVICTS the least-useful existing tile — a Missing one, else an idle (closed, not
/// recently written) one, oldest by last-activity first — and is refused ONLY if every one of the
/// 1 000 is an open window (nothing safe to drop). So the tiles kept are the ones the user is
/// watching or that most recently changed. Fixed (no CLI/Settings knob): 1 000 tiles is already
/// unscrollable, so reaching it is a misconfiguration signal, surfaced in the status strip via
/// `file_overflow`. The 1-hour Missing-prune (a `Removed` event from the watch service) keeps a
/// normally-rotating directory far below this, so in ordinary use the cap never trips.
pub const MAX_FILES: usize = 1000;

/// Rendered appearance of a file button. Precedence: Missing > ActiveOpen > Open > ActiveClosed
/// > Unread > Idle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonVis {
    Idle,
    Open,
    ActiveOpen,
    ActiveClosed,
    Unread,
    Missing,
}

/// Outcome of an attempt to open a tail window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenResult {
    Opened,
    AlreadyOpen,
    BlockedByCap,
}

/// Per-file state, mutated only through [`SessionModel`].
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: String,
    pub window_open: bool,
    pub missing: bool,
    /// Changed while its window was closed.
    pub unread: bool,
    /// Closed stays closed.
    pub user_closed: bool,
    /// Seconds-since-epoch of the last activity, or `None` for never (== DateTime.MinValue).
    pub last_activity: Option<i64>,
    /// Seconds-since-epoch when this entry FIRST became Missing in the current absence, or `None`
    /// while present. Drives the 1-hour tile prune (`prune_missing`): a file gone continuously for
    /// the grace period has its tile removed so a rotating directory stays bounded and shows only
    /// live files (REVIEW-2026-09-17 finding 1 growth path / finding 5). Cleared the moment the file
    /// is seen present again, so a file that rotates away and comes back never accrues toward pruning.
    pub missing_since: Option<i64>,
}

impl FileEntry {
    fn new(path: &str) -> Self {
        FileEntry {
            path: path.to_string(),
            window_open: false,
            missing: false,
            unread: false,
            user_closed: false,
            last_activity: None,
            missing_since: None,
        }
    }
}

/// The session state machine.
pub struct SessionModel {
    map: HashMap<String, FileEntry>,
    /// Case-folded parent-directory keys of every tracked file — the set whose size is the number of
    /// directory boxes. Kept incrementally as entries are added so the `MAX_DIR_BOXES` cap is an O(1)
    /// check per new file, not an O(files) scan (R122).
    dirs: HashSet<String>,
    /// Latches true the first time a file in a NEW directory is dropped for exceeding `MAX_DIR_BOXES`.
    /// The GUI reads it each tick to show the status-strip cap message. Cleared by `reset()`.
    dir_overflow: bool,
    /// Latches true the first time a new file is REFUSED by the `MAX_FILES` cap because every tracked
    /// tile is an open window (nothing safe to evict). An eviction (the normal at-cap path) does NOT
    /// set it — that is the cap working as intended, not an overflow. The GUI reads it each tick to
    /// show the status-strip file-cap message. Cleared by `reset()`.
    file_overflow: bool,
    /// Monotonic count of `MAX_FILES` cap EVICTIONS (a full model made room for a new file). The GUI
    /// traces it under DIRWATCH_DEBUG so a probe / gate step can assert the cap is evicting. Not
    /// cleared by `reset()` — it is a lifetime diagnostic counter, not visible state.
    evicted_total: u64,
    pub active_seconds: i64,
    pub max_windows: i32,
    pub no_open: bool,
    open_count: i32,
    /// Monotonic counter bumped ONLY when the SET of tracked paths changes — a new entry inserted
    /// (`add_or_drop`) or all entries cleared (`reset`). It does NOT change when an entry's mutable
    /// state changes (`missing`, `unread`, `window_open`, `last_activity`), because those affect a
    /// tile's COLOR but never which tiles exist, in which box, or in what order. The GUI caches the
    /// grouped+sorted tile layout (an O(files log files) + O(files×dirs) build that used to run every
    /// 100 ms tick — REVIEW-2026-09-17 finding 1) and rebuilds it only when this counter changes,
    /// recomputing each tile's colour per frame via the cheap `vis_of`. See `gui::main_view`.
    structure_rev: u64,
}

impl Default for SessionModel {
    fn default() -> Self {
        SessionModel {
            map: HashMap::new(),
            dirs: HashSet::new(),
            dir_overflow: false,
            file_overflow: false,
            evicted_total: 0,
            active_seconds: 5,
            max_windows: 10,
            no_open: false,
            open_count: 0,
            structure_rev: 0,
        }
    }
}

impl SessionModel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open_count(&self) -> i32 {
        self.open_count
    }

    /// Iterate the tracked entries.
    pub fn entries(&self) -> impl Iterator<Item = &FileEntry> {
        self.map.values()
    }

    /// Number of tracked entries.
    pub fn entry_count(&self) -> usize {
        self.map.len()
    }

    /// A token that changes exactly when the SET of tracked paths changes (an entry added, or all
    /// cleared). Unchanged by per-entry state changes. The GUI compares it against the token behind
    /// its cached tile layout: equal => the grouped+sorted layout is still valid and only per-tile
    /// colours are recomputed; different => rebuild the layout (finding 1).
    pub fn structure_rev(&self) -> u64 {
        self.structure_rev
    }

    fn key(path: &str) -> String {
        // Case-folded ONLY where the filesystem is case-insensitive (review A1, DECISIONS R97) —
        // the same rule `WatchService` and the GUI use, from the one shared `fskey` module.
        fs_key_str(path)
    }

    /// Case-folded key of a path's PARENT directory — the identity by which directory boxes are
    /// counted for the `MAX_DIR_BOXES` cap. Uses the same `fs_key_str` folding as file keys so the
    /// count matches how the GUI groups tiles into boxes. A path with no parent (a bare name) keys to
    /// the empty string — one bucket, harmless.
    fn dir_key(path: &str) -> String {
        let parent = Path::new(path)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        fs_key_str(&parent)
    }

    /// Number of DISTINCT directories that currently hold a tracked file (the box count).
    pub fn dir_count(&self) -> usize {
        self.dirs.len()
    }

    /// True once a file in a new directory was dropped for exceeding `MAX_DIR_BOXES` (R122). The GUI
    /// surfaces this in the status strip. Cleared by `reset()`.
    pub fn dir_overflow(&self) -> bool {
        self.dir_overflow
    }

    /// True once a new file was REFUSED by the `MAX_FILES` cap because every tile was an open window
    /// (nothing safe to evict). A normal at-cap eviction does NOT set this. The GUI surfaces it in the
    /// status strip. Cleared by `reset()`.
    pub fn file_overflow(&self) -> bool {
        self.file_overflow
    }

    /// Lifetime count of `MAX_FILES` cap evictions (finding 1 verification): each time a full model
    /// dropped a tile to admit a new file. The GUI traces increases under DIRWATCH_DEBUG.
    pub fn evicted_total(&self) -> u64 {
        self.evicted_total
    }

    /// Get or create the entry for `path`, enforcing the `MAX_DIR_BOXES` directory-box cap (R122).
    /// Returns `None` when the path is DROPPED — it is in a NEW directory and the model already holds
    /// `MAX_DIR_BOXES` directories — in which case nothing is inserted, its directory is not counted,
    /// and `dir_overflow` latches true. A path in an already-known directory, or one already tracked,
    /// is always inserted/returned. This is the single funnel through which the cap is enforced; the
    /// public mutators below route through it so a dropped path is inert in every one of them (no
    /// entry, no auto-open, no open-count churn) — accepted directories keep working, the excess is
    /// refused rather than silently melting the per-tick view (the user's choice A).
    fn add_or_drop(&mut self, path: &str) -> Option<&mut FileEntry> {
        let fkey = Self::key(path);
        if self.map.contains_key(&fkey) {
            return Some(self.map.get_mut(&fkey).unwrap());
        }
        let dkey = Self::dir_key(path);
        let dir_known = self.dirs.contains(&dkey);
        if !dir_known && self.dirs.len() >= MAX_DIR_BOXES {
            self.dir_overflow = true;
            return None;
        }
        // File cap (MAX_FILES): this is a brand-new entry. If the model is already full, make room by
        // evicting the least-useful tile — a Missing one, else an idle (closed, not open) one, oldest
        // by last-activity first — so the tiles kept are the ones being watched or most recently
        // active. If EVERY tile is an open window there is nothing safe to drop: latch `file_overflow`
        // and refuse the newcomer (it stays untracked, exactly like a dir-cap drop).
        if self.map.len() >= MAX_FILES {
            match self.pick_eviction_victim() {
                Some(victim) => {
                    self.remove_entry(&victim);
                    // Monotonic count so the GUI can trace an eviction under DIRWATCH_DEBUG (a probe /
                    // gate step asserts the cap is evicting, not just refusing). `remove_entry` bumps
                    // `structure_rev` too, so the tile-layout cache rebuilds.
                    self.evicted_total = self.evicted_total.wrapping_add(1);
                }
                None => {
                    self.file_overflow = true;
                    return None;
                }
            }
        }
        if !dir_known {
            self.dirs.insert(dkey);
        }
        // A brand-new entry is about to be inserted: the set of tracked paths changes, so the GUI's
        // cached tile layout must rebuild (finding 1). `contains_key` above already returned the
        // early path for a known key, so reaching here always means an insert.
        self.structure_rev = self.structure_rev.wrapping_add(1);
        Some(self.map.entry(fkey).or_insert_with(|| FileEntry::new(path)))
    }

    /// Choose the key of the tile to evict when the `MAX_FILES` cap is full and a new file arrives, or
    /// `None` when nothing is safe to evict (every tile is an open window). Priority is: first a
    /// Missing entry (oldest `last_activity` first, `None` counting as oldest); else a non-Missing,
    /// NON-open entry (idle/closed/unread), again oldest `last_activity` first. An OPEN window is never
    /// evicted. Ranking by ascending `last_activity` keeps the most recently active tiles (a
    /// just-written file has the newest stamp), which is the whole point — so `now` is not needed: the
    /// stored `last_activity` order already encodes "most recently active".
    fn pick_eviction_victim(&self) -> Option<String> {
        // `last_activity` of `None` (never active) sorts before any `Some`, i.e. is evicted first.
        let rank = |e: &FileEntry| e.last_activity.unwrap_or(i64::MIN);
        // Tier 1: Missing entries.
        let missing = self
            .map
            .iter()
            .filter(|(_, e)| e.missing)
            .min_by_key(|(_, e)| rank(e))
            .map(|(k, _)| k.clone());
        if missing.is_some() {
            return missing;
        }
        // Tier 2: idle (not open) entries — never an open window.
        self.map
            .iter()
            .filter(|(_, e)| !e.window_open)
            .min_by_key(|(_, e)| rank(e))
            .map(|(k, _)| k.clone())
    }

    /// Remove one entry by its (already-folded) key, keeping every derived structure consistent: the
    /// `dirs` box-set (drop the parent key if this was its last file), the open-window count (if it
    /// was open), and the `structure_rev` (the tracked set changed, so the GUI tile-layout cache must
    /// rebuild). Used by both cap eviction and the 1-hour Missing prune (`remove`).
    fn remove_entry(&mut self, fkey: &str) {
        let Some(entry) = self.map.remove(fkey) else {
            return;
        };
        if entry.window_open {
            self.open_count = (self.open_count - 1).max(0);
        }
        // Recompute the directory-box set membership for this entry's parent: if no OTHER tracked file
        // shares the parent key, the box is gone.
        let dkey = Self::dir_key(&entry.path);
        let still_used = self.map.values().any(|e| Self::dir_key(&e.path) == dkey);
        if !still_used {
            self.dirs.remove(&dkey);
        }
        self.structure_rev = self.structure_rev.wrapping_add(1);
    }

    /// Remove the tracked entry for `path` entirely (the 1-hour Missing prune: the watch service
    /// raised `Removed` after the file was gone for the prune grace). No-op for an unknown path.
    /// Keeps `dirs`, `open_count` and `structure_rev` consistent via [`remove_entry`].
    pub fn remove(&mut self, path: &str) {
        let fkey = Self::key(path);
        if self.map.contains_key(&fkey) {
            self.remove_entry(&fkey);
        }
    }

    /// Get or create the entry for `path`. Returns `None` when the path is DROPPED for the
    /// `MAX_DIR_BOXES` cap (its directory is new and the model is already at the cap) — a public,
    /// honest alias for [`add_or_drop`](Self::add_or_drop). Callers that do not care about the drop
    /// case use `if let Some(e) = ...` (or ignore the result); there is no throwaway sink entry, so a
    /// dropped path can never be mutated or observed by mistake.
    pub fn get_or_add(&mut self, path: &str) -> Option<&mut FileEntry> {
        self.add_or_drop(path)
    }

    pub fn is_open(&self, path: &str) -> bool {
        self.map
            .get(&Self::key(path))
            .map(|e| e.window_open)
            .unwrap_or(false)
    }

    /// Record a write. Returns true if it now WANTS to auto-open (not disabled, not user-closed,
    /// not already open) — the UI then calls [`try_open`](Self::try_open).
    pub fn mark_activity(&mut self, path: &str, now: i64) -> bool {
        let no_open = self.no_open;
        // A dropped path (dir cap) never becomes tracked and never wants to auto-open.
        match self.add_or_drop(path) {
            Some(e) => {
                e.missing = false;
                e.missing_since = None; // present again: reset the prune clock (finding 1 prune)
                e.last_activity = Some(now);
                !no_open && !e.user_closed && !e.window_open
            }
            None => false,
        }
    }

    /// Clear the Missing state on an existing `&mut FileEntry`: both the flag and the 1-hour prune
    /// clock, kept paired. The GUI's Discovered / Reappeared handling calls this on the entry it got
    /// from `get_or_add`, instead of setting `e.missing = false` directly (which would leave
    /// `missing_since` stale and prune a file that is actually present).
    pub fn clear_missing(e: &mut FileEntry) {
        e.missing = false;
        e.missing_since = None;
    }

    /// A file that changed while its window is closed is "unread".
    pub fn mark_unread_if_closed(&mut self, path: &str) {
        // A dropped path (dir cap) is not tracked, so there is nothing to mark unread.
        if let Some(e) = self.add_or_drop(path) {
            if !e.window_open {
                e.unread = true;
            }
        }
    }

    /// Attempt to open a window, enforcing the max-open-windows cap. At the cap NO window opens.
    pub fn try_open(&mut self, path: &str) -> OpenResult {
        let cap = self.max_windows;
        let count = self.open_count;
        // A dropped path (dir cap) has no tile to open from, but guard anyway so it can never open a
        // window or perturb open_count.
        let Some(e) = self.add_or_drop(path) else {
            return OpenResult::BlockedByCap;
        };
        if e.window_open {
            return OpenResult::AlreadyOpen;
        }
        if count >= cap {
            return OpenResult::BlockedByCap;
        }
        e.window_open = true;
        e.user_closed = false;
        e.unread = false;
        self.open_count += 1;
        OpenResult::Opened
    }

    /// The user closed a window. Frees a cap slot; closed stays closed.
    pub fn mark_closed(&mut self, path: &str) {
        if let Some(e) = self.map.get_mut(&Self::key(path)) {
            if e.window_open {
                e.window_open = false;
                e.user_closed = true;
                self.open_count = (self.open_count - 1).max(0);
            }
        }
    }

    pub fn mark_missing(&mut self, path: &str, now: i64) {
        if let Some(e) = self.map.get_mut(&Self::key(path)) {
            e.missing = true;
            // Stamp the start of THIS absence only on the transition into Missing, so a file that
            // stays Missing across many sweeps ages toward the prune from its FIRST missing sweep,
            // not the latest (finding 1 prune). Already-Missing => leave the original stamp.
            if e.missing_since.is_none() {
                e.missing_since = Some(now);
            }
        }
    }

    /// Remove the tiles of files that have been continuously Missing for at least `grace_secs`
    /// (the 1-hour prune, finding 1 growth path). Returns the display paths removed so the GUI can
    /// drop any bookkeeping keyed on them (a tail window is closed by its own reader's `missing`
    /// marker independently). A file seen present again has its `missing_since` cleared, so only files
    /// that are really gone for the whole grace are pruned; this is what keeps a rotating directory
    /// bounded without waiting to hit the `MAX_FILES` cap. The GUI calls this each Tick with its own
    /// wall clock, so the grace is real elapsed time regardless of the poll interval.
    pub fn prune_missing(&mut self, now: i64, grace_secs: i64) -> Vec<String> {
        let doomed: Vec<(String, String)> = self
            .map
            .iter()
            // Never prune an OPEN window's tile out from under the user: they still have it open, and
            // its tail reports the file's own missing/rotated state. Keep it until they close it.
            .filter(|(_, e)| !e.window_open)
            .filter_map(|(k, e)| match e.missing_since {
                Some(since) if now - since >= grace_secs => Some((k.clone(), e.path.clone())),
                _ => None,
            })
            .collect();
        let mut removed = Vec::with_capacity(doomed.len());
        for (fkey, disp) in doomed {
            self.remove_entry(&fkey);
            removed.push(disp);
        }
        removed
    }

    /// Restart: forget everything and re-scan fresh.
    pub fn reset(&mut self) {
        self.map.clear();
        self.dirs.clear();
        self.dir_overflow = false;
        self.file_overflow = false;
        self.open_count = 0;
        // The tracked set went to empty: invalidate the GUI's cached tile layout (finding 1).
        self.structure_rev = self.structure_rev.wrapping_add(1);
    }

    /// Compute the button appearance for an entry at time `now`.
    pub fn vis_entry(&self, e: &FileEntry, now: i64) -> ButtonVis {
        if e.missing {
            return ButtonVis::Missing;
        }
        let active = match e.last_activity {
            Some(t) => (now - t) <= self.active_seconds,
            None => false,
        };
        if e.window_open {
            return if active {
                ButtonVis::ActiveOpen
            } else {
                ButtonVis::Open
            };
        }
        if active {
            return ButtonVis::ActiveClosed;
        }
        if e.unread {
            return ButtonVis::Unread;
        }
        ButtonVis::Idle
    }

    /// Convenience: appearance for a path (creating the entry if needed).
    pub fn vis(&mut self, path: &str, now: i64) -> ButtonVis {
        // get_or_add then compute; clone the small entry to avoid borrow conflict. A path dropped for
        // the dir cap has no entry and renders Idle (it is not shown at all in practice).
        match self.get_or_add(path) {
            Some(e) => {
                let e = e.clone();
                self.vis_entry(&e, now)
            }
            None => ButtonVis::Idle,
        }
    }

    /// Immutable appearance lookup by path — an O(1) `HashMap` get, NO entry creation. Returns
    /// [`ButtonVis::Idle`] for an unknown path. Lets a read-only caller (e.g. the GUI's per-tick
    /// tile repaint) get a tile's color without a `&mut` borrow and without a linear scan of all
    /// entries (the O(n²)-per-tick pattern the GUI used before).
    pub fn vis_of(&self, path: &str, now: i64) -> ButtonVis {
        match self.map.get(&Self::key(path)) {
            Some(e) => self.vis_entry(e, now),
            None => ButtonVis::Idle,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // T0 is an arbitrary epoch-seconds baseline; offsets model the .NET AddSeconds() calls.
    const T0: i64 = 1_752_496_800; // 2026-07-14T12:00:00Z, in seconds.

    // Ported from SessionModelTests.cs (parity oracle — the 8 state tests).

    #[test]
    fn new_file_is_idle_until_activity() {
        let mut m = SessionModel::new();
        let e = m.get_or_add("a.log").unwrap().clone();
        assert_eq!(m.vis_entry(&e, T0), ButtonVis::Idle);
    }

    #[test]
    fn activity_makes_a_closed_file_active_then_decays_to_unread() {
        let mut m = SessionModel {
            active_seconds: 10,
            no_open: true,
            ..Default::default()
        };
        let wants = m.mark_activity("a.log", T0);
        assert!(!wants); // NoOpen suppresses auto-open
        m.mark_unread_if_closed("a.log");
        assert_eq!(m.vis("a.log", T0 + 5), ButtonVis::ActiveClosed); // within timeout
        assert_eq!(m.vis("a.log", T0 + 11), ButtonVis::Unread); // decayed
    }

    #[test]
    fn open_window_is_active_open_while_writing_then_plain_open() {
        let mut m = SessionModel {
            active_seconds: 10,
            ..Default::default()
        };
        assert!(m.mark_activity("a.log", T0));
        assert_eq!(m.try_open("a.log"), OpenResult::Opened);
        assert_eq!(m.vis("a.log", T0 + 3), ButtonVis::ActiveOpen);
        assert_eq!(m.vis("a.log", T0 + 20), ButtonVis::Open);
    }

    #[test]
    fn cap_blocks_further_opens_until_a_slot_frees() {
        let mut m = SessionModel {
            max_windows: 2,
            ..Default::default()
        };
        assert_eq!(m.try_open("a.log"), OpenResult::Opened);
        assert_eq!(m.try_open("b.log"), OpenResult::Opened);
        assert_eq!(m.try_open("c.log"), OpenResult::BlockedByCap);
        assert_eq!(m.open_count(), 2);
        m.mark_closed("a.log");
        assert_eq!(m.open_count(), 1);
        assert_eq!(m.try_open("c.log"), OpenResult::Opened); // slot freed
    }

    #[test]
    fn closed_stays_closed_no_auto_reopen() {
        let mut m = SessionModel::new();
        assert_eq!(m.try_open("a.log"), OpenResult::Opened);
        m.mark_closed("a.log");
        assert!(!m.mark_activity("a.log", T0)); // does not want to auto-open after user closed it
    }

    #[test]
    fn missing_overrides_everything() {
        let mut m = SessionModel::new();
        m.try_open("a.log");
        m.mark_activity("a.log", T0);
        m.mark_missing("a.log", T0);
        assert_eq!(m.vis("a.log", T0), ButtonVis::Missing);
    }

    #[test]
    fn reopen_clears_unread() {
        let mut m = SessionModel {
            no_open: true,
            ..Default::default()
        };
        m.mark_activity("a.log", T0);
        m.mark_unread_if_closed("a.log");
        assert_eq!(m.vis("a.log", T0 + 30), ButtonVis::Unread);
        assert_eq!(m.try_open("a.log"), OpenResult::Opened);
        assert_eq!(m.vis("a.log", T0 + 30), ButtonVis::Open);
    }

    #[test]
    fn vis_of_matches_vis_without_creating_entries() {
        // vis_of is the immutable O(1) lookup the GUI uses per tick; it must agree with vis() for a
        // known entry and NOT create an entry for an unknown path (unlike vis()).
        let mut m = SessionModel {
            active_seconds: 10,
            ..Default::default()
        };
        m.mark_activity("a.log", T0);
        assert_eq!(m.vis_of("a.log", T0 + 5), m.vis("a.log", T0 + 5));
        // Unknown path: Idle, and the read must not have added an entry.
        assert_eq!(m.vis_of("ghost.log", T0), ButtonVis::Idle);
        assert!(m.entries().all(|e| e.path != "ghost.log"));
    }

    #[test]
    fn reset_clears_entries_and_open_count() {
        let mut m = SessionModel::new();
        m.try_open("a.log");
        m.reset();
        assert_eq!(m.open_count(), 0);
        assert_eq!(m.entry_count(), 0);
    }

    #[test]
    fn structure_rev_bumps_on_add_and_reset_but_not_on_state_change() {
        // The GUI's cached tile layout (finding 1) rebuilds only when this token changes. It MUST
        // change when the set of tracked paths changes (a new tile, or all cleared) and MUST NOT
        // change when only an entry's state changes (colour changes, but not which tiles/order).
        let mut m = SessionModel::new();
        let r0 = m.structure_rev();

        // A brand-new entry: bumps.
        m.get_or_add("/w/a.log");
        let r1 = m.structure_rev();
        assert_ne!(r0, r1, "adding a new entry must bump the structure rev");

        // The SAME path again: no new entry, no bump.
        m.get_or_add("/w/a.log");
        assert_eq!(
            r1,
            m.structure_rev(),
            "re-adding a known path must not bump"
        );

        // Per-entry state changes: activity, unread, missing, open/close — colour, not structure.
        m.mark_activity("/w/a.log", 100);
        m.mark_unread_if_closed("/w/a.log");
        m.mark_missing("/w/a.log", 100);
        let _ = m.try_open("/w/a.log");
        m.mark_closed("/w/a.log");
        assert_eq!(
            r1,
            m.structure_rev(),
            "per-entry state changes must not bump the structure rev"
        );

        // A second new entry: bumps again.
        m.get_or_add("/w/b.log");
        let r2 = m.structure_rev();
        assert_ne!(r1, r2, "a second new entry must bump");

        // reset() empties the set: bumps.
        m.reset();
        assert_ne!(r2, m.structure_rev(), "reset must bump the structure rev");
    }

    // ---- MAX_DIR_BOXES directory-box cap (R122) ----

    #[test]
    fn dir_cap_counts_boxes_not_files_and_admits_many_files_per_dir() {
        // Many files in a FEW directories must never trip the cap — the cap is on distinct
        // directories (boxes), not files.
        let mut m = SessionModel::new();
        for i in 0..(MAX_DIR_BOXES as i32 * 3) {
            m.get_or_add(&format!("/w/logs/f{i}.log"));
        }
        m.get_or_add("/w/other/x.log");
        assert_eq!(
            m.dir_count(),
            2,
            "two directories, regardless of file count"
        );
        assert!(!m.dir_overflow());
        assert_eq!(m.entry_count(), MAX_DIR_BOXES * 3 + 1);
    }

    #[test]
    fn dir_cap_admits_exactly_max_dir_boxes_then_drops_new_dirs() {
        let mut m = SessionModel::new();
        // One file each in MAX_DIR_BOXES distinct directories — all admitted.
        for i in 0..MAX_DIR_BOXES {
            m.get_or_add(&format!("/w/d{i}/a.log"));
        }
        assert_eq!(m.dir_count(), MAX_DIR_BOXES);
        assert_eq!(m.entry_count(), MAX_DIR_BOXES);
        assert!(!m.dir_overflow(), "at the cap, not over it");

        // A file in a BRAND-NEW directory is dropped and latches overflow.
        m.get_or_add("/w/overflow/z.log");
        assert!(m.dir_overflow());
        assert_eq!(m.dir_count(), MAX_DIR_BOXES, "no new box added");
        assert_eq!(
            m.entry_count(),
            MAX_DIR_BOXES,
            "the dropped file is not tracked"
        );
        assert!(
            m.entries().all(|e| e.path != "/w/overflow/z.log"),
            "dropped file never appears in entries",
        );

        // But another file in an ALREADY-KNOWN directory is still admitted past the cap.
        m.get_or_add("/w/d0/b.log");
        assert_eq!(m.dir_count(), MAX_DIR_BOXES);
        assert_eq!(m.entry_count(), MAX_DIR_BOXES + 1);
        assert!(m.entries().any(|e| e.path == "/w/d0/b.log"));
    }

    #[test]
    fn dropped_file_is_an_inert_noop_through_the_mutating_entry_points() {
        // Fill to the cap, then a dropped file must not create state via mark_activity /
        // mark_unread_if_closed / vis (all route through get_or_add) — no entry, no open-count churn.
        let mut m = SessionModel::new();
        for i in 0..MAX_DIR_BOXES {
            m.get_or_add(&format!("/w/d{i}/a.log"));
        }
        let ghost = "/w/nope/ghost.log";
        assert!(
            !m.mark_activity(ghost, T0),
            "dropped file wants no auto-open"
        );
        m.mark_unread_if_closed(ghost);
        assert_eq!(m.vis(ghost, T0), ButtonVis::Idle);
        assert!(m.entries().all(|e| e.path != ghost));
        assert_eq!(m.dir_count(), MAX_DIR_BOXES);
    }

    #[test]
    fn reset_clears_dir_cap_state() {
        let mut m = SessionModel::new();
        for i in 0..MAX_DIR_BOXES {
            m.get_or_add(&format!("/w/d{i}/a.log"));
        }
        m.get_or_add("/w/overflow/z.log"); // trips it
        assert!(m.dir_overflow());
        m.reset();
        assert!(!m.dir_overflow(), "Restart clears the overflow latch");
        assert_eq!(m.dir_count(), 0, "Restart clears the directory set");
        // ...and a fresh, small watch is admitted normally afterward.
        m.get_or_add("/w/fresh/a.log");
        assert_eq!(m.dir_count(), 1);
    }

    // review #3 finding 6 (R124): the dir-cap drop path is a type-level None (no throwaway sink).
    #[test]
    fn get_or_add_returns_none_for_a_dropped_path_no_sink() {
        // The dir-cap drop path is a type-level None now (no throwaway `overflow_sink`): a dropped
        // path can never hand back a mutable entry to corrupt by mistake.
        let mut m = SessionModel::new();
        for i in 0..MAX_DIR_BOXES {
            m.get_or_add(&format!("/w/d{i}/a.log"));
        }
        // A file in an ALREADY-KNOWN directory is still Some past the cap...
        assert!(m.get_or_add("/w/d0/second.log").is_some());
        // ...but a file in a BRAND-NEW directory over the cap is None (dropped, no sink).
        assert!(m.get_or_add("/w/brand_new/z.log").is_none());
        assert!(m.dir_overflow());
        assert!(m.entries().all(|e| e.path != "/w/brand_new/z.log"));
    }

    // ---- MAX_FILES cap with keep-most-active eviction (finding 1) ----

    #[test]
    fn file_cap_evicts_the_oldest_idle_tile_to_admit_a_new_file() {
        // Fill the model to MAX_FILES with idle files, each with a distinct last_activity so there is a
        // clear "oldest". All in ONE directory so the dir cap never trips first.
        let mut m = SessionModel::new();
        for i in 0..MAX_FILES {
            let p = format!("/w/f{i:05}.log");
            m.get_or_add(&p);
            m.mark_activity(&p, T0 + i as i64); // f00000 oldest, f00999 newest
        }
        assert_eq!(m.entry_count(), MAX_FILES);
        // A new file arrives: the OLDEST idle tile (f00000) is evicted, the newcomer admitted.
        assert!(m.get_or_add("/w/new.log").is_some());
        assert_eq!(m.entry_count(), MAX_FILES, "still capped, not grown");
        assert!(
            m.entries().all(|e| e.path != "/w/f00000.log"),
            "the oldest idle tile was evicted"
        );
        assert!(
            m.entries().any(|e| e.path == "/w/new.log"),
            "the new file was admitted"
        );
        assert!(!m.file_overflow(), "an eviction is not an overflow");
    }

    #[test]
    fn file_cap_evicts_a_missing_tile_before_any_idle_one() {
        let mut m = SessionModel::new();
        for i in 0..MAX_FILES {
            let p = format!("/w/f{i:05}.log");
            m.get_or_add(&p);
            m.mark_activity(&p, T0 + 1000 + i as i64); // all fairly recent
        }
        // Make ONE recent file Missing — it must be evicted before any (older) idle file.
        m.mark_missing("/w/f00999.log", T0 + 2000);
        assert!(m.get_or_add("/w/new.log").is_some());
        assert!(
            m.entries().all(|e| e.path != "/w/f00999.log"),
            "the Missing tile is evicted first, even though it was recently active"
        );
        assert!(
            m.entries().any(|e| e.path == "/w/f00000.log"),
            "an older but present idle tile is kept"
        );
    }

    #[test]
    fn file_cap_never_evicts_an_open_window_and_refuses_when_all_open() {
        // A small model where every entry is an open window: a new file cannot evict any of them, so
        // it is refused and file_overflow latches. (Use try_open to open; max_windows default is 10.)
        let mut m = SessionModel::new();
        m.max_windows = MAX_FILES as i32 + 5; // don't let the window cap interfere
        for i in 0..MAX_FILES {
            let p = format!("/w/f{i:05}.log");
            assert_eq!(m.try_open(&p), OpenResult::Opened);
        }
        assert_eq!(m.open_count(), MAX_FILES as i32);
        // Every tile is an open window: nothing safe to evict.
        assert!(
            m.get_or_add("/w/new.log").is_none(),
            "refused: nothing safe to evict"
        );
        assert!(m.file_overflow(), "refusal latches file_overflow");
        assert_eq!(m.entry_count(), MAX_FILES, "not grown, not shrunk");
        // reset clears the latch.
        m.reset();
        assert!(!m.file_overflow());
    }

    #[test]
    fn file_cap_prefers_evicting_an_idle_tile_over_an_open_one() {
        let mut m = SessionModel::new();
        m.max_windows = MAX_FILES as i32 + 5;
        // Fill: all but one are OPEN (recent); one is idle and old.
        for i in 0..(MAX_FILES - 1) {
            let p = format!("/w/open{i:05}.log");
            assert_eq!(m.try_open(&p), OpenResult::Opened);
            m.mark_activity(&p, T0 + 5000 + i as i64);
        }
        m.get_or_add("/w/idle.log");
        m.mark_activity("/w/idle.log", T0); // oldest, and NOT open
        assert_eq!(m.entry_count(), MAX_FILES);
        // The newcomer must evict the idle one, never one of the open windows.
        assert!(m.get_or_add("/w/new.log").is_some());
        assert!(m.entries().all(|e| e.path != "/w/idle.log"), "idle evicted");
        assert_eq!(
            m.open_count(),
            (MAX_FILES - 1) as i32,
            "no open window was evicted"
        );
        assert!(!m.file_overflow());
    }

    // ---- 1-hour Missing prune (finding 1 growth path) ----

    #[test]
    fn prune_missing_removes_only_files_gone_the_whole_grace() {
        const HOUR: i64 = 3600;
        let mut m = SessionModel::new();
        m.get_or_add("/w/a.log");
        m.get_or_add("/w/b.log");
        m.mark_activity("/w/a.log", T0);
        m.mark_activity("/w/b.log", T0);
        // Both go Missing at T0.
        m.mark_missing("/w/a.log", T0);
        m.mark_missing("/w/b.log", T0);
        // Half an hour later: nothing pruned yet.
        assert!(m.prune_missing(T0 + HOUR / 2, HOUR).is_empty());
        assert_eq!(m.entry_count(), 2);
        // b.log reappears just before the hour; a.log stays gone. (mark_activity clears missing_since.)
        let _ = m.mark_activity("/w/b.log", T0 + HOUR - 10);
        // At the full hour: only a.log (continuously gone) is pruned; b.log survived (clock reset).
        let removed = m.prune_missing(T0 + HOUR, HOUR);
        assert_eq!(removed, vec!["/w/a.log".to_string()]);
        assert_eq!(m.entry_count(), 1);
        assert!(m.entries().any(|e| e.path == "/w/b.log"));
    }

    #[test]
    fn prune_missing_leaves_an_open_window_alone() {
        const HOUR: i64 = 3600;
        let mut m = SessionModel::new();
        assert_eq!(m.try_open("/w/a.log"), OpenResult::Opened);
        m.mark_missing("/w/a.log", T0);
        // Even long past the grace, an OPEN tile is not pruned out from under the window.
        let removed = m.prune_missing(T0 + HOUR * 3, HOUR);
        assert!(removed.is_empty());
        assert_eq!(m.entry_count(), 1);
    }

    #[test]
    fn prune_missing_bumps_structure_rev_when_it_removes() {
        const HOUR: i64 = 3600;
        let mut m = SessionModel::new();
        m.get_or_add("/w/a.log");
        m.mark_missing("/w/a.log", T0);
        let r = m.structure_rev();
        // No removal yet: rev unchanged.
        let _ = m.prune_missing(T0 + 1, HOUR);
        assert_eq!(r, m.structure_rev());
        // Removal: rev bumps so the GUI tile-layout cache rebuilds.
        let _ = m.prune_missing(T0 + HOUR, HOUR);
        assert_ne!(r, m.structure_rev());
        assert_eq!(m.entry_count(), 0);
    }

    #[test]
    fn remove_entry_keeps_the_dir_box_set_consistent() {
        let mut m = SessionModel::new();
        m.get_or_add("/w/one/a.log");
        m.get_or_add("/w/one/b.log");
        m.get_or_add("/w/two/c.log");
        assert_eq!(m.dir_count(), 2);
        // Removing one of two files in /w/one keeps the box (b.log still there).
        m.remove("/w/one/a.log");
        assert_eq!(m.dir_count(), 2, "box stays while another file uses it");
        // Removing the last file in /w/two drops that box.
        m.remove("/w/two/c.log");
        assert_eq!(m.dir_count(), 1, "box gone when its last file is removed");
    }
}
