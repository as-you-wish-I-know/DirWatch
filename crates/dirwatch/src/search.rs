//! Per-file SEARCH within a tail window (BACKLOG §2b / DECISIONS R80). PURE match-finding +
//! current-match navigation, kept free of any iced type so it is UNIT-TESTABLE off-hardware - the
//! same discipline as `placement.rs` / `visible.rs` / `blink.rs`. The GUI owns the query string and
//! the current-match index; this module turns (text, query) into the list of match byte-ranges and
//! does the next/prev/wrap arithmetic. Rendering (highlighting the term, scrolling the match into
//! view) is the GUI's job, driven by what this returns.
//!
//! CASE-INSENSITIVE (ASCII): log search is overwhelmingly case-insensitive, so a search for `error`
//! finds `Error` and `ERROR`. Matching is done on an ASCII-lowercased copy of both sides; the byte
//! offsets returned index the ORIGINAL text (ASCII case-folding is length-preserving, so offsets are
//! unchanged). Non-ASCII bytes are compared as-is (a defensible default; full Unicode case-folding is
//! out of scope and would break the length-preserving offset guarantee).

/// Most matches kept for one query (review #2 finding 6, DECISIONS R112). Each `Match` is 16 bytes;
/// a one-letter query on a 64 MB buffer produced millions of them (~100 MB+). Past the cap the
/// scan stops, the count reads `100000+`, and next/prev cycle within the kept prefix — the first
/// 100 k hits, which is more than anyone steps through. `extend_matches` respects it too.
pub const MATCH_CAP: usize = 100_000;

/// SEARCH DEBOUNCE (DECISIONS R162, from the `.i54` MAINPC measurement). A `find_matches` scan runs
/// on the GUI thread on every keystroke (`SearchChanged`). MAINPC measured that scan at 0.055 ms on
/// a 10 KB buffer and 5.6 ms on 1 MB — imperceptible — but 91 ms on 16 MB and 277–377 ms on the
/// 48–64 MB load/scrollback clamps: typing into a search box over a large tail froze the GUI a
/// fraction of a second PER KEYSTROKE. The fix is CONDITIONAL debounce: below this threshold the scan
/// is instant, so it runs inline as before (no added latency on the common case); at or above it, the
/// scan is deferred until the user pauses typing, collapsing a keystroke burst into one scan.
///
/// 4 MB: at MAINPC's rate a 4 MB scan is ~22 ms (one frame and a half) — still imperceptible inline —
/// while keeping most real-world logs a person searches on the instant path. Above it the deferral
/// pays off; 16 MB (91 ms) and up is where the freeze was felt.
pub const DEBOUNCE_THRESHOLD_BYTES: usize = 4 * 1024 * 1024;

/// A keystroke into a buffer at/above [`DEBOUNCE_THRESHOLD_BYTES`] defers its rescan this many GUI
/// ticks (the app ticks every 100 ms, `gui::TICK_MS`). 2 ticks ≈ a ~200 ms typing pause — the
/// standard find-as-you-type feel: type a burst and one scan runs when you stop, not one per letter.
pub const DEBOUNCE_TICKS: u64 = 2;

/// Whether a `SearchChanged` on a buffer of `text_len` bytes should scan INLINE now (`true`) or be
/// DEFERRED to the debounce tick (`false`). PURE (unit-testable off-hardware): a small buffer always
/// scans inline; a large one defers. The empty-query / clear case is the GUI's business (it scans
/// inline regardless — clearing is not a scan), so this is only consulted for a non-empty query.
pub fn scan_inline(text_len: usize) -> bool {
    text_len < DEBOUNCE_THRESHOLD_BYTES
}

/// Whether a deferred rescan armed for tick `deadline` is now DUE at `tick_count`. PURE. `None`
/// deadline (nothing armed) is never due. A saturating compare so tick wraparound can't mis-fire.
pub fn rescan_due(deadline: Option<u64>, tick_count: u64) -> bool {
    matches!(deadline, Some(d) if tick_count >= d)
}

/// One match: a half-open byte range `[start, end)` in the ORIGINAL text. `end - start` always equals
/// the query's byte length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Match {
    pub start: usize,
    pub end: usize,
}

