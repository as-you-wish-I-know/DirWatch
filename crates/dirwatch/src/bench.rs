//! `DIRWATCH_SEARCH_BENCH` — a DEBUG-GATED, headless self-measurement of the per-keystroke search
//! scan cost (DECISIONS R161). It exists to answer, with numbers on the user's hardware, the open
//! "search-per-keystroke" question: is a full `find_matches` scan on the GUI thread (what
//! `SearchChanged` runs per keystroke, `gui/mod.rs::refresh_matches`) fast enough to leave alone, or
//! slow enough to warrant debounce / off-thread work? No product behaviour changes — this fires ONLY
//! when BOTH `DIRWATCH_DEBUG` and `DIRWATCH_SEARCH_BENCH` are set, so a normal user can never trigger
//! it (mirrors the `DIRWATCH_PRUNE_SECS` debug-override discipline, R157).
//!
//! WHAT IT MEASURES: the EXACT work `TailWin::refresh_matches` does — `search::find_matches(text,
//! query)` then `search::clamp_current(..)` — timed with a monotonic clock, on a synthetic buffer.
//! `refresh_matches` is those two calls and nothing else, and `find_matches` is the whole cost, so
//! timing them here measures precisely what the GUI thread spends per keystroke. It does NOT drive a
//! live keystroke through a rendered window (there is no headless keystroke-injection path, and the
//! scan cost is not in the widget) — the decision this feeds is "is the scan slow", which this times
//! directly.
//!
//! CLAMPS (the "existing clamps" the sweep respects): buffer sizes run up to the real
//! `INITIAL_LOAD_CAP` (48 MB, the most a window loads at open) and `SCROLLBACK_CAP_BYTES` (64 MB, the
//! hard ceiling a live tail is trimmed to). Queries include the worst case (`"e"` — one byte, maximal
//! match density, drives toward `search::MATCH_CAP`) alongside ordinary multi-char queries.
//!
//! CONTENT is deterministic (a fixed log-like line repeated, with a sprinkled hit), so a run is
//! reproducible and the match counts are stable across runs and machines.

use crate::search;
use dirwatch_core::tail::{INITIAL_LOAD_CAP, SCROLLBACK_CAP_BYTES};
use std::time::Instant;

