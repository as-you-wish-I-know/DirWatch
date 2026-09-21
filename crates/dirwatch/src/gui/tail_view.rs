//! Tail-window rendering: the virtualized live-tail view, its search bar, and the per-line clip/highlight span builder.
//!
//! Split out of the former single-file `gui.rs` (STYLE BUILD .i49, DECISIONS R153): no
//! behaviour change — the code is the pre-split source verbatim. `use super::*` brings in the
//! shared state, message, constants and helpers that live in the parent [`crate::gui`] module.

use super::*;
// Explicit macro imports disambiguate the `column!`/`row!` macros from the same-named
// `iced::widget` functions once both arrive through the `use super::*` glob (E0659).
use iced::widget::{column, row, stack};

/// A tail window: header (path + encoding + follow state) and the scrolling read-only text.
///
/// VIRTUALIZED (DECISIONS R69, .i11): the whole file lives in `tw.text`, but this only ever renders
/// the lines currently scrolled into view. `responsive` reports the viewport height; `visible_slice`
/// turns (scroll offset, viewport height, fixed line height, line count) into the line band to draw
/// plus the top/bottom spacer heights that stand in for the off-screen lines - so the scrollbar
/// geometry, follow, and scroll-pause are identical to rendering every line, but render cost depends
/// on the viewport, not the file size. This is the freeze fix: a 20 MB log renders ~1 screen of
/// lines, not 20 MB of `text` widget.
pub(super) fn tail_view(id: window::Id, tw: &TailWin) -> Element<'_, Message> {
    let follow = if tw.following { "following" } else { "paused" };
    // §8 (R118): the header never wraps (a long, now-absolute §7 path used to fold onto 2-3 rows in a
    // narrow tail). `Wrapping::None` keeps it one line; a hidden horizontal scrollbar anchored to the
    // END shows the RIGHT of the string — the filename, encoding and follow state stay visible while
    // the long directory prefix is clipped off the LEFT (the user's spec).
    let header = scrollable(
        text(format!(
            "{}   [{}]   {}",
            tw.path,
            if tw.encoding.is_empty() {
                "..."
            } else {
                &tw.encoding
            },
            follow
        ))
        .size(12)
        .color(Color::from_rgb8(0xC8, 0xC8, 0xC8))
        .wrapping(iced::widget::text::Wrapping::None),
    )
    .direction(scrollable::Direction::Horizontal(
        scrollable::Scrollbar::hidden().anchor(scrollable::Anchor::End),
    ))
    .width(Length::Fill);

    // "loading..." until the first chunk arrives from the background reader (a big file may take a
    // moment to start streaming; the window is responsive the whole time). An EMPTY file sends an
    // empty `ready` chunk, which clears `loading`, so it renders blank rather than "loading..."
    // forever (review #2 finding 4).
    if tw.loading && tw.text.is_empty() {
        let body = scrollable(
            container(
                text("loading...")
                    .size(TAIL_TEXT_SIZE)
                    .font(iced::Font::MONOSPACE)
                    .color(Color::from_rgb8(0xEC, 0xEC, 0xEC)),
            )
            .padding(8)
            .width(Length::Fill),
        )
        .id(tw.scroll_id.clone())
        .direction(tail_scroll_direction())
        .on_scroll(move |vp| Message::TailScrolled(id, vp))
        .height(Length::Fill)
        .width(Length::Fill);
        return column![header, body].spacing(6).padding(8).into();
    }

    let total = tw.line_count();
    let scroll_y = tw.scroll_y;
    let scroll_id = tw.scroll_id.clone();
    // `responsive` fills the body area and hands us its size, so `size.height` IS the scrollable's
    // viewport height. We build the scrollable INSIDE it, rendering only the visible slice with
    // spacer widgets above and below. The scroll offset comes from `tw.scroll_y` (updated by
    // `on_scroll`); the one-frame lag between a scroll and the re-slice is what the overscan covers.
    let body = responsive(move |size| {
        let slice = visible_slice(SliceInput {
            scroll_y,
            viewport_h: size.height,
            line_h: TAIL_LINE_H,
            total_lines: total,
            overscan: TAIL_OVERSCAN,
        });
        // Build the visible text as a rich_text so search matches can be HIGHLIGHTED per-character
        // (search, DECISIONS R80). With no active query this is one plain span == the old single
        // `text` widget; with a query it splits the visible slice into before/match/after spans, the
        // CURRENT match tinted stronger than the others. Only the visible slice is spanned, so the
        // virtualization cost is unchanged.
        // §8 (R119): add TAIL_HBAR_H to the bottom spacer so, when snapped to the bottom, the last
        // real line clears the floating horizontal scrollbar (which iced draws over the viewport's
        // bottom edge — see `tail_scroll_direction`). Uniform at every scroll position, so the
        // one-row-per-line virtualization arithmetic in `visible.rs` is unaffected.
        let content = column![
            iced::widget::Space::new().height(Length::Fixed(slice.top_pad)),
            highlighted_slice(tw, slice.first, slice.last),
            iced::widget::Space::new().height(Length::Fixed(slice.bottom_pad + TAIL_HBAR_H)),
        ]
        .width(Length::Fill);

        // Horizontal padding only: vertical padding would shift line 0 off y=0 and desync the
        // spacer arithmetic from the scroll offset. The spacers own the vertical geometry.
        // Both directions (review A4): long lines scroll HORIZONTALLY instead of wrapping, so one
        // logical line is exactly one `TAIL_LINE_H` row and the spacer arithmetic in `visible.rs`
        // is exact — the Win32-edit-control behavior R69 described.
        scrollable(
            container(content)
                .padding(iced::Padding::from([0.0, 8.0]))
                .width(Length::Shrink),
        )
        .id(scroll_id.clone())
        .direction(tail_scroll_direction())
        .on_scroll(move |vp| Message::TailScrolled(id, vp))
        .height(Length::Fill)
        .width(Length::Fill)
        .into()
    });

    // The "jump to bottom & resume following" chip (R173): a small neutral-grey button parked at the
    // foot of the scrollbar (bottom-right of the body), shown ONLY while follow is paused. It drives
    // the existing follow mechanism — `Message::ResumeFollow` snaps to the bottom, which re-engages
    // follow — so it adds no new follow state. When following, the body is shown alone.
    let body_area: Element<'_, Message> = if tw.following {
        body.into()
    } else {
        stack![body, resume_chip(id)].into()
    };

    column![header, body_area, search_bar(id, tw)]
        .spacing(6)
        .padding(8)
        .into()
}

