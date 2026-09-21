//! Main-window rendering: the directory/pattern chrome, the directory-box + tile grid, and the status strip.
//!
//! Split out of the former single-file `gui.rs` (STYLE BUILD .i49, DECISIONS R153): no
//! behaviour change — the code is the pre-split source verbatim. `use super::*` brings in the
//! shared state, message, constants and helpers that live in the parent [`crate::gui`] module.

use super::*;
// Explicit macro imports disambiguate the `column!`/`row!` macros from the same-named
// `iced::widget` functions once both arrive through the `use super::*` glob (E0659).
use iced::widget::{column, row};

pub(super) fn main_view(state: &DirWatch) -> Element<'_, Message> {
    let chrome = chrome_rows(state);
    let files = file_area(state);
    let status = status_strip(state);
    column![
        chrome,
        container(files).height(Length::Fill).width(Length::Fill),
        status,
    ]
    .spacing(8)
    .padding(10)
    .into()
}

fn chrome_rows(state: &DirWatch) -> Element<'_, Message> {
    let label = |s: &str| text(s.to_string()).size(14).width(Length::Shrink);
    let row1 = row![
        label("Directory:"),
        text_input("path to watch", &state.dir_input)
            .on_input(Message::DirChanged)
            .size(14)
            .line_height(INPUT_LINE_H)
            .width(Length::Fill),
        label("Depth:"),
        text_input("0", &state.depth_input)
            .on_input(Message::DepthChanged)
            .size(14)
            .line_height(INPUT_LINE_H)
            .width(Length::Fixed(48.0)),
        button(text("Browse...").size(14)).on_press(Message::Browse),
    ]
    .spacing(8)
    .align_y(iced::Alignment::Center);

    let watch_label = if state.runtime.is_some() {
        "Stop Watching"
    } else {
        "Start Watching"
    };
    let row2 = row![
        label("Patterns:"),
        text_input(GlobMatcher::DEFAULT_PATTERNS_STR, &state.patterns_input)
            .on_input(Message::PatternsChanged)
            .size(14)
            .line_height(INPUT_LINE_H)
            .width(Length::Fill),
        button(text("Settings...").size(14)).on_press(Message::OpenSettings),
        button(text("Restart").size(14)).on_press(Message::Restart),
        button(text(watch_label).size(14)).on_press(Message::ToggleWatch),
    ]
    .spacing(8)
    .align_y(iced::Alignment::Center);

    column![row1, row2].spacing(8).into()
}

/// Ordering for two directory boxes given the watched `root` (DECISIONS R72). Pure + case-
/// insensitive so it is UNIT-TESTABLE off-hardware. Rules:
///   * The watched ROOT sorts before every subdirectory (the old DirWatch showed the launched
///     directory first, then subdirs beneath it).
///   * Otherwise, order by the lowercased path. Because the path separator (`\` or `/`) sorts
///     before letters/digits, this yields a DEPTH-FIRST hierarchy: a parent directory sorts
///     immediately before its own children, and siblings come out alphabetical -
///     `aywik` < `aywik\Saved Games` < `aywik\upcheck` < `aywik\upcheck\logs`.
pub(super) fn dir_box_order(root: &str, a: &str, b: &str) -> std::cmp::Ordering {
    let root_l = root.to_lowercase();
    let a_l = a.to_lowercase();
    let b_l = b.to_lowercase();
    // Component-wise `Path` equality (review #2 finding 11): a typed root with a trailing
    // separator (`C:\logs\`, `/var/log/`) never string-equalled the `parent()` of its files, so
    // the root box sorted among its subdirectories instead of first.
    let a_is_root = Path::new(&a_l) == Path::new(&root_l);
    let b_is_root = Path::new(&b_l) == Path::new(&root_l);
    match (a_is_root, b_is_root) {
        (true, true) => std::cmp::Ordering::Equal,
        (true, false) => std::cmp::Ordering::Less, // root first
        (false, true) => std::cmp::Ordering::Greater,
        (false, false) => a_l.cmp(&b_l),
    }
}

