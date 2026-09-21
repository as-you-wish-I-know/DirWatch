//! Cross-platform *watch runtime*: the scheduling layer that drives
//! [`WatchService::sweep`](dirwatch_core::watch::WatchService::sweep) from a background thread and
//! streams the resulting events to the GUI thread over a channel (DECISIONS R8 — the layer the
//! core's sweep was designed to sit under).
//!
//! WHY A THREAD + CHANNEL: the GUI (iced) owns the `SessionModel` on its own thread, and iced's
//! update/view must never block. The filesystem work — the `notify` watcher, the poll timer, and
//! the `WatchService` — runs OFF the GUI thread so a slow network-share sweep never freezes the UI.
//! The only thing crossing the boundary is a stream of [`WatchEvent`] path messages; the GUI drains
//! them (on its tick subscription) and mutates the model. This mirrors the .NET design where
//! `WatchService` raised events the UI marshaled onto its dispatcher, and is exactly the shape
//! iced's message loop wants (PORT-PLAN-crossplatform §3).
//!
//! SWEEP IS STILL AUTHORITY: `notify` only nudges the [`SweepScheduler`]; every state decision is
//! made by `WatchService::sweep`. The periodic poll runs regardless of `notify`, which is exactly
//! why network/UNC shares work when the OS watcher (inotify/FSEvents/ReadDirectoryChangesW) misses
//! events (DECISIONS entry 10). The poll is never dropped (BACKLOG §2 item 4 stays an accepted gap,
//! not a regression here).
//!
//! PORTABILITY: this module is now platform-agnostic — `notify` is cross-platform, so the thread +
//! watcher + channel wiring links and RUNS on Windows, macOS, and Linux. Its scheduling *policy*
//! is the platform-agnostic [`SweepScheduler`] in `dirwatch-core`, unit-tested with a mock clock;
//! the OS wiring here (thread, `notify` watcher, channel) is exercised end-to-end by this module's
//! own integration tests, which now RUN on every host (previously Windows-only under `cfg`).

use dirwatch_core::glob::GlobMatcher;
use dirwatch_core::schedule::SweepScheduler;
use dirwatch_core::watch::{SweepStats, WatchService};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, SyncSender};
use std::time::Instant;

/// How many undelivered [`TailChunk`]s a tail reader may have in flight before it BLOCKS instead of
/// buffering (review #6 finding 3, DECISIONS R142). The reader used to send on an unbounded channel
/// and loop on `more` as fast as the disk allowed, so whenever `update` was blocked — the Browse
/// dialog, any modal — a busy log queued itself whole in memory (250 MB in 3 s measured) and then
/// landed in ONE tick. At 4 MB per piece this is 32 MB of back-pressure: the reader parks on `send`
/// (the bytes stay in the file; nothing is lost), wakes when the GUI drains, and still exits promptly
/// on a dropped receiver because `send` returns `Err` then. A tick therefore appends at most this
/// many pieces.
pub const TAIL_CHANNEL_CHUNKS: usize = 8;

/// How many undelivered [`WatchEvent`]s the watch runtime may queue before it COALESCES instead of
/// buffering (REVIEW-2026-09-17 finding 7). The tail pipe was bounded at R142; this event pipe was
/// still `mpsc::channel` (unbounded). While `update` is blocked (Browse's modal `rfd::pick_folder`,
/// R142's own scenario) the runtime keeps sweeping every poll and pushing one event per changed
/// file: a 1000-file dir under continuous append ≈ 4000 events/s, so a dialog left open for minutes
/// buffered hundreds of MB, applied in one tick. Bounded here: a full channel makes the emit
/// callback SKIP its dispatch — the sweep is authority, so the NEXT sweep re-derives current state
/// from the on-disk stamps and nothing is lost (unlike the tail reader, which back-pressures because
/// its bytes are the payload; here the payload is a stamp the next sweep re-reads). 1024 ≈ one sweep
/// of a 1000-file dir, so a normally-drained tick never coalesces — only a stuck GUI does.
/// Reproduced before the fix (a scratch probe queued 10000 on the unbounded channel; this caps it).
pub const WATCH_EVENT_CHANNEL_CAP: usize = 1024;

/// The tail reader's floor poll period in ms. The reader polls at the WATCH POLL INTERVAL (review #6
/// finding 8, DECISIONS R144 — a `--poll-ms 5000` chosen for a share used to leave every open tail
/// stat-ing at a fixed 100 ms), never faster than this.
pub const TAIL_POLL_FLOOR_MS: u64 = dirwatch_core::POLL_INTERVAL_FLOOR_MS as u64;

/// One event raised by the watch runtime, addressed by full file path. The GUI thread turns these
/// into [`SessionModel`](dirwatch_core::session::SessionModel) mutations.
#[derive(Debug, Clone)]
pub enum WatchEvent {
    Discovered(PathBuf),
    Activity(PathBuf),
    Missing(PathBuf),
    Reappeared(PathBuf),
    /// The watched directory could not be read (missing, permission denied, share gone). Raised
    /// once per failure episode by the core (review B2, DECISIONS R97); the GUI shows it in the
    /// status strip instead of a silent "no matching files yet".
    Error(String),
    /// The directory reads again after an `Error` episode (once per recovery; review #2 finding
    /// 2, DECISIONS R111) — the GUI clears the error note.
    Recovered,
}

/// Immutable knobs for one watch session (from CLI or the GUI's Start button).
#[derive(Debug, Clone)]
pub struct WatchConfig {
    pub directory: PathBuf,
    pub patterns: Vec<String>,
    pub poll_interval_ms: u32,
    pub depth: i32,
}

