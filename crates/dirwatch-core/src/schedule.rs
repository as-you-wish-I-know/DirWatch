//! Platform-agnostic *sweep scheduling* policy — the decision of WHEN to run
//! [`WatchService::sweep`](crate::watch::WatchService::sweep), factored out of the bin crate's
//! runtime so it is unit-testable with a mock clock.
//!
//! DESIGN (PORT-PLAN "Filesystem watch"; DECISIONS R8): the sweep is the authority — it always
//! decides state. Two things ask for a sweep:
//!   * the PERIODIC POLL — a fallback reconcile that runs every `poll_interval_ms`. This is why
//!     network/UNC shares work even when the OS FS watcher misses events (DECISIONS entry 10); it
//!     must NEVER be dropped.
//!   * the FS WATCHER (`notify`, wired in the bin crate's cross-platform runtime) — the primary,
//!     real-time detector. Its raw events are bursty (many per write), so they are DEBOUNCED:
//!     an event schedules a sweep `debounce_ms` in the future, and further events within that
//!     window collapse into the one pending sweep rather than each firing their own.
//!
//! This module owns ONLY the timing arithmetic. It reads the current time as an argument (a
//! monotonic millisecond count supplied by the caller) so it carries no clock and no threads —
//! the bin crate's runtime loop owns the real clock, the `notify` watcher, and the GUI thread,
//! and calls [`SweepScheduler::poll`] to learn whether it's time to sweep now. That keeps the
//! whole "when to sweep" policy verifiable here with a mock clock; the OS wiring is covered by the
//! bin crate's runtime integration tests.

/// Decides when a sweep is due, given a monotonic millisecond clock supplied by the caller.
///
/// The caller drives it: on each runtime tick it calls [`poll`](Self::poll) with `now_ms`; when
/// an FS-watcher event arrives it calls [`note_fs_event`](Self::note_fs_event) with `now_ms`.
/// `poll` returns `true` exactly when a sweep should run now, and records that sweep so the next
/// periodic sweep is scheduled one interval later.
///
/// Time is a `u64` millisecond count from an arbitrary monotonic origin (e.g. `Instant` elapsed).
/// Only differences matter, so the origin is irrelevant.
#[derive(Debug)]
pub struct SweepScheduler {
    poll_interval_ms: u64,
    debounce_ms: u64,
    /// Absolute time the next *periodic* sweep is due.
    next_poll_due: u64,
    /// Absolute time a *debounced* FS-event sweep is due, or `None` when none is pending.
    fs_due: Option<u64>,
    /// Whether the very first sweep (the initial scan) has been handed out yet.
    started: bool,
}

impl SweepScheduler {
    /// The FS-watcher debounce window (PORT-PLAN: 150 ms). A burst of raw events collapses into
    /// one sweep this many ms after the FIRST event of the burst.
    pub const DEFAULT_DEBOUNCE_MS: u64 = 150;

    /// `poll_interval_ms` is clamped to the shared [`crate::POLL_INTERVAL_FLOOR_MS`] floor (R118) to
    /// match [`WatchService`](crate::watch::WatchService)'s own clamp — the scheduler must never ask
    /// for sweeps faster than the service is built to service.
    pub fn new(poll_interval_ms: u64, now_ms: u64) -> Self {
        Self::with_debounce(poll_interval_ms, Self::DEFAULT_DEBOUNCE_MS, now_ms)
    }

    /// As [`new`](Self::new) but with an explicit debounce window (used by tests).
    pub fn with_debounce(poll_interval_ms: u64, debounce_ms: u64, now_ms: u64) -> Self {
        let poll_interval_ms = poll_interval_ms.max(crate::POLL_INTERVAL_FLOOR_MS as u64);
        SweepScheduler {
            poll_interval_ms,
            debounce_ms,
            // The FIRST poll is due immediately: the runtime performs the initial scan up front.
            next_poll_due: now_ms,
            fs_due: None,
            started: false,
        }
    }

    pub fn poll_interval_ms(&self) -> u64 {
        self.poll_interval_ms
    }