/// Buffer sizes the sweep measures, in bytes. "Normal" (a live tail of recent content) through
/// "stupid human tricks" (a window sitting at the load cap / scrollback ceiling). The two large
/// entries are the real clamps, not round numbers, so the worst case measured is the worst case the
/// app actually permits.
fn buffer_sizes() -> [(&'static str, usize); 5] {
    [
        ("10KB", 10 * 1024),
        ("1MB", 1024 * 1024),
        ("16MB", 16 * 1024 * 1024),
        ("48MB(INITIAL_LOAD_CAP)", INITIAL_LOAD_CAP as usize),
        ("64MB(SCROLLBACK_CAP)", SCROLLBACK_CAP_BYTES),
    ]
}

/// Queries the sweep measures. `"e"` is the pathological one (one byte -> most matches -> toward
/// MATCH_CAP); the rest are ordinary log searches. Order shows the per-keystroke burst of typing
/// "error" one letter at a time ("e","er","err"...): each is a fresh full scan today.
const QUERIES: &[&str] = &["e", "er", "err", "erro", "error", "warn"];

/// Whether the bench should run: BOTH gates set (and non-empty / non-"0"). `DIRWATCH_DEBUG` is the
/// same master gate the trace log uses (R103); `DIRWATCH_SEARCH_BENCH` selects THIS bench so turning
/// on the debug trace alone never launches it.
pub fn requested() -> bool {
    let read = |k: &str| std::env::var_os(k);
    gate_open(
        read("DIRWATCH_DEBUG").as_deref(),
        read("DIRWATCH_SEARCH_BENCH").as_deref(),
    )
}

/// PURE gate rule (unit-testable): both env values must be present and non-empty / non-"0". Split
/// from [`requested`] so the AND-of-both-gates logic is tested without mutating process env.
fn gate_open(debug: Option<&std::ffi::OsStr>, bench: Option<&std::ffi::OsStr>) -> bool {
    let set = |v: Option<&std::ffi::OsStr>| v.map(|s| !(s.is_empty() || s == "0")).unwrap_or(false);
    set(debug) && set(bench)
}

/// Build a deterministic, log-like buffer of AT LEAST `target` bytes. A fixed ~72-byte line is
/// repeated; every 50th line carries "ERROR" so a query for "error" finds a bounded, stable count
/// (not zero, not saturating on the multi-char queries). The one-byte query "e" still saturates
/// toward MATCH_CAP off the ordinary prose, which is the worst case we want to see.
fn make_buffer(target: usize) -> String {
    const PLAIN: &str =
        "2026-09-18 12:00:00 INFO a routine log line with no notable hits here ok\n";
    const HIT: &str = "2026-09-18 12:00:00 ERROR something failed on this particular line here!\n";
    let mut s = String::with_capacity(target + HIT.len());
    let mut i = 0usize;
    while s.len() < target {
        if i.is_multiple_of(50) {
            s.push_str(HIT);
        } else {
            s.push_str(PLAIN);
        }
        i += 1;
    }
    s
}

/// One measurement: run the REAL scan path (`find_matches` + `clamp_current`, i.e. exactly what
/// `refresh_matches` does) and return (match count, elapsed). Repeated `reps` times; the MIN elapsed
/// is reported (least-noise estimate of the true cost — scheduling only ever adds time).
fn measure(text: &str, query: &str, reps: u32) -> (usize, std::time::Duration) {
    let mut best = std::time::Duration::MAX;
    let mut count = 0usize;
    for _ in 0..reps.max(1) {
        let t0 = Instant::now();
        // The two calls `TailWin::refresh_matches` makes, in order. `current_match` starts None
        // (a fresh query), matching the SearchChanged path.
        let matches = search::find_matches(text, query);
        let _current = search::clamp_current(None, matches.len());
        let dt = t0.elapsed();
        best = best.min(dt);
        count = matches.len();
        // Keep the compiler from optimising the scan away.
        std::hint::black_box(&matches);
    }
    (count, best)
}

/// Run the full sweep and write one build-ID-stamped `BENCH` line per (buffer, query) to
/// `DirWatch.log` (via `emit`), plus a header and footer marking the run. `emit` is injected so the
/// unit test can capture the lines without touching `applog`/the filesystem; production passes the
/// `applog` writer.
pub fn run_sweep(reps: u32, mut emit: impl FnMut(&str)) {
    emit(&format!(
        "search-scan sweep START reps={reps} (measures refresh_matches: find_matches + clamp_current)"
    ));
    for (label, size) in buffer_sizes() {
        let text = make_buffer(size);
        for &q in QUERIES {
            let (count, dt) = measure(&text, q, reps);
            let capped = if count >= search::MATCH_CAP { "+" } else { "" };
            emit(&format!(
                "buffer={label} actual_bytes={} query=\"{q}\" matches={count}{capped} scan={:.3}ms",
                text.len(),
                dt.as_secs_f64() * 1000.0
            ));
        }
    }
    emit("search-scan sweep END");
}

/// Entry point called from `main` when [`requested`]. Writes the sweep to `DirWatch.log` through
/// `applog` (the same file `--log-dir` routes and `runtests` collects, R127), then the caller exits
/// WITHOUT launching the GUI — a fast, deterministic, window-free run for the gate/harness.
pub fn run_and_log() {
    // A few reps to de-noise the small buffers; the 48/64 MB scans dominate wall time either way.
    run_sweep(5, |line| {
        applog::append_bench_line(line);
    });
}

// A thin wrapper so the bench can write to `DirWatch.log` with a BENCH-tagged, build-ID-stamped
// line WITHOUT depending on `gui::mod`'s private `write_log_line`. Kept in `applog` (the module that
// owns the file) rather than duplicating the timestamp/stamp format here.
use crate::applog;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn make_buffer_reaches_the_target_and_is_line_terminated() {
        for size in [10 * 1024usize, 1024 * 1024] {
            let b = make_buffer(size);
            assert!(b.len() >= size, "buffer must reach the target size");
            assert!(b.ends_with('\n'), "buffer is whole lines");
        }
    }

    #[test]
    fn sweep_emits_a_line_per_pair_plus_header_and_footer() {
        // Capture the emitted lines instead of writing to disk. One START + one END + one line per
        // (buffer x query). The measurements themselves are timing, not asserted here.
        let mut lines: Vec<String> = Vec::new();
        run_sweep(1, |l| lines.push(l.to_string()));
        let pairs = buffer_sizes().len() * QUERIES.len();
        assert_eq!(
            lines.len(),
            pairs + 2,
            "expected header + {pairs} measurement lines + footer"
        );
        assert!(lines.first().unwrap().contains("sweep START"));
        assert!(lines.last().unwrap().contains("sweep END"));
        // Every measurement line names its buffer, query, a match count and a scan time, and is
        // stamped so a stale log is caught.
        for l in &lines[1..lines.len() - 1] {
            assert!(l.contains("buffer="), "line: {l}");
            assert!(l.contains("query="), "line: {l}");
            assert!(l.contains("matches="), "line: {l}");
            assert!(l.contains("scan="), "line: {l}");
        }
    }

    #[test]
    fn measure_returns_the_same_count_as_a_plain_find_matches() {
        // The bench must time the REAL scan, not a divergent copy: its match count for a query must
        // equal `search::find_matches` on the same buffer (that IS what it calls).
        let text = make_buffer(64 * 1024);
        for &q in QUERIES {
            let (count, _dt) = measure(&text, q, 1);
            assert_eq!(count, search::find_matches(&text, q).len(), "query {q}");
        }
    }

    #[test]
    fn gate_needs_both_env_vars_nonempty_and_nonzero() {
        use std::ffi::OsStr;
        let on = OsStr::new("1");
        let off = OsStr::new("0");
        let empty = OsStr::new("");
        // Both on -> run.
        assert!(gate_open(Some(on), Some(on)));
        // Either missing / empty / "0" -> do not run (a normal user, or debug-trace-only).
        assert!(!gate_open(None, Some(on)));
        assert!(!gate_open(Some(on), None));
        assert!(!gate_open(Some(off), Some(on)));
        assert!(!gate_open(Some(on), Some(off)));
        assert!(!gate_open(Some(empty), Some(on)));
        assert!(!gate_open(Some(on), Some(empty)));
        assert!(!gate_open(None, None));
    }
}