/// A running watch: the background thread plus the receiving end of its event channel. Dropping
/// the handle signals the thread to stop; the thread exits on its own within one tick. Restarting
/// is just replacing the handle.
///
/// NO JOIN ON DROP (review B1, DECISIONS R97): `Drop` used to `join()` the thread. Every drop
/// happens on the GUI thread (Stop, Restart, a Settings poll change, exit), and the thread may be
/// mid-`sweep()` — on a network share that is seconds, on a dead share ~30 s — so the GUI froze for
/// exactly the work the thread exists to keep off it. Now the drop is ~0 ms: set the flag, drop the
/// handle, let the thread finish its current step and exit (its sends fail harmlessly once `rx` is
/// gone). `stop_wakeup` lets the thread's sleep end early so it exits promptly rather than after
/// its full sleep.
pub struct WatchRuntime {
    rx: Receiver<WatchEvent>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Sending (or dropping) this wakes the thread's sleep so it sees `stop` immediately.
    stop_wakeup: Option<Sender<()>>,
    /// The live sweep cadence in ms (finding 6). A Settings poll change stores into this instead of
    /// respawning the thread; the thread reads it each loop iteration. Shared with the thread.
    poll_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl WatchRuntime {
    /// Spawn the watch thread for `cfg`. Events arrive on [`try_drain`](Self::try_drain).
    pub fn start(cfg: WatchConfig) -> Self {
        // Bounded event pipe (finding 7): a full channel coalesces rather than buffering; the next
        // sweep re-derives state. See WATCH_EVENT_CHANNEL_CAP.
        let (tx, rx) = std::sync::mpsc::sync_channel::<WatchEvent>(WATCH_EVENT_CHANNEL_CAP);
        let (wake_tx, wake_rx) = std::sync::mpsc::channel::<()>();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_thread = stop.clone();
        // Live poll interval (finding 6): a Settings poll change updates this atomic in place, so the
        // runtime is NEVER torn down for a cadence change (mirrors the tail reader's R144 pattern).
        // Seeded from cfg; clamped to the shared floor to match SweepScheduler/WatchService.
        let poll_ms = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
            (cfg.poll_interval_ms as u64).max(dirwatch_core::POLL_INTERVAL_FLOOR_MS as u64),
        ));
        let poll_thread = poll_ms.clone();
        std::thread::spawn(move || run_thread(cfg, tx, stop_thread, wake_rx, poll_thread));
        WatchRuntime {
            rx,
            stop,
            stop_wakeup: Some(wake_tx),
            poll_ms,
        }
    }

    /// The live poll interval this runtime sweeps at (ms). Reflects the last [`set_poll_ms`](Self::set_poll_ms).
    /// TEST ONLY: production sets the cadence (from `settings_ok`) but never reads it back — the thread
    /// owns its own copy of the atomic — so this accessor exists only for the runtime's own tests
    /// (matching `from_receiver` below). Shipping it `pub` would be dead code under `-D warnings`.
    #[cfg(test)]
    pub(crate) fn poll_ms(&self) -> u64 {
        self.poll_ms.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Change the sweep cadence of a RUNNING watch without restarting it (finding 6). Clamped to the
    /// shared floor. The loop reads this atomic each iteration and re-anchors the scheduler, so no
    /// event is dropped and no discovery-only re-scan runs (which a runtime swap would cause).
    pub fn set_poll_ms(&self, ms: u64) {
        self.poll_ms.store(
            ms.max(dirwatch_core::POLL_INTERVAL_FLOOR_MS as u64),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// TEST ONLY: a runtime fed by a channel the test owns (no thread, no directory), so GUI tests
    /// can drive `update(Tick)` with an exact event sequence.
    #[cfg(test)]
    pub(crate) fn from_receiver(rx: Receiver<WatchEvent>) -> Self {
        WatchRuntime {
            rx,
            stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            stop_wakeup: None,
            poll_ms: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(TAIL_POLL_FLOOR_MS)),
        }
    }

    /// Pull all events queued since the last call (non-blocking). The GUI drains this on its timer.
    pub fn try_drain(&self) -> Vec<WatchEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = self.rx.try_recv() {
            out.push(ev);
        }
        out
    }
}

impl Drop for WatchRuntime {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        // Dropping the wake sender makes the thread's `recv_timeout` return at once (Disconnected),
        // so it observes `stop` now instead of after its sleep. No join (review B1).
        self.stop_wakeup.take();
    }
}

