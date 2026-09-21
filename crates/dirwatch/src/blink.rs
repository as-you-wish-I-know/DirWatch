//! Blink cadence for the tiles and the cap-reached count field (PORT-PLAN-crossplatform §6 step 4b,
//! DECISIONS R71). The old DirWatch blinked the active-closed tile so it reads distinct from the
//! (same-amber, steady) unread tile, and blinked the `Open Windows: N of M` count for ~3 s after an
//! over-cap open attempt. Both share ONE phase clock so they toggle together and the future Settings
//! legend (step 5) can read the same source — no drift between the tile blink and the legend swatch
//! (the "share scaffolding between siblings" invariant, R28a).
//!
//! Kept PURE (no iced types, no wall clock) so it is UNIT-TESTABLE off-hardware, the same discipline
//! as `placement.rs`/`visible.rs`. The GUI already runs a 100 ms tick; it feeds an incrementing tick
//! counter in here and gets back a bright/dim phase. the user's choice 1A: derive the phase from the
//! existing tick counter, no extra timer; cadence ~450 ms (he's fine anywhere 400-500 ms).

/// The GUI tick period in ms (mirror of `gui::TICK_MS`; kept here so the phase math is self-contained
/// and testable without importing the GUI). If the GUI tick changes, update both together.
pub const TICK_MS: u64 = 100;

/// Blink half-period in ms: the tile stays bright for this long, then dim for this long, so a full
/// bright->dim->bright cycle is ~900 ms and each visible state lasts ~450 ms. the user accepted 400-500.
/// With a 100 ms tick this is a whole number of ticks (~5), so the phase toggles cleanly on a tick
/// boundary rather than mid-tick.
pub const BLINK_HALF_MS: u64 = 450;

/// Bounds (in whole seconds) on how long the cap-reached count field blinks after the LAST over-cap
/// open attempt. Was a fixed 3 s (`CAP_BLINK_MS`, DECISIONS R29 item 5); at .i18 (DECISIONS R80) the user
/// tied it to the Active-timeout setting, clamped to integer seconds `[1, 10]`, so the flash duration
/// tracks how long a file reads as "active".
pub const CAP_BLINK_MIN_S: u64 = 1;
pub const CAP_BLINK_MAX_S: u64 = 10;

/// PURE: the cap-flash duration in ms, derived from the Active-timeout setting (DECISIONS R80). Clamps
/// `active_seconds` to whole seconds in `[CAP_BLINK_MIN_S, CAP_BLINK_MAX_S]` then converts to ms. A
/// negative/zero Active (shouldn't happen - the model floors it at 1) clamps up to the 1 s minimum.
/// Unit-testable off-hardware; the GUI passes `SessionModel::active_seconds`.
pub fn cap_blink_ms(active_seconds: i64) -> u64 {
    let secs = (active_seconds.max(0) as u64).clamp(CAP_BLINK_MIN_S, CAP_BLINK_MAX_S);
    secs * 1000
}

/// Number of ticks in one blink half-period (rounded to at least 1). The phase is bright for this
/// many ticks, then dim for this many.
pub const fn blink_half_ticks() -> u64 {
    let t = BLINK_HALF_MS / TICK_MS;
    if t == 0 {
        1
    } else {
        t
    }
}

/// True on the BRIGHT half of the blink cycle for the given tick count, false on the dim half.
/// Deterministic in `tick` alone, so a test can walk the cycle without any clock.
pub fn is_bright(tick: u64) -> bool {
    let half = blink_half_ticks();
    // tick / half is the half-period index; even => bright, odd => dim.
    (tick / half).is_multiple_of(2)
}

/// Whether the cap-reached blink is still active: true while `now_ms` is within
/// `[start, start + duration_ms)` of the last over-cap attempt at `cap_blink_start_ms`. The duration
/// is passed in (from [`cap_blink_ms`]) rather than a fixed constant, so the flash tracks the Active
/// timeout (DECISIONS R80). `None` start => never triggered => not blinking.
pub fn cap_blink_active(cap_blink_start_ms: Option<u64>, now_ms: u64, duration_ms: u64) -> bool {
    match cap_blink_start_ms {
        Some(start) => now_ms < start.saturating_add(duration_ms),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blink_half_is_a_whole_number_of_ticks() {
        // 450 / 100 = 4 ticks (integer). The exact count isn't load-bearing; that it's >=1 and the
        // cadence lands in the user's 400-500 ms window is.
        let half = blink_half_ticks();
        assert!(half >= 1);
        let ms = half * TICK_MS;
        assert!(
            (400..=500).contains(&ms),
            "blink half-period {ms} ms is outside the accepted 400-500 ms"
        );
    }

    #[test]
    fn phase_starts_bright_and_toggles_each_half_period() {
        let half = blink_half_ticks();
        // First half-period: bright.
        assert!(is_bright(0));
        assert!(is_bright(half - 1));
        // Second half-period: dim.
        assert!(!is_bright(half));
        assert!(!is_bright(2 * half - 1));
        // Third: bright again.
        assert!(is_bright(2 * half));
    }

    #[test]
    fn phase_is_periodic() {
        let half = blink_half_ticks();
        let period = 2 * half;
        for t in 0..(period * 3) {
            assert_eq!(
                is_bright(t),
                is_bright(t + period),
                "phase must repeat every full cycle"
            );
        }
    }

    #[test]
    fn cap_blink_is_active_within_the_window_then_stops() {
        let start = 10_000;
        let dur = 3000; // an arbitrary duration for the window math
        assert!(cap_blink_active(Some(start), start, dur)); // at the start
        assert!(cap_blink_active(Some(start), start + dur - 1, dur)); // just inside
        assert!(!cap_blink_active(Some(start), start + dur, dur)); // exactly at the edge = done
        assert!(!cap_blink_active(Some(start), start + dur + 1, dur)); // past
    }

    #[test]
    fn cap_blink_never_active_when_untriggered() {
        assert!(!cap_blink_active(None, 0, 3000));
        assert!(!cap_blink_active(None, 999_999, 3000));
    }

    #[test]
    fn cap_blink_ms_tracks_active_clamped_to_1_10_seconds() {
        // The flash duration follows Active timeout, clamped to whole seconds [1, 10] (DECISIONS R80).
        assert_eq!(cap_blink_ms(5), 5000); // in range -> that many seconds
        assert_eq!(cap_blink_ms(1), 1000); // min
        assert_eq!(cap_blink_ms(10), 10_000); // max
        assert_eq!(cap_blink_ms(0), 1000); // below min clamps up to 1 s
        assert_eq!(cap_blink_ms(300), 10_000); // above max clamps down to 10 s
        assert_eq!(cap_blink_ms(-4), 1000); // defensive: negative clamps to 1 s
    }
}