    /// Record that the FS watcher reported activity at `now_ms`. Schedules a debounced sweep if
    /// one is not already pending; a second event inside the window does NOT push the deadline
    /// out (leading-edge debounce), so a continuous writer still gets swept every `debounce_ms`
    /// rather than being starved until writes stop.
    pub fn note_fs_event(&mut self, now_ms: u64) {
        if self.fs_due.is_none() {
            self.fs_due = Some(now_ms.saturating_add(self.debounce_ms));
        }
    }

    /// Ask whether a sweep should run now. Returns `true` at most once per due deadline; on a
    /// `true` it advances the periodic schedule and clears any satisfied FS-event deadline, so a
    /// single sweep covers both a coincident poll and FS deadline (no double sweep).
    ///
    /// The runtime loop calls this on every tick (e.g. every ~50 ms) and sweeps when it returns
    /// true. Returning the decision — rather than sweeping internally — is what keeps this
    /// module clock-free and testable.
    pub fn poll(&mut self, now_ms: u64) -> bool {
        let poll_due = now_ms >= self.next_poll_due;
        let fs_ready = matches!(self.fs_due, Some(due) if now_ms >= due);

        if !poll_due && !fs_ready {
            return false;
        }

        // Advance the periodic schedule by whole intervals until it's strictly in the future.
        // Anchoring to the interval grid (not to `now`) keeps the cadence drift-free even when a
        // tick arrives late; stepping past `now` collapses several missed slots into one sweep.
        if poll_due {
            while self.next_poll_due <= now_ms {
                self.next_poll_due = self.next_poll_due.saturating_add(self.poll_interval_ms);
            }
        }
        // Any pending FS deadline is satisfied by this sweep.
        if fs_ready {
            self.fs_due = None;
        }
        self.started = true;
        true
    }

    /// True once the first sweep has been dispatched (the initial scan). Mirrors
    /// [`WatchService`](crate::watch::WatchService)'s own `started` flag for callers that want to
    /// distinguish the initial discovery sweep from later ones.
    pub fn has_started(&self) -> bool {
        self.started
    }

    /// Change the periodic sweep cadence of a RUNNING scheduler in place (REVIEW-2026-09-17 finding
    /// 6): a Settings poll change updates the watch runtime's live interval, which the runtime pushes
    /// here so the swept cadence follows WITHOUT tearing down and respawning the watch thread (a
    /// restart drops queued events and re-runs a discovery-only first sweep). No-op if unchanged.
    /// Re-anchors the next periodic deadline to `now + new_interval` so a change takes effect from
    /// now rather than honouring a deadline set under the old interval (a large-to-small change would
    /// otherwise wait out the old, longer period once). Clamped to the shared floor, as [`new`].
    pub fn set_poll_interval_ms(&mut self, poll_interval_ms: u64, now_ms: u64) {
        let clamped = poll_interval_ms.max(crate::POLL_INTERVAL_FLOOR_MS as u64);
        if clamped == self.poll_interval_ms {
            return;
        }
        self.poll_interval_ms = clamped;
        self.next_poll_due = now_ms.saturating_add(clamped);
    }