/// The background thread body: build the `WatchService`, wire callbacks to the channel, attach a
/// `notify` watcher that nudges the scheduler, and loop poll→sweep until asked to stop.
fn run_thread(
    cfg: WatchConfig,
    tx: SyncSender<WatchEvent>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    wake: Receiver<()>,
    poll_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
) {
    use notify::{RecursiveMode, Watcher};

    let matcher = GlobMatcher::new(if cfg.patterns.is_empty() {
        None
    } else {
        Some(&cfg.patterns)
    });
    let mut svc = WatchService::new(&cfg.directory, matcher, cfg.poll_interval_ms, cfg.depth);

    // Wire each core callback to a BOUNDED channel send (finding 7). A closed channel (GUI gone)
    // drops the send harmlessly; a FULL channel COALESCES — try_send fails with `Full`, the callback
    // skips this dispatch, and the next sweep re-derives current state from the on-disk stamps (the
    // sweep is authority). So an event is only ever "lost" while the GUI is stuck not draining, and
    // it is re-created on the next drained sweep. Both Disconnected and Full are just `Err` here.
    let t = tx.clone();
    svc.on_discovered(move |p| {
        let _ = t.try_send(WatchEvent::Discovered(p.to_path_buf()));
    });
    let t = tx.clone();
    svc.on_activity(move |p| {
        let _ = t.try_send(WatchEvent::Activity(p.to_path_buf()));
    });
    let t = tx.clone();
    svc.on_missing(move |p| {
        let _ = t.try_send(WatchEvent::Missing(p.to_path_buf()));
    });
    let t = tx.clone();
    svc.on_reappeared(move |p| {
        let _ = t.try_send(WatchEvent::Reappeared(p.to_path_buf()));
    });
    let t = tx.clone();
    svc.on_error(move |m| {
        let _ = t.try_send(WatchEvent::Error(m.to_string()));
    });
    let t = tx.clone();
    svc.on_recovered(move || {
        let _ = t.try_send(WatchEvent::Recovered);
    });

    // notify: a raw (non-debounced) watcher. Every OS event just flips a flag the loop reads and
    // hands to the scheduler as a debounced nudge; notify NEVER decides state. If the watch can't
    // be established (e.g. path vanished), we carry on — the periodic poll still reconciles.
    let fs_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let fs_flag_cb = fs_flag.clone();
    let mut watcher: Option<notify::RecommendedWatcher> =
        match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if res.is_ok() {
                fs_flag_cb.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }) {
            Ok(mut w) => {
                // notify is only a nudge; the poll sweep is authority and honours `--depth`, so the
                // watcher never needs to see the whole subtree. On Linux, `RecursiveMode::Recursive`
                // means one `inotify_add_watch` per directory in the ENTIRE tree, ignoring `--depth`
                // (REVIEW-2026-09-17 finding 2): a `--depth 1` watch of 151 dirs installed 30 151
                // inotify watches, and a 66 000-dir tree silently exhausted the per-user inotify
                // budget (the error was swallowed by `let _`), so every other program on the box then
                // failed to watch anything. inotify is also the only OS watcher with a per-directory
                // watch and a small per-user budget; FSEvents (macOS) and ReadDirectoryChangesW
                // (Windows) are one cheap handle for the whole subtree, so recursion stays there.
                let mode = if cfg!(target_os = "linux") {
                    RecursiveMode::NonRecursive
                } else if cfg.depth > 0 {
                    RecursiveMode::Recursive
                } else {
                    RecursiveMode::NonRecursive
                };
                // Log the failure instead of dropping it (finding 2): a watch that cannot be
                // established (budget exhausted, path vanished) leaves the poll as the only reconciler,
                // which is fine but must not be silent.
                if let Err(e) = w.watch(&cfg.directory, mode) {
                    crate::gui::err_log(&format!(
                        "notify watch of {} failed ({e}); relying on the periodic poll",
                        cfg.directory.display()
                    ));
                }
                Some(w)
            }
            Err(_) => None,
        };

    let start = Instant::now();
    let now_ms = || start.elapsed().as_millis() as u64;

    let mut sched = SweepScheduler::new(cfg.poll_interval_ms as u64, now_ms());
    // Per-sweep cost trace (review #6, DECISIONS R140), under DIRWATCH_DEBUG only: the first sweep,
    // any sweep whose file/dir counts changed, any sweep slower than half the poll interval, and a
    // heartbeat every `SWEEP_TRACE_EVERY` sweeps — so a hardware artifacts zip carries the walk's
    // real cost (finding 1's Windows number) without one line per sweep.
    const SWEEP_TRACE_EVERY: u64 = 200;
    let mut sweeps: u64 = 0;
    let mut last_traced: Option<SweepStats> = None;
    let slow_ms = (cfg.poll_interval_ms as u64 / 2).max(1);
    let trace_sweep = |svc: &WatchService, sweeps: u64, last: &mut Option<SweepStats>| {
        let st = svc.last_sweep();
        let counts_changed = last
            .map(|l| (l.files, l.dirs) != (st.files, st.dirs))
            .unwrap_or(true);
        if counts_changed || st.millis >= slow_ms || sweeps.is_multiple_of(SWEEP_TRACE_EVERY) {
            crate::gui::trace_log(&format!(
                "sweep #{sweeps}: files={} dirs={} took={}ms (poll {} ms)",
                st.files, st.dirs, st.millis, cfg.poll_interval_ms
            ));
            *last = Some(st);
        }
    };
    // The initial (discovery-only) scan runs on the scheduler's FIRST `poll`, which is due
    // immediately. It used to run here via `svc.start()` AND again on that first poll — two full
    // walks of the tree at every (re)start, doubling the first-paint I/O on a big root (review #4
    // finding 11, DECISIONS R128). Now exactly one: `start()` on the first due poll, `sweep()` after.

    // The loop ticks on a short cadence, sleeping until the next scheduled sweep so we don't busy
    // spin. A notify event shortens the wait via the debounced nudge.
    let mut cur_poll =
        (cfg.poll_interval_ms as u64).max(dirwatch_core::POLL_INTERVAL_FLOOR_MS as u64);
    loop {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        // Finding 6: apply a live Settings poll change in place — no runtime restart, so no dropped
        // events and no discovery-only re-scan. The atomic is the single source; the scheduler
        // re-anchors its next deadline from now.
        let want_poll = poll_ms.load(std::sync::atomic::Ordering::Relaxed);
        if want_poll != cur_poll {
            sched.set_poll_interval_ms(want_poll, now_ms());
            cur_poll = want_poll;
        }
        if fs_flag.swap(false, std::sync::atomic::Ordering::Relaxed) {
            sched.note_fs_event(now_ms());
        }
        let first = !sched.has_started();
        if sched.poll(now_ms()) {
            if first {
                svc.start();
            } else {
                svc.sweep();
            }
            sweeps += 1;
            trace_sweep(&svc, sweeps, &mut last_traced);
        }
        // Sleep until the next sweep is due, capped so we stay responsive to the fs flag. A stop
        // (the handle dropped) ends the wait immediately via the wake channel (review B1).
        let wait = sched.ms_until_next(now_ms()).clamp(10, 100);
        if let Err(std::sync::mpsc::RecvTimeoutError::Disconnected) =
            wake.recv_timeout(std::time::Duration::from_millis(wait))
        {
            break;
        }
    }

    // Explicitly drop the watcher before the thread exits (documents the ordering; not required).
    watcher.take();
}

// ---------------------------------------------------------------------------------------------
// Tail streaming: read ONE file on a background thread so a large file never blocks the GUI.
// ---------------------------------------------------------------------------------------------

/// One chunk of tail output for an open tail window, streamed from its reader thread. Mirrors the
/// fields of the core [`TailResult`](dirwatch_core::tail::TailResult) that the GUI cares about.
#[derive(Debug, Clone, Default)]
pub struct TailChunk {
    pub new_text: Option<String>,
    pub rotated: bool,
    pub reappeared: bool,
    pub missing: bool,
    pub encoding_label: String,
    /// Edge-triggered open/read error text (review B2): set on the FIRST poll that fails, cleared
    /// when a poll succeeds again — so a locked file shows one marker, not one per poll.
    pub error: Option<String>,
    /// The reader began this file part-way in because the existing history exceeded the initial
    /// load bound: this many leading bytes were skipped (review #2 finding 5). Reported once.
    pub skipped_bytes: Option<u64>,
    /// The reader's first look at a present file — sent even when there was nothing to decode, so
    /// an EMPTY file leaves "loading..." (review #2 finding 4) and shows its encoding label.
    pub ready: bool,
}

