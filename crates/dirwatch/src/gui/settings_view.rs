//! Settings-window rendering: the five live-tunable fields, the poll-interval note, and the color legend.
//!
//! Split out of the former single-file `gui.rs` (STYLE BUILD .i49, DECISIONS R153): no
//! behaviour change — the code is the pre-split source verbatim. `use super::*` brings in the
//! shared state, message, constants and helpers that live in the parent [`crate::gui`] module.

use super::*;
// Explicit macro imports disambiguate the `column!`/`row!` macros from the same-named
// `iced::widget` functions once both arrive through the `use super::*` glob (E0659).
use iced::widget::{column, row};

/// The Settings window (step 5, DECISIONS R79). Re-expresses the retired NWG `settings_window.rs`
/// (R27/R28a) in iced: the five live tunables, the P1 poll-interval FALLBACK-reconcile note, and the
/// P2 color legend whose swatches read `vis_color` (SAME source as the tiles, so they can't drift)
/// with the active-closed swatch BLINKING off the same `blink.rs` phase as the tiles (R28a intent -
/// active-closed and unread share the amber, blink is the only difference, so the legend itself
/// demonstrates it). OK / Cancel buttons; Enter=OK, Esc=Cancel come from the keyboard subscription.
pub(super) fn settings_view<'a>(state: &'a DirWatch, d: &'a SettingsDraft) -> Element<'a, Message> {
    // One labelled numeric field row: fixed-width label, small numeric box, trailing hint.
    let field = |label: &str, value: &str, hint: &str, on: fn(String) -> Message| {
        row![
            text(label.to_string()).size(14).width(Length::Fixed(150.0)),
            text_input("", value)
                .on_input(on)
                .size(14)
                .line_height(INPUT_LINE_H)
                .width(Length::Fixed(80.0)),
            text(hint.to_string())
                .size(12)
                .color(Color::from_rgb8(0x9A, 0x9A, 0x9A)),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center)
    };

    // Field order (DECISIONS R80): Max windows, Active timeout, Max tile width, Auto-open, then Poll
    // interval placed LAST - directly above its P1 note - so the poll control sits next to its
    // explainer text (the user's .i17 feedback). "Tiles per box" was renamed to "Max tile width" (label
    // only; the value is still the 2-10 tiles-across count).
    let fields = column![
        field(
            "Max windows:",
            &d.max_windows,
            "1-50",
            Message::SettingsMaxWindowsChanged
        ),
        field(
            "Active timeout (s):",
            &d.active_seconds,
            "min 1",
            Message::SettingsActiveSecsChanged
        ),
        field(
            "Max tile width:",
            &d.tiles_per_box,
            "2-10",
            Message::SettingsTilesPerBoxChanged
        ),
        row![
            text("Auto-open on activity:")
                .size(14)
                .width(Length::Fixed(150.0)),
            // The stored flag is `no_open` (suppress); the checkbox reads as "auto-open enabled", so
            // it shows `!no_open` and toggling it sets `no_open` to the inverse.
            checkbox(!d.no_open).on_toggle(|on| Message::SettingsNoOpenToggled(!on)),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center),
    ]
    .spacing(10);

    // Poll interval sits on its own, placed just above the P1 note (the reorder above).
    let poll_field = field(
        "Poll interval (ms):",
        &d.poll_ms,
        "min 100",
        Message::SettingsPollMsChanged,
    );

    // P1: the poll-interval FALLBACK-reconcile note (BACKLOG §0 P1 / DECISIONS R14). An IN-UI
    // explanation, not a value change - the user's preferred answer to "why 250 ms".
    let p1 = container(
        text(
            "Poll interval is a FALLBACK reconcile. The filesystem watcher is the primary, \
             real-time detector; the poll catches events missed on network / UNC shares. Default \
             250 ms, floor 100 ms. Deeper or larger trees on a network share cost more to poll - \
             keep Depth conservative there.",
        )
        .size(12)
        .color(Color::from_rgb8(0xB8, 0xB8, 0xB8)),
    )
    .padding(8)
    .style(|theme: &Theme| {
        let p = theme.extended_palette();
        container::Style {
            background: Some(iced::Background::Color(p.background.weak.color)),
            border: iced::Border {
                radius: 4.0.into(),
                ..Default::default()
            },
            ..container::Style::default()
        }
    });

    // P2: color legend (BACKLOG §0 P2 / DECISIONS R28a). Swatches read `vis_color`; the active-closed
    // swatch blinks off the SAME phase as the tiles so it never drifts. Unread shares that amber but
    // stays steady - the legend demonstrates that blink is the only difference.
    let bright = blink::is_bright(state.tick_count);
    let legend = column![
        text("Legend").size(14),
        legend_row(ButtonVis::Idle, true, "idle - present, no window"),
        legend_row(ButtonVis::Open, true, "open - window showing"),
        legend_row(
            ButtonVis::ActiveOpen,
            true,
            "active & open - receiving writes"
        ),
        legend_row(
            ButtonVis::ActiveClosed,
            bright,
            "active, closed - blinks (writing, window closed)"
        ),
        legend_row(
            ButtonVis::Unread,
            true,
            "unread - changed since closed (steady)"
        ),
        legend_row(ButtonVis::Missing, true, "missing - deleted / rotated"),
    ]
    .spacing(6);

    let buttons = row![
        iced::widget::Space::new().width(Length::Fill),
        button(text("Cancel").size(14)).on_press(Message::SettingsCancel),
        button(text("OK").size(14)).on_press(Message::SettingsOk),
    ]
    .spacing(8);

    let content = column![fields, poll_field, p1, legend, buttons]
        .spacing(16)
        .padding(16);
    scrollable(content).height(Length::Fill).into()
}

/// One legend row: a colored swatch (via `tile_fill`, so the active-closed swatch honors the blink
/// phase exactly like a tile) + its description. `bright` only matters for `ActiveClosed`; every
/// other state renders its steady `vis_color`.
fn legend_row(vis: ButtonVis, bright: bool, label: &str) -> Element<'static, Message> {
    let swatch = container(text("").size(1))
        .width(Length::Fixed(26.0))
        .height(Length::Fixed(16.0))
        .style(move |_theme: &Theme| container::Style {
            background: Some(iced::Background::Color(tile_fill(vis, bright))),
            border: iced::Border {
                color: Color::from_rgb8(0x80, 0x80, 0x80),
                width: 1.0,
                radius: 3.0.into(),
            },
            ..container::Style::default()
        });
    row![
        swatch,
        text(label.to_string())
            .size(13)
            .color(Color::from_rgb8(0xCC, 0xCC, 0xCC)),
    ]
    .spacing(10)
    .align_y(iced::Alignment::Center)
    .into()
}