/// Find every (overlapping-free, left-to-right) occurrence of `query` in `text`, case-insensitively
/// over ASCII. Returns byte ranges into `text`. An empty query returns no matches (an empty search is
/// "no search", not "matches everywhere"). Matches do not overlap: after a hit at `i`, scanning
/// resumes at `i + query.len()`.
///
/// NO LOWERCASED COPY (review B3, DECISIONS R97): this used to `to_ascii_lowercase()` the WHOLE text
/// per call — a 20 MB allocation per keystroke, and per append tick under a standing query. Now it
/// compares in place with `eq_ignore_ascii_case`.
pub fn find_matches(text: &str, query: &str) -> Vec<Match> {
    let mut out = Vec::new();
    scan_from(text, query, 0, &mut out);
    out
}

/// INCREMENTAL search (review B3): `text` grew from `old_len` bytes to its current length; find only
/// the matches that are new, and append them to `matches` (which must be the result of searching
/// `text[..old_len]` for the same `query`). A match straddling the old end is found because the scan
/// starts `query.len() - 1` bytes before `old_len` — but never before the last known match's end, so
/// nothing is double-counted and matches stay non-overlapping and sorted. This is what keeps a live
/// tail's per-append cost O(chunk) instead of O(file).
pub fn extend_matches(text: &str, query: &str, old_len: usize, matches: &mut Vec<Match>) {
    if query.is_empty() {
        return;
    }
    let last_end = matches.last().map(|m| m.end).unwrap_or(0);
    let overlap_start = old_len.saturating_sub(query.len() - 1);
    let from = last_end.max(overlap_start);
    scan_from(text, query, from, matches);
}

/// Scan `text[from..]` for `query` (ASCII case-insensitive, non-overlapping), pushing matches.
fn scan_from(text: &str, query: &str, from: usize, out: &mut Vec<Match>) {
    let hay = text.as_bytes();
    let needle = query.as_bytes();
    let n = needle.len();
    if n == 0 || n > hay.len() {
        return;
    }
    // `from` may sit inside a multi-byte char (it is an arithmetic byte offset); that is harmless:
    // a needle (valid UTF-8) can never match starting on a continuation byte, so the offsets we
    // report are always char boundaries.
    let mut i = from;
    while i + n <= hay.len() {
        if out.len() >= MATCH_CAP {
            return; // bounded (finding 6); the GUI shows the count as "N+"
        }
        if hay[i..i + n].eq_ignore_ascii_case(needle) {
            out.push(Match {
                start: i,
                end: i + n,
            });
            i += n; // non-overlapping
        } else {
            i += 1;
        }
    }
}

/// The index range of `matches` that touch the byte window `[from, to)` — i.e. the ones a view
/// rendering only that window needs (review B3). `matches` is sorted and non-overlapping, so this is
/// two binary searches, not a walk over every match in the file.
pub fn matches_in_window(matches: &[Match], from: usize, to: usize) -> std::ops::Range<usize> {
    let lo = matches.partition_point(|m| m.end <= from);
    let hi = matches.partition_point(|m| m.start < to);
    lo..hi.max(lo)
}

/// Advance the current-match index with WRAP. `count` is the number of matches; `forward` picks
/// next (true) vs prev (false). Returns the new index, or `None` when there are no matches. A `cur`
/// of `None` (nothing selected yet) starts at the first match going forward, the last going back.
pub fn step_match(cur: Option<usize>, count: usize, forward: bool) -> Option<usize> {
    if count == 0 {
        return None;
    }
    Some(match (cur, forward) {
        (None, true) => 0,
        (None, false) => count - 1,
        (Some(i), true) => (i + 1) % count,
        (Some(i), false) => (i + count - 1) % count,
    })
}

