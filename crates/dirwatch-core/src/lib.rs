//! DirWatch core — platform-agnostic logic (glob, encoding, tail, watch, session, CLI).
//!
//! This crate is the port target for the .NET `DirWatch.Core` (behavioral spec, build
//! 2026-07-14.9). It builds and unit-tests on any host, including the Linux dev session — this is
//! the part of DirWatch that is verifiable OFF the user's Windows hardware.
//!
//! All six core modules are ported and unit-tested to parity with the C# suite (build .r2), plus
//! the multibyte-split regression. The GUI (iced, cross-platform) lives in the `dirwatch` bin crate.

/// The floor for the fallback-reconcile poll interval, in milliseconds (P1 / DECISIONS R14).
///
/// Review #2 finding 16 (DECISIONS R118): this used to be a bare `100` hardcoded in THREE places
/// — `WatchService::new`, `SweepScheduler::with_debounce`, and the GUI's `POLL_MS_FLOOR` — which
/// could silently drift apart. It now lives here as the single source; all three read it (the GUI's
/// `POLL_MS_FLOOR` is `= dirwatch_core::POLL_INTERVAL_FLOOR_MS`).
pub const POLL_INTERVAL_FLOOR_MS: u32 = 100;

/// The ceiling for the poll interval, in milliseconds (one minute). Review #2 finding 8 gave the
/// Settings dialog this ceiling (as the GUI-local `POLL_MS_CEILING`); review #4 finding 19 noted the
/// CLI's `--poll-ms` had the floor but not the ceiling, so `--poll-ms 2000000000` produced a 23-day
/// poll. Now ONE shared constant: `main.rs` clamps the CLI value and the GUI's `POLL_MS_CEILING`
/// reads it, the same way `POLL_INTERVAL_FLOOR_MS` is shared.
pub const POLL_INTERVAL_CEILING_MS: u32 = 60_000;

pub mod build_info;
pub mod cli;
pub mod encoding;
pub mod fskey;
pub mod glob;
pub mod license;
pub mod schedule;
// selftest module removed at .i31 (release-prep, DECISIONS R106)
pub mod session;
pub mod tail;
pub mod watch;

/// Shared test helpers (review C3, DECISIONS R97): one `unique()` / temp-dir helper instead of a
/// copy per module. Test-only; never compiled into the exe.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A process-unique-ish suffix: pid + a static counter.
    pub fn unique() -> String {
        static N: AtomicU64 = AtomicU64::new(0);
        format!(
            "{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// A fresh temp directory `<tmp>/<prefix>_<unique>`, created.
    pub fn tmpdir(prefix: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("{prefix}_{}", unique()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