fn file_area(state: &DirWatch) -> Element<'_, Message> {
    let now = now_secs();
    // The grouped+sorted box/tile STRUCTURE is cached (finding 1): built in `update` only when the
    // model structure or a layout input changes, not on every frame. Here we only re-read each tile's
    // COLOUR (`vis_of`), which is time-dependent and must be fresh every frame, and clone the cached
    // names/paths into the widget tree iced rebuilds per frame. This turns the per-frame cost from
    // "clone-all + sort (N log N) + group (files × dirs)" into "clone-all + O(1) colour lookups".
    let mut dirs: Vec<(String, Vec<FileTile>)> = Vec::with_capacity(state.tile_layout.boxes.len());
    for b in &state.tile_layout.boxes {
        let files: Vec<FileTile> = b
            .files
            .iter()
            .map(|(name, path)| (name.clone(), path.clone(), state.model.vis_of(path, now)))
            .collect();
        dirs.push((b.dir.clone(), files));
    }

    if dirs.is_empty() {
        let msg = if state.runtime.is_some() {
            "Watching... no matching files yet."
        } else {
            "Stopped. Press Start Watching to begin."
        };
        return container(text(msg).size(14).color(Color::from_rgb8(0xA0, 0xA0, 0xA0)))
            .padding(12)
            .into();
    }

    // Blink phase for the active-closed tiles (step 4b): bright on this tick's half-cycle, dim on
    // the other. Computed once so every active-closed tile pulses in unison (DECISIONS R71).
    let bright = blink::is_bright(state.tick_count);
    let cols = state.settings.tiles_per_box;
    // Per-row packing estimate uses the BASE tile width (R92): individual boxes may render wider when
    // they hold a long filename, but this only sets how many boxes we try per row; iced's responsive
    // layout absorbs the difference. Using the base width keeps normal-name rows packed as before.
    let boxw = dir_box_width(cols, TILE_W);
    let grid = responsive(move |size| {
        let per_row = (((size.width + 10.0) / (boxw + 10.0)).floor() as usize).max(1);
        let mut col = Column::new().spacing(10);
        let mut cur: Row<Message> = Row::new().spacing(10).align_y(iced::Alignment::Start);
        let mut n = 0usize;
        for (dir, files) in &dirs {
            cur = cur.push(dir_box(dir.clone(), files.clone(), bright, cols));
            n += 1;
            if n == per_row {
                col = col.push(cur);
                cur = Row::new().spacing(10).align_y(iced::Alignment::Start);
                n = 0;
            }
        }
        if n > 0 {
            col = col.push(cur);
        }
        scrollable(col)
            .height(Length::Fill)
            .width(Length::Fill)
            .into()
    });

    container(grid)
        .height(Length::Fill)
        .width(Length::Fill)
        .into()
}

fn dir_box(
    dir: String,
    files: Vec<FileTile>,
    bright: bool,
    cols: usize,
) -> Element<'static, Message> {
    let header = text(if dir.is_empty() { ".".to_string() } else { dir })
        .size(13)
        .color(Color::from_rgb8(0xD0, 0xD0, 0xD0));

    // R92: if ANY filename in this box is long, EVERY tile in the box widens (uniform grid, not one
    // ragged oversized tile). Because a widened tile is ~2× the base width, the box also HALVES its
    // columns (min 1), so a long-name box stays roughly the same TOTAL width as a normal box and
    // fits the fixed-width main window instead of overflowing it (the .i27 render caught this — 4
    // doubled tiles ran off the 880px window and hid the very name we were widening to show). The
    // box just gets taller (more rows) instead of wider.
    let wide = files
        .iter()
        .any(|(name, _, _)| name.chars().count() > LONG_NAME_CHARS);
    let tile_w = if wide { WIDE_TILE_W } else { TILE_W };
    let eff_cols = if wide { (cols / 2).max(1) } else { cols };

    let mut col = Column::new().spacing(TILE_GAP);
    let mut cur: Row<Message> = Row::new().spacing(TILE_GAP);
    let mut n = 0usize;
    for (name, path, vis) in &files {
        cur = cur.push(tile(name.clone(), path.clone(), *vis, bright, tile_w));
        n += 1;
        if n == eff_cols {
            col = col.push(cur);
            cur = Row::new().spacing(TILE_GAP);
            n = 0;
        }
    }
    if n > 0 {
        col = col.push(cur);
    }

    container(column![header, col].spacing(6))
        .padding(10)
        .width(Length::Fixed(dir_box_width(eff_cols, tile_w)))
        .style(|theme: &Theme| {
            let p = theme.extended_palette();
            container::Style {
                border: iced::Border {
                    color: p.background.strong.color,
                    width: 1.0,
                    radius: 4.0.into(),
                },
                ..container::Style::default()
            }
        })
        .into()
}