/// A running tail: a background thread owns the file's [`TailReader`] and polls it, sending
/// [`TailChunk`]s over a channel. WHY A THREAD: the FIRST read of a large file (the whole existing
/// history) can take seconds to read+decode; doing that on the GUI thread froze every window
/// (the user's .i8 report - a 10 MB syncthing log). Here the window opens instantly and the content
/// streams in over subsequent ticks, and no read ever blocks the GUI. Dropping the handle stops
/// the thread WITHOUT joining it (review B1, DECISIONS R97 — same discipline as [`WatchRuntime`]):
/// a join would block the GUI for the whole in-flight read (measured 315 ms for a 300 MB file on
/// local NVMe; seconds on a share) every time a window is closed while it is still loading.
pub struct TailStream {
    rx: Receiver<TailChunk>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    stop_wakeup: Option<Sender<()>>,
    /// The reader's poll period in ms, shared with its thread so a Settings poll change applies to
    /// an OPEN tail live (R144) — the reader is never restarted (that would re-deliver the file).
    poll_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl TailStream {
    /// Spawn the reader thread for `path`. It polls the file every `poll_ms` (floored at
    /// [`TAIL_POLL_FLOOR_MS`]) and sends any output over a channel bounded at
    /// [`TAIL_CHANNEL_CHUNKS`] pieces (R142).
    pub fn start(path: PathBuf, poll_ms: u64) -> Self {
        let poll_ms = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
            poll_ms.max(TAIL_POLL_FLOOR_MS),
        ));
        let (tx, rx) = std::sync::mpsc::sync_channel::<TailChunk>(TAIL_CHANNEL_CHUNKS);
        let (wake_tx, wake_rx) = std::sync::mpsc::channel::<()>();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_thread = stop.clone();
        let poll_thread = poll_ms.clone();
        std::thread::spawn(move || tail_thread(path, poll_thread, tx, stop_thread, wake_rx));
        TailStream {
            rx,
            stop,
            stop_wakeup: Some(wake_tx),
            poll_ms,
        }
    }

    /// The poll period (ms) this stream runs at.
    pub fn poll_ms(&self) -> u64 {
        self.poll_ms.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Change the poll period of a RUNNING stream (a Settings poll change, R144); takes effect
    /// from the reader's next sleep. Floored at [`TAIL_POLL_FLOOR_MS`].
    pub fn set_poll_ms(&self, ms: u64) {
        self.poll_ms.store(
            ms.max(TAIL_POLL_FLOOR_MS),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// TEST ONLY: a stream fed by a channel the test owns (no thread, no file), so GUI tests can
    /// drive `update(Tick)` with an exact chunk sequence (review #4 test-quality note).
    #[cfg(test)]
    pub(crate) fn from_receiver(rx: Receiver<TailChunk>) -> Self {
        TailStream {
            rx,
            stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            stop_wakeup: None,
            poll_ms: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(TAIL_POLL_FLOOR_MS)),
        }
    }

    /// Pull all chunks queued since the last call (non-blocking). The GUI drains this on its tick.
    /// Bounded by construction: at most [`TAIL_CHANNEL_CHUNKS`] pieces can be waiting (R142).
    pub fn try_drain(&self) -> Vec<TailChunk> {
        let mut out = Vec::new();
        while let Ok(c) = self.rx.try_recv() {
            out.push(c);
        }
        out
    }
}

impl Drop for TailStream {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.stop_wakeup.take(); // wake the sleep; no join (review B1)
    }
}