/// Tail scrollable direction: vertical AND horizontal (review A4, DECISIONS R98). With vertical-only
/// scrolling the text was laid out at the viewport width and iced's default `Wrapping::Word` folded
/// every line longer than ~62 monospace characters onto 2-4 visual rows — but `visible.rs` assumes
/// exactly ONE `TAIL_LINE_H` row per logical line, so the scrollbar geometry and the search
/// scroll-to-match were off by however many rows had wrapped. Horizontal scrolling + `Wrapping::None`
/// (in `highlighted_slice`) makes the one-row-per-line invariant true.
fn tail_scroll_direction() -> scrollable::Direction {
    // Both directions are required (no-wrap horizontal + virtualized vertical). NOTE (R119): iced's
    // `scrollable::layout` ignores `Scrollbar::spacing` for `Direction::Both` (only single-direction
    // embedded bars reserve space; scrollable.rs:475-536), so the .i34 `.spacing()` was a no-op and
    // the bars still floated. The newest line is kept clear of the floating horizontal bar by the
    // `TAIL_HBAR_H` bottom spacer in `tail_view` instead. Plain default bars here.
    scrollable::Direction::Both {
        vertical: scrollable::Scrollbar::default(),
        horizontal: scrollable::Scrollbar::default(),
    }
}

