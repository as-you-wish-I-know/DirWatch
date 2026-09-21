//! Virtualized tail view: which lines to actually render, and the spacer heights.
//!
//! THE FREEZE THIS KILLS (DECISIONS R68, hardware timing .i10): `tail_view` used to hand iced ONE
//! `text` widget holding the whole file. iced lays out + shapes EVERY line in one frame with no
//! virtualization, so a 10 MB log took ~24 s to render (read+decode was only ~114 ms - render, not
//! read, was the cost). The fix, agreed with the user: keep the WHOLE file in memory (full scrollback,
//! nothing dropped) but only ever RENDER the lines currently scrolled into view - exactly what the
//! old Win32 edit control did. Render cost then depends on the viewport height, not the file size.
//!
//! This module is the LOAD-BEARING math for that, kept PURE (no iced types) so it is UNIT-TESTABLE
//! off-hardware - the same discipline as `placement.rs`. Given the scroll offset, the viewport
//! height, a fixed monospace line height, the total line count, and an overscan, it returns the
//! half-open range of lines to render and the pixel heights of the top/bottom SPACERS that stand in
//! for the off-screen lines. The spacers keep the scrollable's total content height (and therefore
//! the scrollbar geometry and the follow / scroll-pause logic) identical to rendering every line.
//!
//! LINE HEIGHT IS A FIXED CONSTANT (the user's scoping choice 1A, .i11): the tail text is monospace at a
//! fixed size, so every line is the same height; we derive one `TAIL_LINE_H` constant from the font
//! size rather than measuring iced's real metrics each frame. If a DPI/zoom case ever makes the
//! constant drift from iced's actual line box, the symptom is scrollbar drift and the fix is one
//! constant - called out here so it isn't a mystery later.

/// What to render for the current scroll position: the half-open line range `[first, last)` plus the
/// spacer heights (in logical px) that represent the lines above `first` and below `last`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VisibleSlice {
    /// Index of the first line to render (inclusive).
    pub first: usize,
    /// Index one past the last line to render (exclusive). `first..last` is the slice.
    pub last: usize,
    /// Height of the empty top spacer standing in for lines `0..first`.
    pub top_pad: f32,
    /// Height of the empty bottom spacer standing in for lines `last..total`.
    pub bottom_pad: f32,
}

/// Inputs for computing the visible slice of a tail window's text.
#[derive(Debug, Clone, Copy)]
pub struct SliceInput {
    /// Vertical scroll offset in px (distance from the top of the content to the top of the
    /// viewport). This is the `AbsoluteOffset.y` iced reports on scroll.
    pub scroll_y: f32,
    /// The viewport's visible height in px (from `responsive`).
    pub viewport_h: f32,
    /// One line's height in px (fixed constant for the monospace tail font).
    pub line_h: f32,
    /// Total number of lines in the file buffer.
    pub total_lines: usize,
    /// Extra lines to render above and below the strictly-visible band, so a fast scroll doesn't
    /// flash blank before the next frame (the user's choice 2A: ~20).
    pub overscan: usize,
}