fn tail_thread(
    path: PathBuf,
    poll_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
    tx: SyncSender<TailChunk>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    wake: Receiver<()>,
) {
    use dirwatch_core::tail::TailReader;
    let mut reader = TailReader::new(&path);
    // EDGE-TRIGGER the missing marker (DECISIONS R81). `TailReader::poll` returns `missing:true` on
    // EVERY poll while the file is gone (that's correct - it's how core later detects reappearance),
    // so forwarding each one spammed one "--- file missing ---" line per poll (~10/s) into the tail
    // window (the user's .i18 bug). We forward `missing` only on the TRANSITION into missing, and clear
    // the latch once the file is present again (any non-missing poll), so a delete->recreate->delete
    // cycle still shows exactly one marker per disappearance.
    let mut reported_missing = false;
    // Same edge-trigger for open/read ERRORS (review B2): one marker per failure episode.
    let mut reported_error = false;
    loop {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        let r = reader.poll();
        let had_text = r.new_text.is_some();
        // Suppress a repeated missing: pass it through only on the first poll that goes missing.
        let missing_edge = r.missing && !reported_missing;
        if r.missing {
            reported_missing = true;
        } else if r.error.is_none() {
            // File is present (or produced output) this poll - re-arm so the NEXT disappearance is
            // reported again. A `reappeared` result is a present poll, so this covers it too. An
            // ERROR poll (open/stat/read failed) proves nothing about presence and must NOT re-arm:
            // a flapping share alternating "not found" with "timed out" used to print a missing
            // marker per flap (review #4 finding 13, DECISIONS R128).
            reported_missing = false;
        }
        let error_edge = match (&r.error, reported_error) {
            (Some(e), false) => {
                reported_error = true;
                Some(e.clone())
            }
            (Some(_), true) => None,
            (None, _) => {
                reported_error = false;
                None
            }
        };
        // Only send when there's something to report, to keep the channel quiet. `missing` uses the
        // edge, not the raw flag, so a steady-missing file sends nothing after the first marker.
        // `ready` (the first present poll) always sends, even for an empty file (finding 4).
        if r.new_text.is_some()
            || r.rotated
            || r.reappeared
            || missing_edge
            || error_edge.is_some()
            || r.skipped_bytes.is_some()
            || r.ready
        {
            let chunk = TailChunk {
                new_text: r.new_text,
                rotated: r.rotated,
                reappeared: r.reappeared,
                missing: missing_edge,
                encoding_label: r.encoding_label,
                error: error_edge,
                skipped_bytes: r.skipped_bytes,
                ready: r.ready,
            };
            // A closed channel (GUI dropped the stream) ends the thread. A FULL channel parks the
            // thread here until the GUI drains (R142 back-pressure) — and a receiver dropped while
            // we are parked also returns `Err`, so a closed window still ends the thread promptly.
            if tx.send(chunk).is_err() {
                break;
            }
        }
        // A large history streams in bounded pieces (review #2 finding 5): poll again at once
        // while the reader says more is waiting, checking `stop` first so a closed window ends
        // the load within one piece instead of after the whole file. The reader sets `more` ONLY
        // when the poll actually advanced and hit no error (review #4 finding 2), so this can never
        // spin on a file that cannot be read; the belt-and-braces check on `new_text` keeps that
        // true even if a future reader change forgot the rule.
        if r.more && had_text {
            continue;
        }
        // Sleep one poll period (the watch poll interval, R144), or until the handle is dropped
        // (wake channel disconnects) — review B1.
        let sleep_ms = poll_ms.load(std::sync::atomic::Ordering::Relaxed);
        if let Err(std::sync::mpsc::RecvTimeoutError::Disconnected) =
            wake.recv_timeout(std::time::Duration::from_millis(sleep_ms))
        {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests RUN on every host now that the runtime is cross-platform (notify links
    // everywhere). They verify the thread + channel bridge end to end against a real temp
    // directory: the scheduling-policy arithmetic is already covered off-hardware by
    // dirwatch-core's SweepScheduler tests. They are timing-sensitive (they poll a real FS) so
    // they use generous retry budgets rather than fixed sleeps.

    fn tmpdir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "dwrt_{}_{}_{}",
            tag,
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn drain_until<F: Fn(&WatchEvent) -> bool>(rt: &WatchRuntime, pred: F, tries: u32) -> bool {
        for _ in 0..tries {
            if rt.try_drain().iter().any(&pred) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(60));
        }
        false
    }

    #[test]
    fn runtime_reports_a_new_file_as_discovered_and_active() {
        let dir = tmpdir("new");
        // A sentinel that exists BEFORE the runtime starts: the initial sweep (start(), which is
        // discovery-only then sets started=true — watch.rs) must report it as Discovered. Waiting
        // for that Discovered is the DETERMINISTIC proof the initial sweep has run, replacing a
        // fixed pre-create sleep that raced the first scheduled poll on a slow/loaded box (macOS
        // .i45 flake: a.log created before start() was classified Discovered-only, never Activity).
        std::fs::write(dir.join("sentinel.log"), "s\n").unwrap();
        let rt = WatchRuntime::start(WatchConfig {
            directory: dir.clone(),
            patterns: vec![],
            poll_interval_ms: 100,
            depth: 0,
        });
        let started = drain_until(
            &rt,
            |e| matches!(e, WatchEvent::Discovered(p) if p.ends_with("sentinel.log")),
            60,
        );
        assert!(
            started,
            "initial sweep did not report the pre-existing sentinel as Discovered"
        );
        // Now the runtime is past its initial sweep (started=true), so a file created here MUST
        // raise Activity, not just Discovered.
        std::fs::write(dir.join("a.log"), "x\n").unwrap();
        let saw = drain_until(
            &rt,
            |e| matches!(e, WatchEvent::Activity(p) if p.ends_with("a.log")),
            60,
        );
        assert!(saw, "expected Activity for a newly created matching file");
        drop(rt);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_live_poll_change_does_not_restart_the_watch_or_drop_activity() {
        // Finding 6: a Settings poll change used to `runtime = None; WatchRuntime::start(cfg)`,
        // which dropped queued events and re-ran a discovery-only first sweep — a file written in
        // that window became Discovered, never Activity/unread. The fix applies the poll change LIVE
        // via set_poll_ms, keeping the same running thread (started=true). Proof: after a live poll
        // change, a newly created file still raises Activity (it would be Discovered-only if the
        // runtime had restarted). set_poll_ms also updates the observable live cadence.
        let dir = tmpdir("polllive");
        std::fs::write(dir.join("sentinel.log"), "s\n").unwrap();
        let rt = WatchRuntime::start(WatchConfig {
            directory: dir.clone(),
            patterns: vec![],
            poll_interval_ms: 100,
            depth: 0,
        });
        // Wait out the initial (discovery-only) sweep, so the runtime is past started=true.
        assert!(
            drain_until(
                &rt,
                |e| matches!(e, WatchEvent::Discovered(p) if p.ends_with("sentinel.log")),
                60,
            ),
            "initial sweep did not run"
        );
        // Apply a live poll change (the finding-6 action). No restart.
        assert_eq!(rt.poll_ms(), 100);
        rt.set_poll_ms(250);
        assert_eq!(rt.poll_ms(), 250, "live cadence not updated");
        // A file created AFTER the live change must still be Activity, not merely Discovered — which
        // is only true if the same thread kept running (a restart would rediscover it).
        std::fs::write(dir.join("after.log"), "x\n").unwrap();
        let saw_activity = drain_until(
            &rt,
            |e| matches!(e, WatchEvent::Activity(p) if p.ends_with("after.log")),
            80,
        );
        assert!(
            saw_activity,
            "a file written after a live poll change was not reported Activity — the watch restarted"
        );
        drop(rt);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_watch_event_channel_is_bounded_under_a_blocked_drain() {
        // Finding 7: the WatchEvent channel was unbounded (mpsc::channel), so while `update` was
        // blocked (a modal Browse dialog) the runtime buffered one event per changed file per sweep
        // without bound. Now sync_channel(WATCH_EVENT_CHANNEL_CAP) with coalesce-on-full: a full
        // channel skips the dispatch and the next sweep re-derives. Here we NEVER drain (simulating
        // the blocked GUI) while a busy directory is swept many times, and assert the queue can hold
        // at most the cap — the unbounded channel would hold thousands (the pre-fix probe: 10000).
        use std::io::Write;
        let dir = tmpdir("f7cap");
        let n = 400usize;
        for i in 0..n {
            std::fs::write(dir.join(format!("f{i}.log")), b"x").unwrap();
        }
        let rt = WatchRuntime::start(WatchConfig {
            directory: dir.clone(),
            patterns: vec!["*.log".to_string()],
            poll_interval_ms: 100,
            depth: 0,
        });
        // Let sweeps run WITHOUT draining: append to every file repeatedly so each sweep raises n
        // Activity events. With the drain blocked, the bounded channel must not exceed its cap.
        for round in 0..8u8 {
            for i in 0..n {
                let mut f = std::fs::OpenOptions::new()
                    .append(true)
                    .open(dir.join(format!("f{i}.log")))
                    .unwrap();
                f.write_all(&[b'a' + round]).unwrap();
            }
            std::thread::sleep(std::time::Duration::from_millis(140));
        }
        // Now drain in one go (the GUI unblocks). The bound guarantees the buffered depth never
        // exceeded the cap; try_drain returns at most what the channel held.
        let drained = rt.try_drain().len();
        assert!(
            drained <= WATCH_EVENT_CHANNEL_CAP,
            "bounded channel exceeded its cap: {drained} > {WATCH_EVENT_CHANNEL_CAP}"
        );
        // Sanity: the watch is still live after coalescing — a fresh write is still delivered
        // (the sweep re-derives; nothing wedged). Drain first to make room, then observe.
        let _ = rt.try_drain();
        std::fs::write(dir.join("fresh.log"), b"y").unwrap();
        let still_live = drain_until(
            &rt,
            |e| matches!(e, WatchEvent::Activity(p) | WatchEvent::Discovered(p) if p.ends_with("fresh.log")),
            80,
        );
        assert!(still_live, "watch stopped delivering after coalescing");
        drop(rt);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn runtime_reports_missing_on_delete() {
        let dir = tmpdir("del");
        std::fs::write(dir.join("b.log"), "x\n").unwrap();
        let rt = WatchRuntime::start(WatchConfig {
            directory: dir.clone(),
            patterns: vec![],
            poll_interval_ms: 100,
            depth: 0,
        });
        std::thread::sleep(std::time::Duration::from_millis(150));
        let _ = rt.try_drain(); // clear discovery
        std::fs::remove_file(dir.join("b.log")).unwrap();
        let saw = drain_until(
            &rt,
            |e| matches!(e, WatchEvent::Missing(p) if p.to_string_lossy().to_lowercase().ends_with("b.log")),
            40,
        );
        assert!(saw, "expected Missing after deleting a watched file");
        drop(rt);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tail_stream_emits_missing_once_not_repeatedly() {
        // REGRESSION (DECISIONS R81): deleting a file with an open tail spammed one "file missing"
        // marker per poll (~10/s) because `TailReader::poll` returns `missing:true` every poll while
        // the file is gone, and the thread forwarded each one. The fix makes `missing` (and the
        // encoding-only chunk) EDGE-triggered in the thread: send once on the transition into missing.
        let dir = tmpdir("miss");
        let f = dir.join("t.log");
        std::fs::write(&f, "hello\n").unwrap();
        let stream = TailStream::start(f.clone(), 100);
        std::thread::sleep(std::time::Duration::from_millis(200)); // read the initial content
        let _ = stream.try_drain(); // clear the initial text chunk
        std::fs::remove_file(&f).unwrap();
        let mut missing_count = 0;
        for _ in 0..20 {
            for c in stream.try_drain() {
                if c.missing {
                    missing_count += 1;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(60));
        }
        drop(stream);
        let _ = std::fs::remove_dir_all(&dir);
        // EXACTLY one (review #4 test-quality note: `<= 1` passed even if no marker was ever sent).
        assert_eq!(
            missing_count, 1,
            "missing must be emitted exactly once after a delete, got {missing_count}"
        );
    }

    #[test]
    fn tail_stream_reports_missing_again_after_reappear_and_redelete() {
        // The edge-trigger must RE-ARM: delete -> (1 marker), recreate, delete again -> (1 more).
        // A latch that never resets would swallow the second disappearance.
        let dir = tmpdir("miss2");
        let f = dir.join("t.log");
        std::fs::write(&f, "a\n").unwrap();
        let stream = TailStream::start(f.clone(), 100);
        std::thread::sleep(std::time::Duration::from_millis(180));
        let _ = stream.try_drain();

        let count_missing = |stream: &TailStream, tries: u32| -> usize {
            let mut n = 0;
            for _ in 0..tries {
                for c in stream.try_drain() {
                    if c.missing {
                        n += 1;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(60));
            }
            n
        };

        std::fs::remove_file(&f).unwrap();
        let first = count_missing(&stream, 12);
        assert_eq!(first, 1, "first delete should report exactly one missing");

        std::fs::write(&f, "b\n").unwrap(); // reappear
        std::thread::sleep(std::time::Duration::from_millis(300));
        let _ = stream.try_drain(); // clear reappear + text

        std::fs::remove_file(&f).unwrap(); // delete again
        let second = count_missing(&stream, 12);
        assert_eq!(
            second, 1,
            "second delete should report one missing again (re-armed)"
        );

        drop(stream);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- .i29 (review B1 / B2, DECISIONS R97) ---

    #[test]
    fn dropping_a_tail_stream_does_not_block_on_the_initial_read() {
        // REGRESSION (review B1): `Drop` joined the reader thread, so closing a window whose big
        // file was still loading blocked the GUI for the whole read (measured 315 ms for 300 MB on
        // local NVMe before the fix). Now the drop must return immediately even while the thread
        // is inside `poll()` on a large file. 60 MB is big enough that a join would clearly show
        // (tens of ms even on fast disks) while keeping the test quick.
        let dir = tmpdir("bigdrop");
        let f = dir.join("big.log");
        let line = "2026-09-09 12:00:00.000 INFO a fairly typical log line with some text in it\n";
        let mut buf = String::with_capacity(line.len() * 800_000);
        for _ in 0..800_000 {
            buf.push_str(line);
        }
        std::fs::write(&f, buf.as_bytes()).unwrap();
        let stream = TailStream::start(f.clone(), 100);
        std::thread::sleep(std::time::Duration::from_millis(5)); // reader is now inside poll()
        let t = Instant::now();
        drop(stream);
        let blocked = t.elapsed();
        // Give the detached thread a moment to notice and exit before the dir is removed.
        std::thread::sleep(std::time::Duration::from_millis(400));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            blocked < std::time::Duration::from_millis(20),
            "drop(TailStream) blocked the caller for {blocked:?} (must not join the reader)"
        );
    }

    #[test]
    fn dropping_the_runtime_returns_immediately() {
        let dir = tmpdir("stopfast");
        let rt = WatchRuntime::start(WatchConfig {
            directory: dir.clone(),
            patterns: vec![],
            poll_interval_ms: 100,
            depth: 0,
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        let t = Instant::now();
        drop(rt);
        let blocked = t.elapsed();
        std::thread::sleep(std::time::Duration::from_millis(200));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            blocked < std::time::Duration::from_millis(20),
            "blocked {blocked:?}"
        );
    }

    #[test]
    fn runtime_reports_an_error_when_the_directory_does_not_exist() {
        // review B2: a bad directory used to look like an empty one.
        let dir = tmpdir("noexist").join("gone");
        let rt = WatchRuntime::start(WatchConfig {
            directory: dir.clone(),
            patterns: vec![],
            poll_interval_ms: 100,
            depth: 0,
        });
        let saw = drain_until(
            &rt,
            |e| matches!(e, WatchEvent::Error(m) if m.contains("cannot read")),
            40,
        );
        assert!(
            saw,
            "expected a WatchEvent::Error for a nonexistent directory"
        );
        drop(rt);
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    #[cfg(unix)]
    fn tail_stream_reports_an_open_error_once() {
        // review B2: a file that can't be opened shows ONE error marker, not silence / not spam.
        use std::os::unix::fs::PermissionsExt;
        let dir = tmpdir("tailerr");
        let f = dir.join("locked.log");
        std::fs::write(&f, "x\n").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&f).is_ok() {
            // Root can read anything (review #2 finding 13): say so loudly rather than pass
            // vacuously. The Windows sibling below covers the sharing-violation case on the gate
            // that has no root bypass.
            eprintln!(
                "SKIPPED tail_stream_reports_an_open_error_once: running as root, mode 000 \
                 does not refuse reads here — proved on a non-root host"
            );
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let errors = count_error_chunks(&f);
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(150));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            errors, 1,
            "exactly one error marker for a steady open failure"
        );
    }

    #[test]
    #[cfg(windows)]
    fn tail_stream_reports_a_sharing_violation_once() {
        // review B2 on Windows (review #2 finding 13): a writer holding the file with no share
        // access makes every open a sharing violation -> exactly ONE `cannot open` marker.
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tmpdir("tailshare");
        let f = dir.join("locked.log");
        std::fs::write(&f, "x\n").unwrap();
        let holder = std::fs::OpenOptions::new()
            .write(true)
            .share_mode(0)
            .open(&f)
            .unwrap();
        let errors = count_error_chunks(&f);
        drop(holder);
        std::thread::sleep(std::time::Duration::from_millis(150));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            errors, 1,
            "exactly one error marker for a steady open failure"
        );
    }

    /// Run a `TailStream` on `f` for ~0.5 s and count the error chunks it delivers.
    fn count_error_chunks(f: &std::path::Path) -> usize {
        let stream = TailStream::start(f.to_path_buf(), 100);
        let mut errors = 0;
        for _ in 0..8 {
            for c in stream.try_drain() {
                if c.error.is_some() {
                    errors += 1;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(60));
        }
        drop(stream);
        errors
    }

    // --- .i32 (review #2 findings 2, 4, 5; DECISIONS R110-R113) ---

    #[test]
    fn tail_stream_reports_ready_for_an_empty_file() {
        // REGRESSION (review #2 finding 4): an existing 0-byte file produced no chunk at all, so
        // the window said "loading..." until its first byte. The first present poll now always
        // sends a chunk flagged `ready`, carrying the encoding label.
        let dir = tmpdir("empty");
        let f = dir.join("empty.log");
        std::fs::write(&f, "").unwrap();
        let stream = TailStream::start(f.clone(), 100);
        let mut ready = 0;
        for _ in 0..10 {
            for c in stream.try_drain() {
                if c.ready {
                    ready += 1;
                    assert!(c.new_text.is_none());
                    assert!(
                        !c.encoding_label.is_empty(),
                        "label rides on the ready chunk"
                    );
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        drop(stream);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(ready, 1, "exactly one ready chunk");
    }

    #[test]
    fn tail_stream_streams_a_large_history_in_pieces_and_stops_between_them() {
        // REGRESSION (review #2 finding 5): the whole history used to arrive as ONE chunk and a
        // stop could only take effect after the whole read. With the 4 MB piece bound, a 20 MB
        // file arrives as several chunks, and a drop mid-load ends the thread within one piece.
        let dir = tmpdir("pieces");
        let f = dir.join("big.log");
        let line = "2026-09-11 00:00:00.000 INFO a fairly typical log line with some text in it\n";
        let mut buf = String::with_capacity(line.len() * 270_000);
        for _ in 0..270_000 {
            buf.push_str(line); // ~20 MB
        }
        std::fs::write(&f, buf.as_bytes()).unwrap();
        let stream = TailStream::start(f.clone(), 100);
        let mut chunks = 0usize;
        let mut bytes = 0usize;
        let mut max_chunk = 0usize;
        for _ in 0..60 {
            for c in stream.try_drain() {
                if let Some(t) = c.new_text {
                    chunks += 1;
                    bytes += t.len();
                    max_chunk = max_chunk.max(t.len());
                }
            }
            if bytes >= buf.len() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(bytes, buf.len(), "all bytes arrive");
        assert!(
            chunks >= 5,
            "20 MB must arrive in several pieces, got {chunks}"
        );
        assert!(
            max_chunk <= dirwatch_core::tail::MAX_BYTES_PER_POLL as usize,
            "no piece exceeds the per-poll bound: {max_chunk}"
        );
        drop(stream);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn runtime_reports_recovered_after_the_directory_comes_back() {
        // review #2 finding 2 (d): the GUI needs an edge to clear its ERROR note.
        let base = tmpdir("recover");
        let dir = base.join("watched");
        std::fs::create_dir_all(&dir).unwrap();
        let away = base.join("away");
        let rt = WatchRuntime::start(WatchConfig {
            directory: dir.clone(),
            patterns: vec![],
            poll_interval_ms: 100,
            depth: 0,
        });
        std::thread::sleep(std::time::Duration::from_millis(150));
        std::fs::rename(&dir, &away).unwrap();
        assert!(drain_until(&rt, |e| matches!(e, WatchEvent::Error(_)), 40));
        std::fs::rename(&away, &dir).unwrap();
        assert!(
            drain_until(&rt, |e| matches!(e, WatchEvent::Recovered), 40),
            "expected WatchEvent::Recovered once the directory reads again"
        );
        drop(rt);
        let _ = std::fs::remove_dir_all(&base);
    }

    // --- .i40 (review #4 findings 2, 11, 13; DECISIONS R128) ---

    #[test]
    #[cfg(unix)]
    fn tail_stream_reports_a_read_error_once_and_does_not_spin() {
        // REGRESSION (review #4 finding 2): a path whose open+stat succeed but whose read fails (a
        // directory on Unix; a locked region / cloud placeholder on Windows) used to make the
        // reader thread loop on `more` with no sleep — 100 % CPU, nothing shown. Now: ONE
        // `cannot read` marker, then the thread sleeps between polls like any other.
        let dir = tmpdir("readerr");
        let sub = dir.join("dir.log");
        std::fs::create_dir_all(&sub).unwrap();
        let stream = TailStream::start(sub.clone(), 100);
        std::thread::sleep(std::time::Duration::from_millis(450));
        let chunks = stream.try_drain();
        let errors = chunks.iter().filter(|c| c.error.is_some()).count();
        assert_eq!(errors, 1, "exactly one read-error marker: {chunks:?}");
        assert!(
            chunks.len() <= 3,
            "a failing read must not flood the channel (spin): {} chunks",
            chunks.len()
        );
        drop(stream);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_replaced_by_a_directory_does_not_rearm_the_missing_marker() {
        // REGRESSION (review #4 finding 13): an ERROR poll between two missing polls cleared the
        // missing latch, so one absence produced two "file missing" markers (plus the error).
        // Sequence: file present -> deleted (missing) -> a DIRECTORY appears at the path (open ok
        // on Unix, read error; open error on Windows — either way an error poll, not "present")
        // -> directory removed (missing again). Exactly ONE missing marker overall.
        let dir = tmpdir("relatch");
        let f = dir.join("t.log");
        std::fs::write(&f, "hello\n").unwrap();
        let stream = TailStream::start(f.clone(), 100);
        std::thread::sleep(std::time::Duration::from_millis(250));
        let _ = stream.try_drain();
        std::fs::remove_file(&f).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(250));
        std::fs::create_dir_all(&f).unwrap(); // an error poll (not missing, not present)
        std::thread::sleep(std::time::Duration::from_millis(250));
        std::fs::remove_dir_all(&f).unwrap(); // missing again — same absence episode
        std::thread::sleep(std::time::Duration::from_millis(250));
        let chunks = stream.try_drain();
        let missing = chunks.iter().filter(|c| c.missing).count();
        drop(stream);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(missing, 1, "one absence == one missing marker: {chunks:?}");
    }

    // --- .i48 (review #6 findings 3 and 8; DECISIONS R142/R144) ---

    #[test]
    fn a_tail_stream_never_queues_more_than_the_channel_bound_while_undrained() {
        // REGRESSION (review #6 finding 3, R142): with `update` blocked (the Browse dialog) a busy
        // log used to queue itself whole in the unbounded channel — 250 MB in 3 s measured. Now the
        // reader parks on `send` once TAIL_CHANNEL_CHUNKS pieces are waiting.
        use std::io::Write;
        let dir = tmpdir("bounded");
        let f = dir.join("busy.log");
        std::fs::write(&f, "start\n").unwrap();
        let stream = TailStream::start(f.clone(), 100);
        std::thread::sleep(std::time::Duration::from_millis(250));
        let _ = stream.try_drain();
        // ~1 MB every 10 ms for 1.5 s (~100 MB/s), nobody draining.
        let line = "2026-09-16 12:00:00.000 DEBUG a chatty trace line with some payload in it.\n";
        let mut piece = String::new();
        while piece.len() < 1024 * 1024 {
            piece.push_str(line);
        }
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (s2, f2) = (stop.clone(), f.clone());
        let w = std::thread::spawn(move || {
            let mut fh = std::fs::OpenOptions::new().append(true).open(&f2).unwrap();
            let mut n = 0usize;
            while !s2.load(std::sync::atomic::Ordering::Relaxed) {
                fh.write_all(piece.as_bytes()).unwrap();
                n += piece.len();
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            n
        });
        std::thread::sleep(std::time::Duration::from_millis(1500));
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let written = w.join().unwrap();
        let chunks = stream.try_drain();
        let queued: usize = chunks
            .iter()
            .map(|c| c.new_text.as_ref().map(|t| t.len()).unwrap_or(0))
            .sum();
        drop(stream);
        std::thread::sleep(std::time::Duration::from_millis(200));
        let _ = std::fs::remove_dir_all(&dir);
        let bound = TAIL_CHANNEL_CHUNKS * dirwatch_core::tail::MAX_BYTES_PER_POLL as usize;
        assert!(
            written > bound,
            "the writer must outrun the bound for this test to mean anything ({written} bytes)"
        );
        // The GUI's one try_drain may also pull the piece the reader was parked on: bound + 1.
        assert!(
            chunks.len() <= TAIL_CHANNEL_CHUNKS + 1
                && queued <= bound + dirwatch_core::tail::MAX_BYTES_PER_POLL as usize,
            "queued {} bytes in {} chunks; bound is {} chunks / {} bytes",
            queued,
            chunks.len(),
            TAIL_CHANNEL_CHUNKS,
            bound
        );
    }

    #[test]
    fn dropping_a_parked_tail_stream_ends_its_reader() {
        // R142: a reader parked on a full channel must still exit when the window closes (the
        // receiver drops), and the drop itself must not block.
        let dir = tmpdir("parked");
        let f = dir.join("big.log");
        let line = "2026-09-16 12:00:00.000 INFO a fairly typical log line with some text in it\n";
        let mut buf = String::with_capacity(line.len() * 600_000);
        for _ in 0..600_000 {
            buf.push_str(line); // ~46 MB > 8 x 4 MB: the reader WILL park
        }
        std::fs::write(&f, buf.as_bytes()).unwrap();
        let stream = TailStream::start(f.clone(), 100);
        std::thread::sleep(std::time::Duration::from_millis(800)); // reader fills the channel and parks
        let t = Instant::now();
        drop(stream);
        let blocked = t.elapsed();
        assert!(
            blocked < std::time::Duration::from_millis(50),
            "drop blocked {blocked:?}"
        );
        // The file can be removed only once the reader has let go (the thread exits within a poll).
        std::thread::sleep(std::time::Duration::from_millis(400));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tail_stream_polls_at_the_configured_period_and_a_live_change_applies() {
        // R144: the reader sleeps `poll_ms` (floored at TAIL_POLL_FLOOR_MS) between polls, and
        // `set_poll_ms` changes a running stream without restarting it (no re-delivery).
        use std::io::Write;
        let dir = tmpdir("cadence");
        let f = dir.join("c.log");
        std::fs::write(&f, "a\n").unwrap();
        let stream = TailStream::start(f.clone(), 5); // below the floor -> floored
        assert_eq!(stream.poll_ms(), TAIL_POLL_FLOOR_MS);
        std::thread::sleep(std::time::Duration::from_millis(250));
        let first: String = stream
            .try_drain()
            .into_iter()
            .filter_map(|c| c.new_text)
            .collect();
        assert_eq!(first, "a\n");
        stream.set_poll_ms(1000);
        assert_eq!(stream.poll_ms(), 1000);
        // An append now takes up to ~1 s to show (the new period), but it shows exactly once.
        let mut fh = std::fs::OpenOptions::new().append(true).open(&f).unwrap();
        fh.write_all(b"b\n").unwrap();
        let mut got = String::new();
        for _ in 0..30 {
            for c in stream.try_drain() {
                if let Some(t) = c.new_text {
                    got.push_str(&t);
                }
            }
            if got == "b\n" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        drop(stream);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            got, "b\n",
            "the append arrives once, never re-delivered with the old text"
        );
    }
}