/// The always-visible search bar at the bottom of a tail window (search, DECISIONS R80): a query box
/// (Ctrl-F focuses it), prev/next buttons, and the "N of M" match count. Enter in the box = next
/// match (handled in `update` via `EnterPressed` for a focused tail window).
fn search_bar(id: window::Id, tw: &TailWin) -> Element<'_, Message> {
    let count = if tw.search_query.is_empty() {
        String::new()
    } else if tw.search_rescan_due.is_some() {
        // A large-buffer rescan is queued (debounce, R162): the displayed matches belong to the
        // previous query, or were dropped after a trim during the pending window (A1's clear-on-
        // trim fix). Either way the "N of M" would be misleading, so show a pending marker until
        // the deferred scan lands. Branch BEFORE `matches.is_empty()` so a just-cleared pending
        // window reads "…" (scan queued), never a false "no matches" (REVIEW-2026-09-18 A1/A2).
        "\u{2026}".to_string()
    } else if tw.matches.is_empty() {
        "no matches".to_string()
    } else {
        // 1-based "current of total"; if nothing is selected yet show just the total. At the
        // match-list cap the total reads "N+" (review #2 finding 6).
        let total = if tw.matches.len() >= search::MATCH_CAP {
            format!("{}+", search::MATCH_CAP)
        } else {
            tw.matches.len().to_string()
        };
        match tw.current_match {
            Some(i) => format!("{} of {total}", i + 1),
            None => format!("{total} matches"),
        }
    };
    let has = !tw.matches.is_empty();
    // Prev steps the current list only, so it needs matches. Next also FORCES a pending rescan
    // (R165), so it is enabled when a rescan is pending even if the (cleared/previous) match list is
    // empty — clicking it runs the scan for the current query, exactly as Enter does.
    let next_enabled = has || tw.search_rescan_due.is_some();
    let prev = {
        let b = button(text("<").size(13));
        if has {
            b.on_press(Message::SearchPrev(id))
        } else {
            b
        }
    };
    let next = {
        let b = button(text(">").size(13));
        if next_enabled {
            b.on_press(Message::SearchNext(id))
        } else {
            b
        }
    };
    // 'x' clear button: empties the query + highlight. Enabled only when there's something to clear
    // (a query typed); disabled (no on_press) when the box is already empty (DECISIONS R81).
    let clear = {
        let b = button(text("x").size(13));
        if tw.search_query.is_empty() {
            b
        } else {
            b.on_press(Message::SearchCleared(id))
        }
    };
    container(
        row![
            text("Find:")
                .size(12)
                .color(Color::from_rgb8(0xC8, 0xC8, 0xC8)),
            // NO `.on_submit` here (review A3, DECISIONS R97): Enter is routed ONCE by the keyboard
            // subscription (`EnterPressed` -> next match). With `on_submit` too, one Enter stepped
            // TWICE (widget + subscription both fired) — "1 of N" jumped straight to "2 of N".
            text_input("search this file", &tw.search_query)
                .id(tw.search_id.clone())
                .on_input(move |q| Message::SearchChanged(id, q))
                .size(13)
                .line_height(INPUT_LINE_H)
                .width(Length::Fill),
            prev,
            next,
            clear,
            text(count)
                .size(12)
                .color(Color::from_rgb8(0xA6, 0xA6, 0xA6))
                .width(Length::Fixed(90.0)),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center),
    )
    .padding(iced::Padding::from([4.0, 6.0]))
    .style(|theme: &Theme| {
        let p = theme.extended_palette();
        container::Style {
            background: Some(iced::Background::Color(p.background.weak.color)),
            ..container::Style::default()
        }
    })
    .into()
}