    /// Milliseconds until the next scheduled sweep (periodic or debounced FS), for a runtime that
    /// wants to sleep precisely instead of busy-ticking. `0` means one is due now.
    pub fn ms_until_next(&self, now_ms: u64) -> u64 {
        let poll_in = self.next_poll_due.saturating_sub(now_ms);
        match self.fs_due {
            Some(due) => poll_in.min(due.saturating_sub(now_ms)),
            None => poll_in,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_interval_is_floored_at_100ms() {
        let s = SweepScheduler::new(10, 0);
        assert_eq!(s.poll_interval_ms(), 100);
    }

    #[test]
    fn first_poll_is_due_immediately() {
        // The initial scan should fire on the first tick, not after one interval.
        let mut s = SweepScheduler::new(250, 1000);
        assert!(s.poll(1000));
        assert!(s.has_started());
    }

    #[test]
    fn periodic_sweep_fires_once_per_interval() {
        let mut s = SweepScheduler::new(250, 0);
        assert!(s.poll(0)); // initial
        assert!(!s.poll(100)); // too soon
        assert!(!s.poll(249));
        assert!(s.poll(250)); // one interval later
        assert!(!s.poll(300));
        assert!(s.poll(500)); // next interval
    }

    #[test]
    fn set_poll_interval_applies_live_and_reanchors() {
        // Finding 6: a running scheduler's cadence changes in place (a live Settings poll change),
        // without a restart. The change re-anchors the next deadline from `now`, so a large-to-small
        // change takes effect promptly rather than waiting out the old, longer period.
        let mut s = SweepScheduler::new(1000, 0);
        assert!(s.poll(0)); // initial
        assert_eq!(s.poll_interval_ms(), 1000);
        assert!(!s.poll(500)); // not due under the 1000 ms interval
                               // Live change to 250 ms at now=500: next due is 500+250=750, not the old 1000.
        s.set_poll_interval_ms(250, 500);
        assert_eq!(s.poll_interval_ms(), 250);
        assert!(!s.poll(700)); // still before the re-anchored 750
        assert!(s.poll(750)); // fires at the new cadence
        assert!(s.poll(1000)); // and keeps the 250 ms grid
                               // No-op when unchanged (idempotent; does not disturb the schedule).
        s.set_poll_interval_ms(250, 1000);
        assert!(!s.poll(1100));
        assert!(s.poll(1250));
        // Clamped to the floor, like new().
        s.set_poll_interval_ms(10, 1250);
        assert_eq!(s.poll_interval_ms(), 100);
    }

    #[test]
    fn periodic_schedule_does_not_drift_when_ticks_are_late() {
        // A tick arriving well past the deadline still re-anchors to whole intervals, not to now,
        // so we don't slowly slip. After a late tick at 630 (interval 250, slots 0,250,500,750),
        // the next due slot is 750.
        let mut s = SweepScheduler::new(250, 0);
        assert!(s.poll(0));
        assert!(s.poll(630)); // covers the 250 and 500 slots at once (single catch-up sweep)
        assert!(!s.poll(700));
        assert!(s.poll(750)); // back on the whole-interval grid
    }

    #[test]
    fn fs_event_schedules_a_debounced_sweep() {
        let mut s = SweepScheduler::with_debounce(10_000, 150, 0);
        assert!(s.poll(0)); // consume the initial poll so it doesn't mask the fs sweep
        s.note_fs_event(1000);
        assert!(!s.poll(1100)); // within debounce window
        assert!(s.poll(1150)); // debounce elapsed -> sweep
        assert!(!s.poll(1200)); // and it's a one-shot
    }

    #[test]
    fn burst_of_fs_events_collapses_into_one_sweep() {
        let mut s = SweepScheduler::with_debounce(10_000, 150, 0);
        assert!(s.poll(0));
        s.note_fs_event(1000);
        s.note_fs_event(1010);
        s.note_fs_event(1050); // all within the one 150ms window from the FIRST event
        assert!(!s.poll(1100));
        assert!(s.poll(1150)); // single sweep at first-event + 150
        assert!(!s.poll(1160));
    }

    #[test]
    fn continuous_writer_is_not_starved_leading_edge_debounce() {
        // A never-ending stream of events must still be swept every debounce window, not deferred
        // forever. Leading-edge: the deadline is set from the FIRST event and not pushed out.
        let mut s = SweepScheduler::with_debounce(10_000, 150, 0);
        assert!(s.poll(0));
        s.note_fs_event(1000);
        assert!(s.poll(1150)); // first window fires
        s.note_fs_event(1160); // new window opens
        assert!(s.poll(1310)); // fires again ~150ms later
    }

    #[test]
    fn coincident_poll_and_fs_deadline_is_a_single_sweep() {
        // If the periodic poll and a debounced fs deadline land on the same tick, poll() returns
        // true once and both schedules are satisfied — no immediate second sweep.
        let mut s = SweepScheduler::with_debounce(200, 150, 0);
        assert!(s.poll(0)); // initial poll; next periodic due at 200
        s.note_fs_event(50); // fs due at 200 as well
        assert!(s.poll(200)); // single sweep covers both
        assert!(!s.poll(201)); // neither fires again immediately
    }

    #[test]
    fn ms_until_next_reports_the_nearer_deadline() {
        let mut s = SweepScheduler::with_debounce(250, 150, 0);
        assert!(s.poll(0)); // next periodic at 250
        assert_eq!(s.ms_until_next(0), 250);
        s.note_fs_event(0); // fs due at 150, nearer
        assert_eq!(s.ms_until_next(0), 150);
    }
}