/// One CLICKABLE file tile: a button styled as the colored tile; clicking opens/raises its tail.
/// `bright` is the current blink phase; only an `ActiveClosed` tile uses it (bright/dim amber), so
/// it reads distinct from the same-amber, steady `Unread` tile (step 4b, DECISIONS R71). `tile_w`
/// is the box's per-tile width (R92): `WIDE_TILE_W` when the box has a long name, else `TILE_W`. The
/// label is CLIPPED to the tile and the full name is a hover tooltip, so a name still too long even
/// at the wide width never bleeds into the neighbour.
fn tile(
    name: String,
    path: String,
    vis: ButtonVis,
    bright: bool,
    tile_w: f32,
) -> Element<'static, Message> {
    let bg = tile_fill(vis, bright);
    let fg = Color::from_rgb8(0x10, 0x10, 0x10);
    let full = name.clone();
    let label = text(name).size(13).color(fg).width(Length::Fill).center();
    let btn = button(label)
        .on_press(Message::TileClicked(path))
        .width(Length::Fixed(tile_w))
        .height(Length::Fixed(TILE_H))
        .padding(4)
        // Clip the label to the button rect so an over-long name is cut at the edge, never painted
        // past it into the next tile (R92; the whole reason for this change).
        .clip(true)
        .style(move |_theme, _status| button::Style {
            background: Some(iced::Background::Color(bg)),
            text_color: fg,
            border: iced::Border {
                color: Color::from_rgb8(0x80, 0x80, 0x80),
                width: 1.0,
                radius: 4.0.into(),
            },
            ..button::Style::default()
        });
    // Full filename on hover, so a clipped name is always recoverable (R92).
    let tip = container(text(full).size(13))
        .padding(6)
        .style(|theme: &Theme| {
            let p = theme.extended_palette();
            container::Style {
                background: Some(iced::Background::Color(p.background.weak.color)),
                border: iced::Border {
                    color: p.background.strong.color,
                    width: 1.0,
                    radius: 4.0.into(),
                },
                ..container::Style::default()
            }
        });
    iced::widget::tooltip(btn, tip, iced::widget::tooltip::Position::Top).into()
}