/// The "jump to bottom & resume following" chip (R173). A small NEUTRAL-GREY round-ish button that
/// overlays the bottom-right of the body — the foot of the vertical scrollbar — shown only while
/// follow is paused (the caller `stack!`s it over the body in that case). Grey at rest so it reads as
/// scroll furniture and never competes with the amber search highlights or the green follow LED;
/// pressing it sends `ResumeFollow`, which snaps to the bottom and re-engages follow. The bottom
/// padding clears `TAIL_HBAR_H` (the floating horizontal scrollbar iced draws over the viewport's
/// bottom edge) and the right padding clears the vertical bar, so the chip sits just inside them.
fn resume_chip(id: window::Id) -> Element<'static, Message> {
    let chip = button(
        // ↓ down-arrow glyph = "go to the bottom", centered in a fixed 18x18 box.
        container(text("\u{2193}").size(14))
            .center_x(Length::Fixed(18.0))
            .center_y(Length::Fixed(18.0)),
    )
    .padding(0)
    .on_press(Message::ResumeFollow(id))
    .style(|_theme: &Theme, status| {
        // Neutral grey, lifting a step on hover/pressed. Fixed dark-window palette (the tail window
        // is dark regardless of desktop theme), so these are literals like the rest of this view.
        let (bg, border) = match status {
            button::Status::Hovered | button::Status::Pressed => (
                Color::from_rgb8(0x52, 0x58, 0x62),
                Color::from_rgb8(0x6A, 0x71, 0x7C),
            ),
            _ => (
                Color::from_rgb8(0x40, 0x45, 0x4E),
                Color::from_rgb8(0x56, 0x5C, 0x66),
            ),
        };
        button::Style {
            background: Some(iced::Background::Color(bg)),
            text_color: Color::from_rgb8(0xDF, 0xE3, 0xE8),
            border: iced::Border {
                color: border,
                width: 1.0,
                radius: 10.0.into(),
            },
            ..button::Style::default()
        }
    });
    // Fill the body area and pin the chip to the bottom-right, just inside the two scrollbars: the
    // bottom padding clears the floating horizontal bar (TAIL_HBAR_H), the right padding the vertical.
    container(chip)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(iced::alignment::Horizontal::Right)
        .align_y(iced::alignment::Vertical::Bottom)
        .padding(iced::Padding {
            top: 0.0,
            right: TAIL_HBAR_H + 2.0,
            bottom: TAIL_HBAR_H + 2.0,
            left: 0.0,
        })
        .into()
}

/// Colors for the search highlight (search, DECISIONS R80). The CURRENT match gets a saturated orange
/// background; every OTHER match a dimmer amber. Both keep dark text so the term stays legible.
fn highlight_bg(is_current: bool) -> Color {
    if is_current {
        Color::from_rgb8(0xF2, 0x8C, 0x28) // current: bright orange
    } else {
        Color::from_rgb8(0x6E, 0x5A, 0x2A) // other matches: dim amber
    }
}

/// Most bytes of ONE logical line that are rendered (review #1 B12, closed at .i33 — DECISIONS
/// R115). The virtualized view bounds ROWS (`visible.rs`); this bounds COLUMNS. A line with no
/// terminator — a zero-filled or single-line 300 MB file, a minified dump — used to be handed to
/// iced whole: shaping a 48 MB line pegged the GUI thread and grew RSS past 3 GB (reproduced under
/// xvfb; the user's item-5 "Not Responding"). 8 KB is ~40 screens of monospace at the tail width. The
/// rest of the line is summarised by an inline marker, so the row count is unchanged.
const MAX_RENDER_LINE_BYTES: usize = 8192;

/// PURE: the first `max` bytes of `line`, cut back to a char boundary, and how many bytes were cut.
/// A line at or under the bound comes back whole with `0` cut.
fn clip_line(line: &str, max: usize) -> (&str, usize) {
    if line.len() <= max {
        return (line, 0);
    }
    let mut cut = max;
    while cut > 0 && !line.is_char_boundary(cut) {
        cut -= 1;
    }
    (&line[..cut], line.len() - cut)
}

/// One rendered piece of the visible band — the pure output of [`render_pieces`], so the span
/// construction (highlight + clip) is unit-testable without iced.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    /// Body text (may span several lines).
    Plain(String),
    /// A search hit; `current` = the selected match (stronger tint).
    Hit { text: String, current: bool },
    /// The tail of a clipped line: `bytes` were not rendered.
    Clipped { bytes: usize },
}

