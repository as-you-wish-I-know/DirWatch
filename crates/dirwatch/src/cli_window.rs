//! CLI-output window (Windows CLI text goes to a GUI window, not the console).
//!
//! WHY THIS EXISTS (DECISIONS R91, .i26): on pre-24H2 Windows a single binary CANNOT be both a
//! GUI app (no console window on double-click, never holds the shell) and a console app whose CLI
//! output (`--help`/`--version`/…) appears reliably in cmd.exe — the two behaviors are the two
//! opposite meanings of the ONE subsystem bit, chosen at link time, and "the console subsystem
//! cannot influence [the shell's wait] decision" (Microsoft Terminal spec #7335). Every scripting
//! runtime shipped TWO binaries to get both (python/pythonw, …); Microsoft only unified them with
//! the 24H2 `consoleAllocationPolicy` manifest, which does not exist on the user's 23H2.
//!
//! So (per the user's fallback instruction, 2026-09-07) DirWatch stays a pure GUI-subsystem app — the
//! ONLY design that guarantees no console window ever (no flash, the hard requirement) and never
//! holds the terminal — and the CLI text is shown in a small iced WINDOW instead of the console.
//! A window is shell-independent: it works identically from cmd.exe, PowerShell, and a double-click.
//! No `AttachConsole`, no console FFI, no subsystem gymnastics — that whole class of problem is
//! deleted, not managed.
//!
//! This is Windows-only behavior. macOS/Linux have no subsystem problem, so `main` prints their CLI
//! output to the terminal as before and never calls this (the user's choice 2, .i26).
//!
//! Implementation: a minimal single-window `iced::application` (NOT the multi-window `daemon` the
//! main GUI uses). DirWatch runs exactly one iced event loop per process — `decide_launch` picks the
//! CLI window OR the main GUI, never both — so there is no conflict. Default dark theme + the app
//! icon, to match the rest of the app. The text is selectable + scrollable (the user's choice 1: an
//! iced text window, so help is copy-pasteable), monospace so the help's aligned columns line up.
//! Closing the window (Esc / Ctrl-W / Cmd-W, or the title-bar button) ends the process.

use crate::gui::app_icon;
use iced::widget::{column, text_editor};
use iced::{event, keyboard, window, Element, Font, Length, Size, Subscription, Task, Theme};

/// State for the CLI-output window: the text lives in a `text_editor::Content` so it is SELECTABLE
/// and COPYABLE (Ctrl-C) — the .i27 fix (R92). The editor is made read-only in `update` by dropping
/// edit actions, so the user can select/copy/scroll but not modify the text.
struct CliWindow {
    content: text_editor::Content,
}

#[derive(Debug, Clone)]
enum Message {
    /// A text_editor action (cursor move, selection, click/drag, scroll, copy). Non-edit actions are
    /// applied; edit actions are ignored, making the field read-only.
    Edit(text_editor::Action),
    /// Esc / Ctrl-W / Cmd-W pressed — close this window id (which ends the single-window app). The
    /// id rides on the message so the very first key (even Esc) can close without a prior event.
    Close(window::Id),
}

/// Show `body` in a small dark iced window and block until the user closes it. Returns iced's
/// result so `main` can surface a launch failure the same way the main GUI does. Windows-only
/// caller (see the module docs); compiles everywhere (it is just iced, no platform code).
pub fn show(title: String, body: String) -> iced::Result {
    let boot_body = body;
    iced::application(
        move || {
            (
                CliWindow {
                    content: text_editor::Content::with_text(&boot_body),
                },
                Task::none(),
            )
        },
        update,
        view,
    )
    // `.title` accepts a `&'static str` directly. The title is a tiny fixed string and this process
    // shows exactly one window then exits, so leaking it to 'static is fine and avoids a capturing
    // title closure (which trips iced's higher-ranked Fn bounds).
    .title(&*Box::leak(title.into_boxed_str()))
    .theme(theme)
    .window(window::Settings {
        icon: app_icon(),
        // A comfortable default for help text; resizable (the iced default) so a long `--help`
        // can be enlarged.
        size: Size::new(680.0, 520.0),
        ..Default::default()
    })
    .subscription(subscription)
    .run()
}

/// Fixed dark theme (matches the main GUI, §7). A named `fn` (not an inline closure) so iced's
/// higher-ranked `Fn(&State) -> Theme` bound infers correctly.
fn theme(_state: &CliWindow) -> Theme {
    Theme::Dark
}

fn update(state: &mut CliWindow, message: Message) -> Task<Message> {
    match message {
        Message::Edit(action) => {
            // READ-ONLY: apply selection / cursor / scroll / copy, but DROP edit actions (typing,
            // paste, delete) so the help text can be selected and copied but never modified.
            if !action.is_edit() {
                state.content.perform(action);
            }
            Task::none()
        }
        // Closing the sole window ends the single-window application.
        Message::Close(id) => window::close(id),
    }
}

fn view(state: &CliWindow) -> Element<'_, Message> {
    // A read-only text_editor (see `update`): monospace so the help's DESC_COL-aligned columns (see
    // cli.rs) line up, selectable + Ctrl-C copyable (the .i27 fix), and it scrolls internally.
    let editor = text_editor(&state.content)
        .font(Font::MONOSPACE)
        .size(14)
        .padding(16)
        .height(Length::Fill)
        .on_action(Message::Edit);

    column![editor]
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Esc, Ctrl-W, or Cmd-W closes the window. Uses `event::listen_with` (like the main GUI, gui.rs)
/// so each key event carries the window id — we stash it via `Seen` and use it to close on `Close`.
/// `control() || logo()` matches the tail-window close idiom (R89): Cmd-W works on macOS too, though
/// this window is Windows-only in practice.
fn subscription(_state: &CliWindow) -> Subscription<Message> {
    event::listen_with(|ev, _status, id| match ev {
        event::Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) => {
            use keyboard::key::Named;
            match key {
                keyboard::Key::Named(Named::Escape) => Some(Message::Close(id)),
                keyboard::Key::Character(ref c)
                    if c.as_str().eq_ignore_ascii_case("w")
                        && (modifiers.control() || modifiers.logo()) =>
                {
                    Some(Message::Close(id))
                }
                _ => None,
            }
        }
        _ => None,
    })
}