/// Clamp a current-match index to a (possibly changed) match count. After the text grows or the query
/// changes, the old index may be out of range; this keeps it valid (or `None` when there are no
/// matches). Used so a live-appending tail doesn't panic or point past the end when new matches
/// arrive or the query is edited.
pub fn clamp_current(cur: Option<usize>, count: usize) -> Option<usize> {
    match (cur, count) {
        (_, 0) => None,
        (None, _) => None,
        (Some(i), c) if i < c => Some(i),
        (Some(_), c) => Some(c - 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_finds_nothing() {
        assert!(find_matches("anything", "").is_empty());
    }

    // --- .i55 (search debounce, DECISIONS R162) ---

    #[test]
    fn scan_inline_below_threshold_defers_at_or_above() {
        // Small buffers scan inline (instant); at/above the 4 MB threshold they defer.
        assert!(scan_inline(0));
        assert!(scan_inline(1024));
        assert!(scan_inline(DEBOUNCE_THRESHOLD_BYTES - 1));
        assert!(!scan_inline(DEBOUNCE_THRESHOLD_BYTES)); // boundary: at the threshold, defer
        assert!(!scan_inline(DEBOUNCE_THRESHOLD_BYTES + 1));
        assert!(!scan_inline(64 * 1024 * 1024));
    }

    #[test]
    fn rescan_is_due_only_at_or_after_the_deadline_and_never_when_unarmed() {
        assert!(!rescan_due(None, 100)); // nothing armed
        assert!(!rescan_due(Some(102), 100)); // before deadline
        assert!(!rescan_due(Some(102), 101));
        assert!(rescan_due(Some(102), 102)); // exactly at
        assert!(rescan_due(Some(102), 103)); // past
    }

    #[test]
    fn a_keystroke_burst_over_the_threshold_collapses_to_one_scan() {
        // Model the GUI arming logic purely: each keystroke into a large buffer RE-ARMS the deadline
        // to now + DEBOUNCE_TICKS; a scan runs only when a tick reaches an un-superseded deadline.
        // Five rapid keystrokes (ticks 0..5, one per tick) then a pause: exactly ONE scan fires.
        let mut deadline: Option<u64> = None;
        let mut scans = 0u32;
        // Keystrokes arrive faster than the debounce window: at ticks 0,1,2,3,4 (each < prior+2).
        for t in 0..5u64 {
            // A tick first: would a *previously* armed rescan be due? (It won't be — each keystroke
            // re-armed it two ticks out, and the next keystroke arrives one tick later.) The re-arm
            // below supersedes the deadline whether or not this fired.
            if rescan_due(deadline, t) {
                scans += 1;
            }
            // Then the keystroke re-arms (large buffer -> defer).
            assert!(!scan_inline(16 * 1024 * 1024));
            deadline = Some(t + DEBOUNCE_TICKS);
        }
        // Typing stops. Ticks advance past the last deadline (armed at tick 4 -> due at tick 6).
        for t in 5..8u64 {
            if rescan_due(deadline, t) {
                scans += 1;
                deadline = None;
            }
        }
        assert_eq!(
            scans, 1,
            "a burst of 5 keystrokes must collapse to exactly one scan"
        );
        assert_eq!(
            deadline, None,
            "the deadline is cleared once the single rescan fires"
        );
    }

    #[test]
    fn finds_all_case_insensitive_occurrences() {
        let t = "Error: an error occurred. ERROR again.";
        let m = find_matches(t, "error");
        assert_eq!(m.len(), 3);
        // Ranges index the ORIGINAL text and are query-length wide.
        for r in &m {
            assert_eq!(r.end - r.start, 5);
            assert_eq!(t[r.start..r.end].to_ascii_lowercase(), "error");
        }
        // First is the capitalized "Error" at 0.
        assert_eq!(m[0], Match { start: 0, end: 5 });
    }

    #[test]
    fn matches_are_non_overlapping() {
        // "aa" in "aaaa" yields two matches, not three.
        let m = find_matches("aaaa", "aa");
        assert_eq!(
            m,
            vec![Match { start: 0, end: 2 }, Match { start: 2, end: 4 }]
        );
    }

    #[test]
    fn query_longer_than_text_is_no_match() {
        assert!(find_matches("hi", "hello").is_empty());
    }

    #[test]
    fn step_wraps_forward_and_back() {
        // 3 matches: forward from None -> 0 -> 1 -> 2 -> wrap 0.
        assert_eq!(step_match(None, 3, true), Some(0));
        assert_eq!(step_match(Some(0), 3, true), Some(1));
        assert_eq!(step_match(Some(2), 3, true), Some(0));
        // backward from None -> last; then 2 -> 1 -> 0 -> wrap 2.
        assert_eq!(step_match(None, 3, false), Some(2));
        assert_eq!(step_match(Some(0), 3, false), Some(2));
        // no matches -> None either way.
        assert_eq!(step_match(Some(0), 0, true), None);
        assert_eq!(step_match(None, 0, false), None);
    }

    #[test]
    fn clamp_current_keeps_index_valid() {
        assert_eq!(clamp_current(Some(2), 5), Some(2)); // in range
        assert_eq!(clamp_current(Some(9), 3), Some(2)); // past end -> last
        assert_eq!(clamp_current(Some(0), 0), None); // no matches
        assert_eq!(clamp_current(None, 4), None); // nothing selected
    }

    // --- .i29 (review B3, DECISIONS R97) ---

    #[test]
    fn extend_matches_equals_a_full_rescan_including_a_straddling_match() {
        // Append in pieces, extending incrementally; the result must equal find_matches on the
        // whole text — including a match split across the append boundary ("er" + "ror").
        let full = "Error one\nan er";
        let tail = "ror two\nERROR three\n";
        let mut m = find_matches(full, "error");
        assert_eq!(m.len(), 1);
        let mut text = full.to_string();
        let old = text.len();
        text.push_str(tail);
        extend_matches(&text, "error", old, &mut m);
        assert_eq!(m, find_matches(&text, "error"));
        assert_eq!(m.len(), 3);
        // Sorted + non-overlapping.
        for w in m.windows(2) {
            assert!(w[0].end <= w[1].start);
        }
    }

    #[test]
    fn extend_matches_does_not_double_count_a_match_at_the_old_end() {
        let mut text = "xx error".to_string();
        let mut m = find_matches(&text, "error");
        assert_eq!(m.len(), 1);
        let old = text.len();
        text.push_str(" more");
        extend_matches(&text, "error", old, &mut m);
        assert_eq!(
            m.len(),
            1,
            "the match ending exactly at old_len must not be re-added"
        );
    }

    #[test]
    fn matches_in_window_selects_only_touching_matches() {
        let text = "aa bb aa bb aa bb aa";
        let m = find_matches(text, "aa"); // at 0, 6, 12, 18
        assert_eq!(matches_in_window(&m, 0, 2), 0..1);
        assert_eq!(matches_in_window(&m, 7, 13), 1..3); // touches the one at 6..8 and 12..14
        assert_eq!(matches_in_window(&m, 3, 5), 1..1); // none
        assert_eq!(matches_in_window(&m, 0, text.len()), 0..4);
    }

    #[test]
    fn match_list_is_capped_and_extend_respects_the_cap() {
        // review #2 finding 6: a one-byte query on a big buffer must not allocate one Match per hit
        // without bound. With MATCH_CAP hits already found, neither a rescan nor an extend grows
        // the list.
        let text = "a".repeat(MATCH_CAP + 5_000);
        let mut m = find_matches(&text, "a");
        assert_eq!(m.len(), MATCH_CAP, "scan stops at the cap");
        let old = text.len();
        let mut grown = text.clone();
        grown.push_str("aaaa");
        extend_matches(&grown, "a", old, &mut m);
        assert_eq!(m.len(), MATCH_CAP, "extend does not grow past the cap");
        // Under the cap, everything is still found (the cap is not a truncation of normal use).
        assert_eq!(find_matches(&"ab".repeat(1000), "a").len(), 1000);
    }

    #[test]
    fn find_matches_handles_non_ascii_text_without_panicking() {
        let t = "héllo wörld error élan ERROR";
        let m = find_matches(t, "error");
        assert_eq!(m.len(), 2);
        for r in &m {
            assert!(t.is_char_boundary(r.start) && t.is_char_boundary(r.end));
        }
    }

    #[test]
    fn extend_after_an_append_is_far_cheaper_than_a_rescan() {
        // review B3 evidence: a standing query on a ~5 MB buffer; one 80-byte append. The
        // incremental path must cost a small fraction of the full rescan the old code did per tick.
        let line = "2026-09-09 12:00:00 INFO a routine line with no hits in it at all ok\n";
        let mut text = String::with_capacity(line.len() * 70_000);
        for i in 0..70_000 {
            if i % 1000 == 0 {
                text.push_str("2026-09-09 12:00:00 ERROR something failed here\n");
            } else {
                text.push_str(line);
            }
        }
        let t0 = std::time::Instant::now();
        let mut m = find_matches(&text, "error");
        let full = t0.elapsed();
        let old = text.len();
        text.push_str("2026-09-09 12:00:01 ERROR one more at the end of the file\n");
        let t1 = std::time::Instant::now();
        extend_matches(&text, "error", old, &mut m);
        let incr = t1.elapsed();
        assert_eq!(m, find_matches(&text, "error"));
        println!(
            "B3: full rescan {full:?} vs incremental extend {incr:?} on {} bytes",
            text.len()
        );
        assert!(
            incr * 20 < full.max(std::time::Duration::from_micros(200)),
            "incremental extend ({incr:?}) should be far below a full rescan ({full:?})"
        );
    }
}