/// PURE: split the visible slice `visible` (which begins at absolute byte `base` of the buffer) into
/// render pieces: each line clipped to `max_line` bytes (review B12 / R115), search hits from
/// `matches` (absolute byte ranges, sorted, non-overlapping) highlighted where they fall inside the
/// rendered part of a line. Hits beyond a clip are counted by the search bar but not painted (the
/// recorded cost of the bound). Rows are preserved exactly: one `\n` per line boundary, the clip
/// marker inline.
fn render_pieces(
    visible: &str,
    base: usize,
    matches: &[Match],
    cur_start: Option<usize>,
    max_line: usize,
) -> Vec<Piece> {
    let mut pieces: Vec<Piece> = Vec::new();
    let mut plain = String::new();
    let mut off = 0usize; // offset of the current line within `visible`
    for (li, line) in visible.split('\n').enumerate() {
        if li > 0 {
            plain.push('\n');
        }
        let (kept, cut) = clip_line(line, max_line);
        let abs_a = base + off;
        let abs_b = abs_a + kept.len();
        // Only the matches touching this line's rendered part — two binary searches (review B3).
        let window = search::matches_in_window(matches, abs_a, abs_b);
        let mut cursor = 0usize; // within `kept`
        for m in &matches[window] {
            let s = m.start.saturating_sub(abs_a).min(kept.len());
            let mut e = m.end.saturating_sub(abs_a).min(kept.len());
            // A3 (REVIEW-2026-09-18, DECISIONS R166) — DEFENSE IN DEPTH. `render_pieces` is only safe
            // because its caller guarantees `matches` were computed from the CURRENT `text` on char
            // boundaries; A1 (R163) was the path that broke that and panicked here on a mid-char
            // slice. That path is fixed, but any FUTURE drift (a partial refresh, a late off-thread
            // scan, a mis-ordered append) would re-arm the same panic. So never trust the offsets:
            //   * a match whose START lands mid-character is stale/garbage against this text — DROP it
            //     rather than render a highlight that begins inside a glyph (skip on the real start,
            //     per the review's spec), and
            //   * snap the END down to the nearest char boundary at/below it as insurance, so even a
            //     start-valid/end-drifted match can never slice mid-character.
            // Cheap; a no-op on correct offsets, and a substitute for NOTHING — the caller's contract
            // still holds; this only stops a bug elsewhere from becoming a crash here.
            if m.end <= abs_a || s >= kept.len() || !kept.is_char_boundary(s) {
                continue;
            }
            while e > s && !kept.is_char_boundary(e) {
                e -= 1;
            }
            if s >= e {
                continue;
            }
            if cursor < s {
                plain.push_str(&kept[cursor..s]);
            }
            if !plain.is_empty() {
                pieces.push(Piece::Plain(std::mem::take(&mut plain)));
            }
            pieces.push(Piece::Hit {
                text: kept[s..e].to_string(),
                current: Some(m.start) == cur_start,
            });
            cursor = e;
        }
        plain.push_str(&kept[cursor..]);
        if cut > 0 {
            if !plain.is_empty() {
                pieces.push(Piece::Plain(std::mem::take(&mut plain)));
            }
            pieces.push(Piece::Clipped { bytes: cut });
        }
        off += line.len() + 1;
    }
    if !plain.is_empty() {
        pieces.push(Piece::Plain(plain));
    }
    pieces
}