/// Compute the slice of lines to render and the spacer heights for the current scroll position.
///
/// The strictly-visible band is `[scroll_y, scroll_y + viewport_h)`; the first visible line is
/// `floor(scroll_y / line_h)` and the last is `ceil((scroll_y + viewport_h) / line_h)`. We widen by
/// `overscan` on each side and clamp to `[0, total_lines]`. Spacers are the remaining lines times
/// `line_h`, so `top_pad + rendered_height + bottom_pad == total_lines * line_h` exactly (the
/// invariant that keeps the scrollbar honest). Degenerate inputs (zero lines, non-positive
/// `line_h`) yield an empty slice with zero pads.
pub fn visible_slice(input: SliceInput) -> VisibleSlice {
    if input.total_lines == 0 || input.line_h <= 0.0 {
        return VisibleSlice {
            first: 0,
            last: 0,
            top_pad: 0.0,
            bottom_pad: 0.0,
        };
    }

    let total = input.total_lines;
    let line_h = input.line_h;

    // Clamp the scroll offset into the valid content range so a transient over-scroll (e.g. a
    // snap-to-end mid-append) can't push `first` past the end.
    let content_h = total as f32 * line_h;
    let max_scroll = (content_h - input.viewport_h).max(0.0);
    let scroll_y = input.scroll_y.clamp(0.0, max_scroll);

    // Strictly-visible band -> line indices.
    let first_visible = (scroll_y / line_h).floor() as isize;
    let bottom = scroll_y + input.viewport_h.max(0.0);
    let last_visible = (bottom / line_h).ceil() as isize;

    // Widen by overscan and clamp to [0, total].
    let over = input.overscan as isize;
    let first = (first_visible - over).max(0) as usize;
    let last = ((last_visible + over).max(0) as usize).min(total);
    // `first` can never exceed `last` after clamping, but guard against it anyway.
    let first = first.min(last);

    let top_pad = first as f32 * line_h;
    let bottom_pad = (total - last) as f32 * line_h;

    VisibleSlice {
        first,
        last,
        top_pad,
        bottom_pad,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LH: f32 = 18.0;

    fn input(scroll_y: f32, total: usize) -> SliceInput {
        SliceInput {
            scroll_y,
            viewport_h: 360.0, // 20 lines tall at LH=18
            line_h: LH,
            total_lines: total,
            overscan: 0, // most tests pin overscan to 0 for exact arithmetic
        }
    }

    /// The spacer invariant: top_pad + rendered lines' height + bottom_pad == total content height,
    /// for ANY scroll position. This is what keeps the scrollbar geometry identical to rendering
    /// every line - the whole point of virtualization.
    fn assert_height_invariant(s: VisibleSlice, total: usize, line_h: f32) {
        let rendered = (s.last - s.first) as f32 * line_h;
        let sum = s.top_pad + rendered + s.bottom_pad;
        let content = total as f32 * line_h;
        assert!(
            (sum - content).abs() < 0.001,
            "height invariant broken: {sum} != {content} (slice {s:?})"
        );
    }

    #[test]
    fn empty_file_renders_nothing() {
        let s = visible_slice(input(0.0, 0));
        assert_eq!(
            s,
            VisibleSlice {
                first: 0,
                last: 0,
                top_pad: 0.0,
                bottom_pad: 0.0
            }
        );
    }

    #[test]
    fn zero_line_height_is_safe() {
        let mut i = input(0.0, 100);
        i.line_h = 0.0;
        let s = visible_slice(i);
        assert_eq!(s.first, 0);
        assert_eq!(s.last, 0);
    }

    #[test]
    fn top_of_a_large_file_renders_only_the_first_screen() {
        // 100k lines, scrolled to the very top, no overscan: render lines [0, 20) (360px / 18px).
        let s = visible_slice(input(0.0, 100_000));
        assert_eq!(s.first, 0);
        assert_eq!(s.last, 20);
        assert_eq!(s.top_pad, 0.0);
        // 99_980 lines below, each 18px.
        assert_eq!(s.bottom_pad, (100_000 - 20) as f32 * LH);
        assert_height_invariant(s, 100_000, LH);
    }

    #[test]
    fn scrolled_into_the_middle_renders_only_that_band() {
        // Scroll down 1000 lines' worth (1000*18 = 18000px) into a 100k-line file.
        let s = visible_slice(input(1000.0 * LH, 100_000));
        assert_eq!(s.first, 1000);
        assert_eq!(s.last, 1020);
        assert_eq!(s.top_pad, 1000.0 * LH);
        assert_eq!(s.bottom_pad, (100_000 - 1020) as f32 * LH);
        assert_height_invariant(s, 100_000, LH);
    }

    #[test]
    fn rendered_line_count_is_bounded_by_viewport_not_file_size() {
        // The freeze-killer property: a 1M-line file renders the same handful of lines as a 100-line
        // file at the same scroll, so render cost is independent of file size.
        let big = visible_slice(input(500.0 * LH, 1_000_000));
        let rendered = big.last - big.first;
        assert!(
            rendered <= 22,
            "rendered {rendered} lines - should be ~viewport (20) + overscan (0), not file-sized"
        );
    }

    #[test]
    fn overscan_widens_the_band_and_clamps_at_the_top() {
        let mut i = input(0.0, 100_000);
        i.overscan = 20;
        let s = visible_slice(i);
        // At the top, the upper overscan clamps to 0 (no negative index); the lower extends by 20.
        assert_eq!(s.first, 0);
        assert_eq!(s.last, 40); // 20 visible + 20 below
        assert_eq!(s.top_pad, 0.0);
        assert_height_invariant(s, 100_000, LH);
    }

    #[test]
    fn overscan_widens_both_sides_in_the_middle() {
        let mut i = input(1000.0 * LH, 100_000);
        i.overscan = 20;
        let s = visible_slice(i);
        assert_eq!(s.first, 1000 - 20);
        assert_eq!(s.last, 1020 + 20);
        assert_height_invariant(s, 100_000, LH);
    }

    #[test]
    fn bottom_of_file_clamps_last_to_total() {
        // Scroll to the very end of a 50-line file (content 900px, viewport 360px -> max scroll 540).
        let s = visible_slice(input(10_000.0, 50)); // way past the end; clamps
        assert_eq!(s.last, 50);
        assert_eq!(s.bottom_pad, 0.0);
        assert_height_invariant(s, 50, LH);
    }

    #[test]
    fn file_shorter_than_viewport_renders_all_lines() {
        // 5 lines, viewport holds 20: render everything, no spacers.
        let s = visible_slice(input(0.0, 5));
        assert_eq!(s.first, 0);
        assert_eq!(s.last, 5);
        assert_eq!(s.top_pad, 0.0);
        assert_eq!(s.bottom_pad, 0.0);
        assert_height_invariant(s, 5, LH);
    }

    #[test]
    fn negative_scroll_is_clamped_to_zero() {
        let s = visible_slice(input(-500.0, 100_000));
        assert_eq!(s.first, 0);
        assert_eq!(s.top_pad, 0.0);
    }
}