fn status_strip(state: &DirWatch) -> Element<'_, Message> {
    let pat = if state.active_patterns.is_empty() {
        GlobMatcher::DEFAULT_PATTERNS_STR.to_string()
    } else {
        state.active_patterns.join(" ")
    };
    let left = if let Some((_, note)) = &state.note {
        note.clone()
    } else if state.runtime.is_some() {
        format!("watching {} ({})", state.active_dir.display(), pat)
    } else {
        "stopped".to_string()
    };
    let right = format!(
        "Open Windows: {} of {}",
        state.model.open_count(),
        state.max_windows
    );
    // Cap-reached blink (step 4b, DECISIONS R29 item 5 / R71): after an over-cap open attempt (click
    // or auto-open), the COUNT field blinks a bright background, leaving the left status text
    // untouched. This replaces the old transient "cap reached" note (the user's choice 3A). The flash
    // DURATION follows the Active-timeout setting, clamped to [1, 10] s (DECISIONS R80) - so a
    // settings change to Active also changes how long the cap flash lasts, applied live.
    let cap_dur = blink::cap_blink_ms(state.model.active_seconds);
    let cap_blinking = blink::cap_blink_active(state.cap_blink_start_ms, state.now_ms(), cap_dur)
        && blink::is_bright(state.tick_count);
    let count = container(text(right).size(12))
        .padding(iced::Padding::from([1.0, 6.0]))
        .style(move |_theme: &Theme| {
            if cap_blinking {
                // Saturated warning RED with light text for contrast, only on the bright phase
                // (the user, .i12 feedback: red like the old build, not amber - DECISIONS R72). This is
                // a brighter/more urgent red than the muted Missing-tile red (#E06C6C, unchanged) -
                // a cap warning should pop.
                container::Style {
                    background: Some(iced::Background::Color(Color::from_rgb8(0xE0, 0x4C, 0x4C))),
                    text_color: Some(Color::from_rgb8(0xF5, 0xF5, 0xF5)),
                    border: iced::Border {
                        radius: 3.0.into(),
                        ..Default::default()
                    },
                    ..container::Style::default()
                }
            } else {
                container::Style::default()
            }
        });
    container(
        row![text(left).size(12).width(Length::Fill), count,]
            .spacing(12)
            .align_y(iced::Alignment::Center)
            .width(Length::Fill),
    )
    .padding([6, 12])
    .width(Length::Fill)
    .style(|theme: &Theme| {
        let p = theme.extended_palette();
        container::Style {
            background: Some(iced::Background::Color(p.background.weak.color)),
            ..container::Style::default()
        }
    })
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sort a list of directory paths with dir_box_order and return them.
    fn sorted_dirs(root: &str, mut dirs: Vec<&str>) -> Vec<String> {
        dirs.sort_by(|a, b| dir_box_order(root, a, b));
        dirs.into_iter().map(|s| s.to_string()).collect()
    }
    #[test]
    fn watched_root_sorts_first_even_with_a_trailing_separator() {
        // review #2 finding 11: `dirwatch C:\logs\` (tab-completion adds the separator) never
        // string-equalled the files' `parent()`; Path equality is component-wise.
        let got = sorted_dirs(
            r"c:\users\aywik\",
            vec![
                r"c:\users\aywik\apple",
                r"c:\users\aywik",
                r"c:\users\aywik\zebra",
            ],
        );
        assert_eq!(got[0], r"c:\users\aywik");
        let got = sorted_dirs("/var/log/", vec!["/var/log/apt", "/var/log", "/var/log/zz"]);
        assert_eq!(got[0], "/var/log");
    }
    #[test]
    fn watched_root_sorts_first() {
        let root = r"c:\users\aywik";
        let got = sorted_dirs(
            root,
            vec![
                r"c:\users\aywik\Saved Games",
                r"c:\users\aywik",
                r"c:\users\aywik\upcheck",
            ],
        );
        // The root comes first regardless of where it fell alphabetically among the subdirs.
        assert_eq!(got[0], r"c:\users\aywik");
    }
    #[test]
    fn subdirs_are_hierarchical_depth_first() {
        // A parent sorts immediately before its own children; siblings alphabetical. This is the
        // exact case the user reported: aywik first, then Saved Games, then upcheck (and upcheck\logs
        // right after upcheck).
        let root = r"c:\users\aywik";
        let got = sorted_dirs(
            root,
            vec![
                r"c:\users\aywik\upcheck\logs",
                r"c:\users\aywik\Saved Games",
                r"c:\users\aywik",
                r"c:\users\aywik\upcheck",
            ],
        );
        assert_eq!(
            got,
            vec![
                r"c:\users\aywik".to_string(),
                r"c:\users\aywik\Saved Games".to_string(),
                r"c:\users\aywik\upcheck".to_string(),
                r"c:\users\aywik\upcheck\logs".to_string(),
            ]
        );
    }
    #[test]
    fn dir_order_is_case_insensitive() {
        let root = r"C:\Users\Aywik";
        // Root matches regardless of case; subdirs order case-insensitively.
        let got = sorted_dirs(
            root,
            vec![
                r"c:\users\aywik\Zebra",
                r"c:\users\aywik",
                r"c:\users\aywik\apple",
            ],
        );
        assert_eq!(got[0], r"c:\users\aywik"); // root first despite case difference
        assert_eq!(got[1], r"c:\users\aywik\apple"); // apple before Zebra (case-insensitive)
        assert_eq!(got[2], r"c:\users\aywik\Zebra");
    }
    #[test]
    fn a_new_dir_lands_in_sorted_position_not_appended() {
        // The re-sort requirement (the user): a directory that appears later must slot into its sorted
        // place, not tack onto the end. Simulate by sorting a set that includes a "late" dir which
        // belongs in the middle.
        let root = r"c:\w";
        let got = sorted_dirs(root, vec![r"c:\w\zzz", r"c:\w", r"c:\w\mmm"]);
        // mmm sorts before zzz even though (in the unsorted input) zzz was seen first.
        assert_eq!(
            got,
            vec![
                r"c:\w".to_string(),
                r"c:\w\mmm".to_string(),
                r"c:\w\zzz".to_string()
            ]
        );
    }

    // --- step 5: Settings commit mapping (pure, DECISIONS R79) ---
    // `commit_settings` parses + clamps the staged draft strings into a committed `Settings`. These
    // pin the clamps to the CLI/const bounds so the two entry points can't drift, and pin the
    // "unparseable field keeps the previous value" rule (a typo never silently rewrites a good value).
    #[test]
    fn dir_box_grouping_folds_case_like_the_cap_count() {
        // review #3 finding 5 (R124): the GUI groups directory boxes by the SHARED fs_key, exactly as
        // session::dir_key COUNTS them for MAX_DIR_BOXES. So two casings of one directory produce ONE
        // box on a case-insensitive fs (matching one cap count), and TWO on a case-sensitive fs
        // (matching two cap counts) — the two can never disagree. This asserts the predicate the
        // grouping in `file_area` uses: `fs_key_str(a) == fs_key_str(b)`.
        // Forward slashes so the directory SEPARATOR is honoured on every host (a `\` is not a
        // separator on Linux); only the CASE varies here, which is the axis this fix is about.
        let a = fs_key_str("/w/Logs");
        let b = fs_key_str("/w/logs");
        // And confirm the model counts these the same way, so display and cap agree.
        let mut m = dirwatch_core::session::SessionModel::new();
        m.get_or_add("/w/Logs/a.log");
        m.get_or_add("/w/logs/b.log");
        if dirwatch_core::fskey::fs_is_case_insensitive() {
            assert_eq!(a, b, "one box: casings fold together");
            assert_eq!(
                m.dir_count(),
                1,
                "cap counts one dir; GUI must draw one box"
            );
        } else {
            assert_ne!(a, b, "two boxes: casings are distinct files");
            assert_eq!(
                m.dir_count(),
                2,
                "cap counts two dirs; GUI must draw two boxes"
            );
        }
    }

    // ---- §7: resolve_watch_dir (DECISIONS R118) ---------------------------------------------
}