/// Render lines `[first, last)` of a tail as a `rich_text`, highlighting each occurrence of the search
/// query and tinting the CURRENT match more strongly (search, DECISIONS R80), with every line
/// clipped to [`MAX_RENDER_LINE_BYTES`] (R115). Builds spans only for the visible slice, so cost
/// tracks the viewport (the virtualization guarantee, R69) — and, since .i33, never exceeds the
/// viewport's worth of columns either.
fn highlighted_slice(tw: &TailWin, first: usize, last: usize) -> Element<'static, Message> {
    let body = Color::from_rgb8(0xEC, 0xEC, 0xEC);
    let line_h = iced::widget::text::LineHeight::Absolute(TAIL_LINE_H.into());
    let styled = |s: String| -> Span<'static, (), iced::Font> {
        span(s)
            .font(iced::Font::MONOSPACE)
            .size(TAIL_TEXT_SIZE)
            .line_height(line_h)
    };

    let visible = tw.slice_text(first, last);
    // NO WRAPPING (review A4): one logical line == one `TAIL_LINE_H` row, always. Width shrinks to
    // the longest visible line; the scrollable scrolls horizontally for the rest.
    let no_wrap = iced::widget::text::Wrapping::None;
    // The byte offset in `tw.text` where the visible slice begins (== the first line's start), so we
    // can map absolute match offsets into the `visible` substring.
    let base = tw.line_starts.get(first).copied().unwrap_or(0);
    let cur_start = tw
        .current_match
        .and_then(|i| tw.matches.get(i))
        .map(|m| m.start);
    let matches: &[Match] = if tw.search_query.is_empty() {
        &[]
    } else {
        &tw.matches
    };

    let spans: Vec<Span<'static, (), iced::Font>> =
        render_pieces(visible, base, matches, cur_start, MAX_RENDER_LINE_BYTES)
            .into_iter()
            .map(|p| match p {
                Piece::Plain(t) => styled(t).color(body),
                Piece::Hit { text, current } => styled(text)
                    .color(Color::from_rgb8(0x10, 0x10, 0x10))
                    .background(highlight_bg(current)),
                // ASCII marker (was " … " with U+2026): pure ASCII keeps a clipped line on the
                // shaper's fast path, one more guard for the R138 one-row-per-line invariant.
                Piece::Clipped { bytes } => styled(format!(" ... [{bytes} more bytes not shown]"))
                    .color(Color::from_rgb8(0xA6, 0xA6, 0xA6)),
            })
            .collect();
    let rich: Rich<'static, (), Message, Theme> =
        rich_text(spans).wrapping(no_wrap).width(Length::Shrink);
    rich.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_line_keeps_short_lines_and_cuts_long_ones_at_a_char_boundary() {
        assert_eq!(clip_line("short", 8192), ("short", 0));
        assert_eq!(clip_line("", 8192), ("", 0));
        let exact = "x".repeat(8192);
        assert_eq!(clip_line(&exact, 8192), (exact.as_str(), 0));
        let long = "y".repeat(10_000);
        let (kept, cut) = clip_line(&long, 8192);
        assert_eq!(kept.len(), 8192);
        assert_eq!(cut, 10_000 - 8192);
        // A multi-byte char straddling the bound is not split: "é" (2 bytes) at bytes 7..9 → cut at 7.
        let mut s = "a".repeat(7);
        s.push('\u{e9}');
        s.push_str(&"b".repeat(100));
        let (kept, cut) = clip_line(&s, 8);
        assert_eq!(kept, "aaaaaaa");
        assert_eq!(cut, s.len() - 7);
        assert!(s.is_char_boundary(kept.len()));
        // NUL-filled input (a `fsutil createnew` file) is just bytes.
        let zeros = "\0".repeat(20_000);
        let (kept, cut) = clip_line(&zeros, 8192);
        assert_eq!((kept.len(), cut), (8192, 20_000 - 8192));
    }
    #[test]
    fn render_pieces_bounds_every_line_and_keeps_the_row_count() {
        // REGRESSION (item 5 / B12): a 1 MB single line rendered whole pegged the GUI. Now at most
        // `max_line` bytes of it are rendered, plus the inline marker; rows are unchanged.
        let long = "z".repeat(1_000_000);
        let pieces = render_pieces(&long, 0, &[], None, 8192);
        assert_eq!(
            pieces,
            vec![
                Piece::Plain("z".repeat(8192)),
                Piece::Clipped {
                    bytes: 1_000_000 - 8192
                }
            ]
        );
        // 20 lines, one of them long: 20 rows (19 newlines), one marker, nothing else lost.
        let mut text = String::new();
        for i in 0..20 {
            if i == 7 {
                text.push_str(&"L".repeat(9000));
            } else {
                text.push_str(&format!("line {i}"));
            }
            if i < 19 {
                text.push('\n');
            }
        }
        let pieces = render_pieces(&text, 0, &[], None, 8192);
        let rendered: String = pieces
            .iter()
            .map(|p| match p {
                Piece::Plain(t) | Piece::Hit { text: t, .. } => t.clone(),
                Piece::Clipped { .. } => String::new(),
            })
            .collect();
        assert_eq!(rendered.matches('\n').count(), 19, "row count preserved");
        assert_eq!(
            pieces
                .iter()
                .filter(|p| matches!(p, Piece::Clipped { .. }))
                .count(),
            1
        );
        assert!(rendered.contains("line 6\n") && rendered.contains("\nline 8"));
        assert_eq!(rendered.len(), text.len() - (9000 - 8192));
        // Short lines: a single plain piece identical to the input (the old fast path).
        assert_eq!(
            render_pieces("a\nb\nc", 0, &[], None, 8192),
            vec![Piece::Plain("a\nb\nc".to_string())]
        );
    }
    #[test]
    fn render_pieces_highlights_hits_inside_the_rendered_part_only() {
        // Matches are absolute buffer offsets; `base` maps them into the visible slice. A hit past
        // the clip is not painted (counted by the bar, recorded cost); one inside is.
        let visible = "xx err yy\n".to_string() + &"q".repeat(20) + "err" + &"q".repeat(20);
        let base = 100;
        let m = search::find_matches(&visible, "err")
            .into_iter()
            .map(|m| Match {
                start: m.start + base,
                end: m.end + base,
            })
            .collect::<Vec<_>>();
        assert_eq!(m.len(), 2);
        let pieces = render_pieces(&visible, base, &m, Some(m[0].start), 10);
        assert_eq!(
            pieces,
            vec![
                Piece::Plain("xx ".to_string()),
                Piece::Hit {
                    text: "err".to_string(),
                    current: true
                },
                Piece::Plain(" yy\n".to_string() + &"q".repeat(10)),
                Piece::Clipped { bytes: 33 },
            ]
        );
        // Without the clip, the second hit paints too (not current).
        let pieces = render_pieces(&visible, base, &m, Some(m[0].start), 8192);
        assert!(pieces.contains(&Piece::Hit {
            text: "err".to_string(),
            current: false
        }));
    }

    #[test]
    fn render_pieces_skips_a_stale_mid_char_match_instead_of_panicking() {
        // REGRESSION (REVIEW-2026-09-18 finding A3, .i57 guard): render_pieces must never slice on a
        // non-char-boundary even if it is fed stale offsets that don't match the current text. This is
        // exactly the input that panicked the SHIPPED .i55 (review probe 1): visible line "aéb" (é =
        // 0xC3 0xA9, bytes 1..3) with a stale match {start:2, end:3} — start INSIDE é. Before the A3
        // guard this panicked at the `kept[s..e]` slice ("byte index 2 is not a char boundary"). With
        // the guard it must NOT panic, and the mid-char match must be dropped (no Hit piece), leaving
        // the line rendered as plain text.
        let visible = "aéb\n".to_string();
        let stale = vec![Match { start: 2, end: 3 }]; // start bisects é
        let pieces = render_pieces(&visible, 0, &stale, Some(2), 8192);
        // No panic reaching here is the primary assertion. The mid-char match is skipped -> no Hit.
        assert!(
            !pieces.iter().any(|p| matches!(p, Piece::Hit { .. })),
            "a stale mid-char match must be skipped, not highlighted: {pieces:?}"
        );
        // The full line text survives as plain content (concatenated plain pieces == the line).
        let plain: String = pieces
            .iter()
            .filter_map(|p| match p {
                Piece::Plain(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(plain, "aéb\n");
    }

    #[test]
    fn render_pieces_snaps_a_hit_end_off_a_char_boundary_back() {
        // A3 guard, the END-boundary arm: a stale match whose START is a valid boundary but whose END
        // lands mid-char must not slice mid-char either — the end is snapped back to the nearest
        // boundary at/below it. "aéb": a match {start:0, end:2} ends inside é (bytes 1..3); the guard
        // snaps end 2 -> 1, so the hit is just "a" and nothing panics.
        let visible = "aéb\n".to_string();
        let m = vec![Match { start: 0, end: 2 }];
        let pieces = render_pieces(&visible, 0, &m, Some(0), 8192);
        assert!(
            pieces.contains(&Piece::Hit {
                text: "a".to_string(),
                current: true
            }),
            "end snapped to a char boundary -> hit is 'a': {pieces:?}"
        );
    }
}
