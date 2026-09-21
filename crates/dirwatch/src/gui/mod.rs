//! DirWatch GUI - iced (cross-platform: Windows, macOS, Linux).
//!
//! This module is the iced multi-window `daemon` (PORT-PLAN-crossplatform §6 step 4, where the GUI
//! moved off iced's single-window `application`): the application state, the `Message` enum, the
//! `update`/`view`/`subscription` wiring, the tail text engine, and the shared constants and helpers.
//! Per-window RENDERING lives in three child modules split out at STYLE BUILD .i49 (DECISIONS R153):
//! `main_view`, `tail_view`, and `settings_view`. Each `use super::*`s this module, so the
//! shared state and helpers here are their single source; `view()` below dispatches to the three
//! per-window entry points.
//!
//! Window model:
//!
//!   * The MAIN window is one `window::Id`; closing it exits the app.
//!   * Clicking a file tile opens that file's live TAIL in a SEPARATE OS window (draggable, its own
//!     taskbar entry) - `SessionModel::try_open` gates it against the cap; `AlreadyOpen` raises the
//!     existing window to the front; `BlockedByCap` blinks the count.
//!   * A tail window shows the whole file from where tailing began, follows the tail (auto-scroll to
//!     bottom), and PAUSES follow when you scroll up - resuming when you return to the bottom. It
//!     shows the encoding label and rotation / missing / reappear markers, all from the proven core
//!     `TailReader`.
//!   * The first tail window is placed EDGE-AWARE (toward the screen side with room) and subsequent
//!     ones CASCADE with wrap (`placement::next_position`, unit-tested).
//!   * Closing a tail window (its X) frees the cap slot (`mark_closed`) and recolors the tile, via a
//!     `window::close_events` subscription. `Open Windows: N of M` reflects the real open count.
//!   * Auto-open-on-activity (the `wants_open` signal, .i12) and the active-closed blink cadence sit
//!     on top of this.
//!
//! dirwatch-core is UNTOUCHED; tile colors stay on `vis_color`. THEME: iced default dark (§7).

use crate::applog;
use crate::blink;
use crate::placement::{next_position, PlaceInput, Rect};
use crate::runtime::{TailStream, WatchConfig, WatchEvent, WatchRuntime};
use crate::search::{self, Match};
use crate::visible::{visible_slice, SliceInput};
use dirwatch_core::build_info;
use dirwatch_core::fskey::fs_key_str;
use dirwatch_core::glob::GlobMatcher;
use dirwatch_core::session::{ButtonVis, OpenResult, SessionModel};
use dirwatch_core::tail::{SCROLLBACK_CAP_BYTES, SCROLLBACK_TRIM_TO};
use dirwatch_core::watch::MAX_DEPTH;
use iced::keyboard;
use iced::widget::operation::{focus, scroll_to, snap_to, RelativeOffset};
use iced::widget::text::{Rich, Span};
use iced::widget::{
    button, checkbox, container, responsive, rich_text, scrollable, span, text, text_input, Column,
    Row,
};
use iced::window;
use iced::{event, time, Color, Element, Length, Point, Size, Subscription, Task, Theme};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// View rendering is split into three child modules (STYLE BUILD .i49, DECISIONS R153). Each
// `use super::*`s this module's shared state, message, constants and helpers; only the three
// per-window entry points are called from `view()` below, so only they are `pub(super)`.
mod main_view;
mod settings_view;
mod tail_view;
use main_view::main_view;
use settings_view::settings_view;
use tail_view::tail_view;

/// SessionModel knobs carried from the CLI (A1 fix, DECISIONS R53).
#[derive(Debug, Clone, Default)]
pub struct ModelOpts {
    pub no_open: bool,
    pub active_seconds: Option<i32>,
    pub max_windows: Option<i32>,
}

/// The five live tunables the Settings dialog exposes (step 5, DECISIONS R79). These are the
/// COMMITTED values the running app uses right now: `max_windows` / `active_seconds` / `no_open`
/// mirror `SessionModel` fields, `poll_ms` drives the watch runtime, and `tiles_per_box` drives the
/// dir-box layout. The dialog stages edits as strings and commits them here on OK (never on keypress),
/// so a half-typed number never perturbs the running app. No persistence across runs (backlog §1,
/// dropped) — this is in-memory only.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub max_windows: i32,
    pub active_seconds: i64,
    pub no_open: bool,
    pub poll_ms: u32,
    pub tiles_per_box: usize,
}

impl Settings {
    /// Seed the live settings from the launch config + CLI knobs (the values already in effect at
    /// boot), so opening the dialog the first time shows what the app is actually using.
    fn from_boot(cfg: &WatchConfig, opts: &ModelOpts) -> Settings {
        Settings {
            max_windows: opts
                .max_windows
                .unwrap_or(10)
                .clamp(1, dirwatch_core::session::MAX_WINDOWS_CEILING),
            active_seconds: opts.active_seconds.map(|s| s.max(1) as i64).unwrap_or(5),
            no_open: opts.no_open,
            poll_ms: cfg.poll_interval_ms.max(POLL_MS_FLOOR),
            tiles_per_box: TILES_PER_BOX_DEFAULT,
        }
    }
}

/// The Settings window's staged (editable) fields, plus its window id. Present only while the window
/// is open (`DirWatch::settings` is `Some`). The fields are strings so a partially-typed number is
/// legal mid-edit; `commit_settings` parses + clamps them on OK. Seeded from the live [`Settings`]
/// each time the window opens, so Cancel simply drops this and the live values are untouched.
struct SettingsDraft {
    id: window::Id,
    max_windows: String,
    active_seconds: String,
    no_open: bool,
    poll_ms: String,
    tiles_per_box: String,
}

impl SettingsDraft {
    /// Build a draft (string fields) from the live settings, for a freshly-opened window.
    fn from_live(id: window::Id, s: &Settings) -> SettingsDraft {
        SettingsDraft {
            id,
            max_windows: s.max_windows.to_string(),
            active_seconds: s.active_seconds.to_string(),
            no_open: s.no_open,
            poll_ms: s.poll_ms.to_string(),
            tiles_per_box: s.tiles_per_box.to_string(),
        }
    }
}

/// PURE: parse + clamp a [`SettingsDraft`]'s strings into a committed [`Settings`]. Unit-testable
/// off-hardware (DECISIONS R79). Clamps match the CLI parser and the existing constants so the two
/// entry points can't drift: max-windows `[1, MAX_WINDOWS_CEILING]` (R29), active-secs floor 1,
/// poll-ms floor `POLL_MS_FLOOR` (R14/P1), tiles-per-box `[MIN, MAX]` (R64). A field that fails to
/// parse KEEPS its current live value (`prev`) rather than snapping to a default — a typo in one box
/// never silently rewrites a good value the user didn't touch.
fn commit_settings(draft: &SettingsDraft, prev: &Settings) -> Settings {
    let parse_or = |s: &str, cur: i64| s.trim().parse::<i64>().unwrap_or(cur);
    let max_windows = parse_or(&draft.max_windows, prev.max_windows as i64)
        .clamp(1, dirwatch_core::session::MAX_WINDOWS_CEILING as i64) as i32;
    // Ceilings before the narrowing casts (review #2 finding 8): a digits-only field has no length
    // limit, so `4294967396` used to commit as 100 ms via `as u32` wraparound.
    let active_seconds =
        parse_or(&draft.active_seconds, prev.active_seconds).clamp(1, ACTIVE_SECS_CEILING);
    let poll_ms = parse_or(&draft.poll_ms, prev.poll_ms as i64)
        .clamp(POLL_MS_FLOOR as i64, POLL_MS_CEILING as i64) as u32;
    let tiles_per_box = parse_or(&draft.tiles_per_box, prev.tiles_per_box as i64)
        .clamp(TILES_PER_BOX_MIN as i64, TILES_PER_BOX_MAX as i64) as usize;
    Settings {
        max_windows,
        active_seconds,
        no_open: draft.no_open,
        poll_ms,
        tiles_per_box,
    }
}

/// RGB swatch for each button state. SINGLE SOURCE OF TRUTH for the render colors (DECISIONS entry
/// 9 palette, carried verbatim from NWG). A future Settings legend (step 5) reads this.
pub fn vis_color(v: ButtonVis) -> [u8; 3] {
    match v {
        ButtonVis::Idle => [0xE0, 0xE0, 0xE0],
        // Open = window showing, file idle. Deepened from the old #BFD8F0 pale blue to a clearly
        // darker but still legible-with-dark-text blue (the user, .i8 / DECISIONS R66).
        ButtonVis::Open => [0x8F, 0xB8, 0xE0],
        ButtonVis::ActiveOpen => [0x4C, 0xC2, 0x4C],
        ButtonVis::ActiveClosed => [0xF2, 0xC7, 0x4C],
        ButtonVis::Unread => [0xF2, 0xC7, 0x4C],
        ButtonVis::Missing => [0xE0, 0x6C, 0x6C],
    }
}

/// The DIM amber shown on the OFF half of the active-closed blink (DECISIONS R71). The bright amber
/// is `vis_color(ActiveClosed)` = `#F2C74C`; this is a clearly darker amber so the tile visibly
/// pulses, while staying the SAME hue (so it still reads as "amber", distinct from red/green). Only
/// ActiveClosed blinks; every other state renders its solid `vis_color`.
fn vis_color_dim_active_closed() -> [u8; 3] {
    [0x8C, 0x72, 0x2C]
}

/// The tile fill for a state at a given blink phase. All states render their solid `vis_color`
/// EXCEPT `ActiveClosed`, which alternates bright/dim by `bright` so it reads distinct from the
/// same-amber, steady `Unread` (the user's choice 4A: blink is the only difference).
fn tile_fill(v: ButtonVis, bright: bool) -> Color {
    let [r, g, b] = if v == ButtonVis::ActiveClosed && !bright {
        vis_color_dim_active_closed()
    } else {
        vis_color(v)
    };
    Color::from_rgb8(r, g, b)
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The DirWatch window icon (BACKLOG §2b / DECISIONS R80). The original artwork's 256x256 render is
/// decoded to raw RGBA at BUILD time and embedded with `include_bytes!`, so the running binary needs
/// NO image-decoder dependency (the minimal-deps rule): `window::icon::from_rgba` takes pre-decoded
/// RGBA bytes directly. Applied to the main, Settings, and every tail window's `window::Settings.icon`
/// so the taskbar/dock/titlebar shows the folder-with-eye icon on all three OSes. Returns `None` if
/// the embedded bytes are somehow the wrong length (they can't be, but `from_rgba` is fallible) - a
/// missing icon is cosmetic, never fatal. Built per window open; cheap (one Vec copy).
const ICON_RGBA: &[u8] = include_bytes!("../../assets/dirwatch_icon_256.rgba");
const ICON_SIDE: u32 = 256;
pub(crate) fn app_icon() -> Option<window::Icon> {
    window::icon::from_rgba(ICON_RGBA.to_vec(), ICON_SIDE, ICON_SIDE).ok()
}

const TICK_MS: u64 = 100;
/// How long a matched file must be continuously Missing before its tile is pruned from the main
/// window (REVIEW-2026-09-17 finding 1 growth path). One hour (the user's choice): long enough that an
/// ordinary rotation or a transient unmount never drops a tile the user still cares about, short
/// enough that a rotating directory stays bounded and shows only live files. Deliberately generous —
/// the `MAX_FILES` cap is the hard backstop, so the grace can afford to keep tiles around a while.
/// Fixed (no Settings knob this build). Wall-clock seconds, so it is a real hour regardless of the
/// poll interval, and independent of the core's own short `known` prune (which is internal
/// reappearance bookkeeping, not tile lifetime).
const MISSING_PRUNE_GRACE_SECS: i64 = 60 * 60;

/// The Missing-prune grace actually used, in seconds. Normally [`MISSING_PRUNE_GRACE_SECS`] (1 hour),
/// but when `DIRWATCH_DEBUG` is ON, `DIRWATCH_PRUNE_SECS` may override it with a smaller value so a
/// verification run can observe a prune in seconds instead of waiting an hour (`Verify-DirWatchPrune`).
/// The override is IGNORED unless `DIRWATCH_DEBUG` is set, so a normal user can never accidentally
/// shrink the grace; it is clamped to at least 1 s. Checked once and cached.
fn prune_grace_secs() -> i64 {
    use std::sync::OnceLock;
    static SECS: OnceLock<i64> = OnceLock::new();
    *SECS.get_or_init(|| {
        if !trace_enabled() {
            return MISSING_PRUNE_GRACE_SECS;
        }
        match std::env::var("DIRWATCH_PRUNE_SECS")
            .ok()
            .and_then(|v| v.trim().parse::<i64>().ok())
        {
            Some(n) => {
                let n = n.max(1);
                trace_log(&format!(
                    "DIRWATCH_PRUNE_SECS override active: Missing-prune grace = {n}s (default {MISSING_PRUNE_GRACE_SECS}s)"
                ));
                n
            }
            None => MISSING_PRUNE_GRACE_SECS,
        }
    })
}
const TILE_W: f32 = 190.0;
const TILE_H: f32 = 30.0;
const TILE_GAP: f32 = 6.0;

/// Long-filename handling (DECISIONS R92, the user 2026-09-07): a name wider than a tile used to paint
/// past the tile's right edge and collide with its neighbour (the user's .i23 screenshot). Fix: if ANY
/// filename in a directory box is "long", EVERY tile in that box widens to `WIDE_TILE_W` — two tiles
/// plus the gap between them (the user's exact spec), so the box stays a uniform grid rather than one
/// ragged oversized tile. A name still too long even at the wide width is clipped (never bleeds) and
/// its full text is always available via the per-tile tooltip. `LONG_NAME_CHARS` is the trigger: at
/// `size(13)` a ~190px tile fits ~26 chars, so 24 leaves a small margin. Internal constants, not
/// user-settable (minimal-config).
const LONG_NAME_CHARS: usize = 24;
/// A widened tile: exactly two normal tiles plus the gap between them (the user's spec, .i27).
const WIDE_TILE_W: f32 = 2.0 * TILE_W + TILE_GAP;

/// Tiles-per-dir-box width (DECISIONS R64): default 4, clamped [2,10]; Settings-exposed at step 5
/// (DECISIONS R79). The clamp lives in `commit_settings`; call sites read the committed value off
/// `DirWatch::settings.tiles_per_box`. The constants remain the single source of the default + bounds.
const TILES_PER_BOX_DEFAULT: usize = 4;
const TILES_PER_BOX_MIN: usize = 2;
const TILES_PER_BOX_MAX: usize = 10;
/// Poll-interval floor in ms (P1 / DECISIONS R14): the fallback-reconcile poll can't go below this.
/// Both the CLI (`--poll-ms`, via `main.rs`) and the Settings dialog (`commit_settings`) enforce it.
/// Review #2 finding 16 (R118): this is the SHARED core constant, the same one `WatchService::new`
/// and `SweepScheduler::with_debounce` clamp to — one source of truth, no drift.
const POLL_MS_FLOOR: u32 = dirwatch_core::POLL_INTERVAL_FLOOR_MS;
/// Poll-interval ceiling (review #2 finding 8): one minute. Bounds the Settings field before its
/// `as u32` cast so an over-long digit string cannot wrap to a tiny or enormous interval. Since
/// review #4 finding 19 the SHARED core constant, so the CLI (`main.rs`) clamps to the same value.
const POLL_MS_CEILING: u32 = dirwatch_core::POLL_INTERVAL_CEILING_MS;
/// Active-timeout ceiling in seconds (one day) — same reason as `POLL_MS_CEILING`.
const ACTIVE_SECS_CEILING: i64 = 86_400;
/// Width of a directory box holding `cols` tiles each `tile_w` wide (R92: `tile_w` is `WIDE_TILE_W`
/// for a box with a long filename, else `TILE_W`). The +2*10 is the container's L/R padding.
fn dir_box_width(cols: usize, tile_w: f32) -> f32 {
    let cols = cols.max(1) as f32;
    cols * tile_w + (cols - 1.0) * TILE_GAP + 2.0 * 10.0
}

const MAIN_W: f32 = 880.0;
const MAIN_H: f32 = 600.0;
const TAIL_W: f32 = 520.0;
const TAIL_H: f32 = 420.0;
/// Shared line-height for every `text_input` (BACKLOG §10, DECISIONS R135). iced's default is
/// `LineHeight::Relative(1.3)` (`iced_core-0.14.0/src/text.rs:217`); at our 13-14 px input text the
/// text is vertically centered in a 1.3x box, which leaves a lowercase descender (`g`/`y`/`p`/`q`/`j`)
/// touching the box's bottom border, where the border clips it (the user's Linux/RDP `*.log` capture:
/// the `g` cut off at the BOTTOM, not the right). 1.5x gives the descender daylight below the
/// baseline without reflowing the center-aligned rows. One const so the five inputs can't drift.
const INPUT_LINE_H: iced::widget::text::LineHeight = iced::widget::text::LineHeight::Relative(1.5);
/// The Settings window (step 5, DECISIONS R79). Sized to hold the five field rows, the P1 note, and
/// the P2 legend without scrolling on a normal display.
const SETTINGS_W: f32 = 460.0;
const SETTINGS_H: f32 = 560.0;
/// Tail body text size, and the fixed per-line height derived from it (the user's scoping choice 1A,
/// .i11 / DECISIONS R69): the tail text is monospace at a fixed size, so every line is the same
/// height and the virtualized view can size its spacers by pure arithmetic instead of measuring
/// iced's real metrics each frame. `TAIL_LINE_H` is `TAIL_TEXT_SIZE * a line-box factor`; iced's
/// default line height is ~1.3x the text size, so 13 * 1.3 ~= 17 -> 18 with a hair of slack. If a
/// DPI/zoom case ever makes this drift from iced's actual line box the symptom is scrollbar drift,
/// and the fix is this one constant (called out in `visible.rs`).
const TAIL_TEXT_SIZE: f32 = 13.0;
const TAIL_LINE_H: f32 = 18.0;
/// Extra lines rendered above and below the strictly-visible band so a fast scroll never flashes
/// blank before the next frame re-slices (the user's choice 2A).
const TAIL_OVERSCAN: usize = 20;
/// A fixed spacer (px) appended below the tail content so the floating horizontal scrollbar never
/// covers the newest line while following (BACKLOG §8; DECISIONS R119 — supersedes the .i34 attempt).
/// WHY A SPACER, NOT `Scrollbar::spacing`: `.i34` set `spacing` to make the bars embedded, but iced's
/// `scrollable::layout` only reserves space for a SINGLE-direction embedded bar — a `Direction::Both`
/// scrollable falls through to `_ => layout(0,0)` and the spacing is ignored (scrollable.rs:475-536),
/// so the bars still floated and the .i34 fix was a no-op (the user's narrow-window screenshot). The tail
/// needs BOTH directions (no-wrap horizontal + virtualized vertical), so instead we reserve the space
/// ourselves: an extra `TAIL_HBAR_H` at the bottom of the content means that, when snapped to the
/// bottom, the last real line sits ABOVE where the floating bar paints. Kept < `TAIL_LINE_H` (18) so
/// it can't trip the 2-line follow-pause threshold (R99). The bar is ~10 px; 16 clears it with a gap.
const TAIL_HBAR_H: f32 = 16.0;
/// Assumed screen work area for placement when the real monitor size isn't queried. Placement is
/// clamped to this; a window dragged elsewhere is the user's business.
const ASSUMED_SCREEN_W: f32 = 1920.0;
const ASSUMED_SCREEN_H: f32 = 1040.0;

/// A file tile's render data: (display name, full path, visual state).
type FileTile = (String, String, ButtonVis);

fn split_patterns(s: &str) -> Vec<String> {
    s.split([';', ','])
        .map(|p| p.trim())
        // Strip a surrounding pair of double quotes per pattern (finding 8): a pasted `"*.log"` used
        // to become a literal name no file has, silently matching nothing.
        .map(strip_surrounding_quotes)
        .filter(|p| !p.is_empty())
        .map(|p| p.to_string())
        .collect()
}

/// One open tail window: the file it follows, its BACKGROUND reader stream, the text accumulated so
/// far, and whether follow (auto-scroll-to-bottom) is currently engaged. The reader runs on its own
/// thread ([`TailStream`]) so a large file's initial read never blocks the GUI (DECISIONS R67).
///
/// VIRTUALIZED RENDER (DECISIONS R69, .i11): the whole file stays in `text` (full scrollback), but
/// `tail_view` only renders the lines currently scrolled into view (see [`crate::visible`]). To
/// slice cheaply we keep `line_starts` - the byte offset of each line's start in `text` - updated
/// incrementally on every append, so the visible slice is an O(visible) substring, never an O(file)
/// walk. `scroll_y` mirrors the scrollable's absolute vertical offset (from `on_scroll`) so the
/// `responsive` view can compute the visible band without querying the widget.
struct TailWin {
    path: String,
    stream: TailStream,
    text: String,
    /// Byte offset in `text` where each line begins. Always starts with `[0]`; a trailing empty
    /// line (text ending in '\n') is represented by a final offset == text.len(), which we skip when
    /// rendering. Maintained by [`append_text`] so slicing never rescans the whole buffer.
    line_starts: Vec<usize>,
    encoding: String,
    /// True until the first chunk arrives; drives the "loading..." placeholder.
    loading: bool,
    /// True while the view should auto-scroll to the bottom. Set false when the user scrolls up,
    /// true again when they return to the bottom.
    following: bool,
    /// The scrollable's current absolute vertical offset in px (from `on_scroll`), used to compute
    /// the visible line band. Starts at 0 (top).
    scroll_y: f32,
    /// A widget id so we can drive follow (snap-to-end) on the scrollable.
    scroll_id: iced::widget::Id,

    // --- per-file search (BACKLOG §2b, DECISIONS R80) ---
    /// The current search query (from the always-visible search box at the bottom of the window).
    /// Empty => no search.
    search_query: String,
    /// Byte-range matches of `search_query` in `text`, recomputed when the query changes or new text
    /// appends (via `refresh_matches`). Drives both the highlight spans and the "N of M" count.
    matches: Vec<Match>,
    /// The current match index (into `matches`), or `None` when nothing is selected / no matches.
    /// Next/Prev step it with wrap; a match is scrolled into view when it changes.
    current_match: Option<usize>,
    /// DEBOUNCE (DECISIONS R162): when a query change on a LARGE buffer (>= `search::DEBOUNCE_THRESHOLD_BYTES`)
    /// is deferred, this holds the `tick_count` deadline at which the `Tick` handler runs the rescan.
    /// A later keystroke re-arms it (pushing the deadline out), so a typing burst collapses to one
    /// scan. `None` when nothing is pending (a small-buffer change scans inline and never arms this).
    /// Mirrors the `raise_pending` tick-deadline pattern (R85).
    search_rescan_due: Option<u64>,
    /// A widget id for the search text box, so Ctrl-F can focus it.
    search_id: iced::widget::Id,
    /// Running total of lines dropped by the scrollback cap (review #4 finding 9): the marker line
    /// reports this total, so re-trimming never "loses" earlier drops or counts the marker itself.
    dropped_lines: usize,
    /// Running total of BYTES cut from the front of a single over-cap line (R146); reported by the
    /// marker beside `dropped_lines`.
    dropped_bytes: usize,
    /// True while line 0 of `text` is the scrollback-cap marker (so a later trim replaces it
    /// instead of counting it as a dropped content line).
    has_drop_marker: bool,
    /// A `\r` seen as the LAST byte of the previous chunk, held back to see whether the next chunk
    /// opens with `\n` (a `\r\n` split across the reader's 4 MB pieces or a decode boundary). See
    /// [`append_text`] and the R138 line-ending normalisation (review 2026-09-16 finding 1).
    pending_cr: bool,
}

impl TailWin {
    /// Number of NON-EMPTY-TERMINATOR lines: `line_starts` has one entry per line start, but a file
    /// ending in '\n' leaves a final offset == text.len() that is not a real line. That trailing
    /// sentinel is only present when text is non-empty and ends in '\n'.
    fn line_count(&self) -> usize {
        let n = self.line_starts.len();
        if n == 0 {
            0
        } else if self.text.ends_with('\n') {
            n - 1
        } else {
            n
        }
    }

    /// The substring covering lines `[first, last)` (byte-exact; the caller has already clamped the
    /// range to `line_count()`). Empty if the range is empty.
    fn slice_text(&self, first: usize, last: usize) -> &str {
        if first >= last || first >= self.line_starts.len() {
            return "";
        }
        let start = self.line_starts[first];
        let end = if last < self.line_starts.len() {
            // Up to (not including) the start of `last`, minus EXACTLY the one '\n' that ends line
            // `last-1` — never more: `trim_end_matches` used to strip a whole run of terminators,
            // so a band ending in k blank lines rendered k rows short of what `visible.rs` sized
            // its spacers for (review #2 finding 3; the R69/R98 one-row-per-line invariant).
            let e = self.line_starts[last];
            self.text[..e]
                .strip_suffix('\n')
                .map(str::len)
                .unwrap_or(e)
                .max(start)
        } else {
            self.text.len()
        };
        &self.text[start..end]
    }

    /// Recompute `matches` for the current query from scratch and clamp `current_match` to the new
    /// count. Called when the QUERY changes (a keystroke). O(text), no lowercased copy (review B3).
    fn refresh_matches(&mut self) {
        self.matches = search::find_matches(&self.text, &self.search_query);
        self.current_match = search::clamp_current(self.current_match, self.matches.len());
    }

    /// Extend `matches` for text appended since `old_len` — O(chunk), not O(file) (review B3,
    /// DECISIONS R97). Called after every append while a query is active; `refresh_matches` used to
    /// rescan the whole buffer (plus a full lowercased copy) on every tick a busy log appended.
    fn extend_matches(&mut self, old_len: usize) {
        if self.search_query.is_empty() {
            return;
        }
        search::extend_matches(&self.text, &self.search_query, old_len, &mut self.matches);
        self.current_match = search::clamp_current(self.current_match, self.matches.len());
    }

    /// The line index (0-based, into the rendered lines) containing byte offset `off`. Binary-search
    /// over `line_starts`. Used to scroll the current match into view: the target scroll offset is
    /// `line_of(match.start) * TAIL_LINE_H`.
    fn line_of(&self, off: usize) -> usize {
        // partition_point: number of starts <= off; minus 1 is the line index (>=0 since starts[0]=0).
        self.line_starts
            .partition_point(|&s| s <= off)
            .saturating_sub(1)
    }
}

/// Placeholder for a lone `\r` (a progress-bar / bare-CR line): a single visible cell that is NOT a
/// paragraph separator, so it never splits a shaped row. U+240D SYMBOL FOR CARRIAGE RETURN.
const CR_PLACEHOLDER: char = '\u{240D}';

/// Normalise line endings and stray separators so that ONE logical line (as counted by `\n`) always
/// shapes to exactly ONE row in the renderer (review 2026-09-16 finding 1, DECISIONS R138).
///
/// Why this exists: the tail's whole geometry rests on "one logical line == one `TAIL_LINE_H` row"
/// (R69/R98). We index lines by `\n` and hand the visible band to iced's `rich_text`, which routes
/// it through cosmic-text. cosmic-text has TWO paragraph splitters: an ASCII fast path that splits
/// on `\n` and strips a trailing `\r` (so CRLF is fine), and — the instant the band holds one
/// non-ASCII char OR one control char other than `\n \r \t` — a Unicode-Bidi path where `\r`, `\n`,
/// U+0085, U+2028, U+2029 and U+001C–U+001E are EACH a paragraph separator. On that path `"a\r\nb"`
/// becomes `a`, ``, `b` — every CRLF line renders as two rows. Ordinary Windows logs (CRLF) with any
/// accented name, smart quote, en dash, ANSI colour escape, or DirWatch's own U+FFFD for a cp1252
/// byte all trip it. The fix is to never emit a byte that the shaper would treat as a line break
/// except the `\n` we count:
///
/// - `\r\n` -> `\n`   (with a pending-CR flag for a split across chunk boundaries)
/// - lone `\r` -> [`CR_PLACEHOLDER`]   (a bare-CR progress line stays one row)
/// - U+0085, U+2028, U+2029, U+001C..=U+001E and every other C0 control except `\t` -> U+FFFD
///
/// `\t` is left alone (the shaper does not break on it). The result is appended verbatim, and
/// `line_starts` is derived from it, so the index and the renderer agree by construction.
///
/// `pending_cr` is a `\r` held back from the END of the previous chunk (its classification depends
/// on the next byte). Returns the new pending-CR state. Pure and unit-tested against the probe
/// strings from the review.
fn normalize_line_endings(chunk: &str, pending_cr: bool, out: &mut String) -> bool {
    // FAST PATH (review #6 finding 7, DECISIONS R145): the char-by-char loop below cost ~47 ms per
    // 4 MB piece at the release profile — 3x the pre-R138 append — on the GUI thread, for every byte
    // ever shown. Almost every chunk of a real log needs no rewriting at all, so find the first byte
    // the slow path would touch and copy everything before it verbatim; a chunk with nothing to
    // rewrite is one `push_str`. `first_special` and the `match` below MUST agree on what "special"
    // is (test `normaliser_fast_and_slow_paths_agree`).
    let start = if pending_cr {
        0
    } else {
        match first_special(chunk) {
            Some(i) => i,
            None => {
                out.push_str(chunk);
                return false;
            }
        }
    };
    out.push_str(&chunk[..start]);
    let mut prev_cr = pending_cr;
    for ch in chunk[start..].chars() {
        if prev_cr {
            // The previous char was a `\r` awaiting its partner. If this is `\n`, the pair is a
            // CRLF -> one `\n`. Otherwise the `\r` was lone -> placeholder, then handle this char.
            prev_cr = false;
            if ch == '\n' {
                out.push('\n');
                continue;
            }
            out.push(CR_PLACEHOLDER);
            // fall through to classify `ch` normally
        }
        match ch {
            '\r' => prev_cr = true, // decide on the next char (or at flush)
            '\n' | '\t' => out.push(ch),
            // Bidi class B separators other than \n\r, plus any remaining C0 control: neutralise.
            '\u{0085}' | '\u{2028}' | '\u{2029}' | '\u{001C}'..='\u{001E}' => out.push('\u{FFFD}'),
            c if (c as u32) < 0x20 => out.push('\u{FFFD}'),
            c => out.push(c),
        }
    }
    prev_cr
}

/// Byte offset of the first character [`normalize_line_endings`] would rewrite, or `None` when the
/// chunk passes through untouched: any C0 control other than `\n`/`\t` (that includes `\r`), NEL
/// (`C2 85`), U+2028/U+2029 (`E2 80 A8` / `E2 80 A9`). A byte scan, so the common all-clean chunk
/// costs one pass with no per-char work. Every offset returned is a char boundary (a lead byte).
fn first_special(chunk: &str) -> Option<usize> {
    let b = chunk.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c < 0x20 && c != b'\n' && c != b'\t' {
            return Some(i);
        }
        if c == 0xC2 && b.get(i + 1) == Some(&0x85) {
            return Some(i);
        }
        if c == 0xE2
            && b.get(i + 1) == Some(&0x80)
            && matches!(b.get(i + 2), Some(0xA8) | Some(0xA9))
        {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Append `chunk` to `tw.text` (after [`normalize_line_endings`]) and extend `line_starts` for the
/// newly added lines only. This keeps the line index in sync incrementally: after each append,
/// `line_starts[i]` is the byte offset of line i's first character, so `slice_text` is O(visible)
/// not O(file). Pure w.r.t. the rest of the struct so it can be reasoned about on its own.
///
/// INVARIANT: `line_starts` holds one offset per line start, always beginning with `0`. Each '\n'
/// pushes the offset of the character AFTER it (the next line's start). So a buffer ending in '\n'
/// carries a final offset == `text.len()` - a sentinel with no real line after it, which
/// `line_count()` subtracts and `slice_text` never dereferences past. Seeded with `[0]` on the
/// first byte and extended purely by scanning the appended (normalised) chunk, so it never rescans
/// the buffer. Line endings are normalised first (R138) so the count here matches the renderer's.
fn append_text(tw: &mut TailWin, chunk: &str) {
    if chunk.is_empty() {
        return;
    }
    // Normalise STRAIGHT into the buffer (R145: no scratch copy), then index the bytes that landed.
    // A held-back `\r` may add nothing (a chunk that is just "\r"): nothing to index then.
    let base = tw.text.len();
    tw.pending_cr = normalize_line_endings(chunk, tw.pending_cr, &mut tw.text);
    if tw.text.len() == base {
        return;
    }
    if tw.line_starts.is_empty() {
        tw.line_starts.push(0);
    }
    // Record a new line start after each '\n' within the appended (normalised) bytes.
    for (i, b) in tw.text.as_bytes()[base..].iter().enumerate() {
        if *b == b'\n' {
            tw.line_starts.push(base + i + 1);
        }
    }
}

// The scrollback bound (`SCROLLBACK_CAP_BYTES` / `SCROLLBACK_TRIM_TO`, BACKLOG §4, DECISIONS R104)
// lives in `dirwatch_core::tail` since .i32 (review #2 finding 5): the READER applies the same bound
// to an existing file's first load, so the producer and this consumer cannot disagree.

/// Enforce [`SCROLLBACK_CAP_BYTES`] on a tail buffer (BACKLOG §4, DECISIONS R104). When `text`
/// exceeds the cap, drop whole oldest LINES until at/under the trim target, prepend a
/// `--- earlier N lines dropped ---` marker, and rebuild `line_starts` in ONE O(lines) pass.
/// Returns `true` if it trimmed (the caller then rebuilds search matches, whose byte offsets moved).
/// Dropping whole lines keeps the one-row-per-line invariant (R69/R98) intact. Cost: search can no
/// longer find dropped text — the recorded consequence of the R69 reversal.
fn cap_scrollback(tw: &mut TailWin) -> bool {
    cap_scrollback_to(tw, SCROLLBACK_CAP_BYTES, SCROLLBACK_TRIM_TO)
}

/// The scrollback trim with explicit `cap`/`trim_to` bounds, so the logic is testable with small
/// values instead of allocating 64 MB in a unit test (BACKLOG §4, DECISIONS R104).
fn cap_scrollback_to(tw: &mut TailWin, cap: usize, trim_to: usize) -> bool {
    if tw.text.len() <= cap {
        return false;
    }
    let mut trimmed = false;
    // Only REAL lines may be cut: `line_starts` may end in the sentinel (a trailing '\n' leaves a
    // final offset == text.len() with no line after it). Cutting AT the sentinel dropped a single
    // over-cap terminated line entirely — buffer gone, marker only (review #4 finding 9).
    let real_lines = tw.line_count();
    if real_lines == 0 {
        return false;
    }
    let marker_lines = usize::from(tw.has_drop_marker);
    if real_lines >= 2 {
        // Find the first line start at/after the number of bytes we must drop to reach the trim
        // target. `line_starts` is sorted; the first start strictly past `drop_at_least` begins the
        // kept region. Never cut at/after the last REAL line: the newest line always survives.
        let drop_at_least = tw.text.len() - trim_to;
        let cut_line = tw
            .line_starts
            .partition_point(|&s| s <= drop_at_least)
            .min(real_lines - 1);
        // Lines [0, cut_line) go. If line 0 is a previous trim's marker it is not CONTENT: exclude it
        // from the count. If dropping whole lines gains nothing (the newest line alone is over the
        // cap) fall through to the byte bound below.
        let content_dropped = cut_line.saturating_sub(marker_lines);
        if content_dropped > 0 {
            let cut_byte = tw.line_starts[cut_line];
            tw.dropped_lines += content_dropped;
            let kept = tw.text[cut_byte..].to_string();
            rebuild_with_marker(tw, kept);
            trimmed = true;
            if tw.text.len() <= cap {
                return true;
            }
            // Still over the cap: the newest line alone is over it — continue with the byte bound
            // IN THIS CALL (a giant line that arrived behind older lines in one tick, e.g. the
            // reader's skipped-bytes marker followed by a 48 MB NUL line — caught by the .i48 gate).
        }
    }
    // ONE GIANT LINE (review #1 B12 residual, review #6 finding 6, DECISIONS R146): the newest line
    // is itself over the cap, so no whole-line cut can help — it used to be kept "whatever its
    // size" (a 48 MB zero-filled first load became 144 MB of U+FFFD, over the 64 MB cap forever).
    // Now the line is cut by BYTES from its front to the trim target, on a char boundary, and the
    // marker reports bytes as well as lines. Any older lines before it (a previous marker excluded)
    // go too — they are the least recent content. Rows are unchanged: it is still one line.
    let real_lines = tw.line_count();
    let marker_lines = usize::from(tw.has_drop_marker);
    let last = real_lines - 1;
    let line_start = tw.line_starts[last];
    let older = last.saturating_sub(marker_lines);
    let line_len = tw.text.len() - line_start;
    if line_len <= trim_to && older == 0 {
        return trimmed; // nothing to gain (only a marker sits before a line under the target)
    }
    let mut cut = line_len.saturating_sub(trim_to);
    while cut < line_len && !tw.text.is_char_boundary(line_start + cut) {
        cut += 1;
    }
    tw.dropped_lines += older;
    tw.dropped_bytes += cut;
    let kept = tw.text[line_start + cut..].to_string();
    rebuild_with_marker(tw, kept);
    true
}

/// Replace the buffer with the drop marker + `kept`, rebuild `line_starts` in one pass, and mark
/// the marker present (shared by the line and byte trims, R146).
fn rebuild_with_marker(tw: &mut TailWin, kept: String) {
    let marker = drop_marker(tw.dropped_lines, tw.dropped_bytes);
    let mut new_text = String::with_capacity(marker.len() + kept.len());
    new_text.push_str(&marker);
    new_text.push_str(&kept);
    tw.text = new_text;
    tw.has_drop_marker = true;
    tw.line_starts.clear();
    tw.line_starts.push(0);
    for (i, b) in tw.text.bytes().enumerate() {
        if b == b'\n' {
            tw.line_starts.push(i + 1);
        }
    }
    // Search offsets are now stale; the caller does a full refresh. Scroll position is unaffected in
    // FOLLOW mode (snaps to bottom); a paused user scrolled up may jump — acceptable at the cap.
}

/// The scrollback-cap marker line: lines, bytes, or both (R104 wording kept for the lines-only case).
fn drop_marker(lines: usize, bytes: usize) -> String {
    match (lines, bytes) {
        (l, 0) => format!("--- earlier {l} lines dropped ---\n"),
        (0, b) => format!("--- earlier {b} bytes dropped ---\n"),
        (l, b) => format!("--- earlier {l} lines and {b} bytes dropped ---\n"),
    }
}

/// What KIND of message the status-strip note currently holds (review #3 finding 3 + i38 fix A,
/// DECISIONS R125). The note used to be a bare `String` whose kind was inferred from its `"ERROR:"`
/// text prefix — but the watch-error note and the dir-cap note share that prefix, so the Tick logic
/// could not tell them apart. That caused two bugs: a transient blip cleared/re-logged the cap note
/// (finding 3, patched at i37), and a REAL root outage recovering re-logged the cap note every cycle
/// (fix A). Carrying the kind explicitly removes the whole class: each block acts on the kind, never
/// on the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoteKind {
    /// A live watch error (root unreadable). Cleared when the root reads again (`Recovered`).
    WatchError,
    /// The directory-box cap overflow message (R122). Latched config state; cleared by Restart
    /// (`reset()`), never by a root recovering.
    DirCap,
    /// The file-tile cap overflow message (`MAX_FILES`, finding 1): raised only when a new file was
    /// REFUSED because every tile was an open window (a normal eviction is silent). Latched like
    /// DirCap; cleared by Restart.
    FileCap,
    /// A plain informational note (e.g. "stopped"). Not an error, not the cap.
    Plain,
}

/// One directory box in the cached tile layout: the display path of the box header and the files it
/// holds, already grouped and in display order. Only the STRUCTURE is cached (which tiles, in which
/// box, in what order) — never a tile's colour, which is time-dependent (`ButtonVis` decays by the
/// clock) and so is recomputed every frame from the path via `vis_of` (finding 1).
#[derive(Clone)]
struct CachedBox {
    /// The box header / directory display string (the first casing seen, matching the file sort).
    dir: String,
    /// `(file_name, full_path)` for each tile in the box, in display order.
    files: Vec<(String, String)>,
}

/// The GUI's cached, grouped-and-sorted tile layout for the main window (finding 1). Building this
/// is O(files log files) for the sort plus O(files × dirs) for the grouping; it used to run on every
/// 100 ms `Tick` even when nothing changed, which is the idle CPU cost the review measured (40 % of a
/// core at 5 000 files on Windows). It is now rebuilt only when the model's `structure_rev` (the set
/// of tracked paths) or a layout-affecting input (the watched root, `tiles_per_box`) changes; between
/// rebuilds `view()` maps this cache and only recomputes each tile's colour.
#[derive(Clone, Default)]
struct TileLayout {
    boxes: Vec<CachedBox>,
    /// The `SessionModel::structure_rev` this layout was built from.
    rev: u64,
    /// The watched root the boxes were ordered against (affects `dir_box_order`).
    root: String,
    /// The `tiles_per_box` the packing was computed for.
    tiles_per_box: usize,
    /// Whether this cache has ever been built (a fresh `Default` has not).
    built: bool,
}

/// Group a set of file paths into the sorted directory boxes the main window renders (finding 1).
/// PURE and unit-testable off-hardware. This is the exact grouping/ordering the pre-cache `file_area`
/// did inline every frame, with the per-comparison / per-candidate allocations lifted out:
///   * files sort by their lowercased path (one precomputed key each, not `to_lowercase()` per
///     comparison);
///   * boxes group by the shared `fs_key_str` parent key (the same fold the cap count uses), first
///     casing seen keeps the display string;
///   * boxes order by `dir_box_order` (watched root first, then depth-first hierarchical).
fn group_tile_boxes(paths: &[String], root: &str) -> Vec<CachedBox> {
    struct Row {
        sort_key: String,
        name: String,
        path: String,
        parent: String,
        parent_key: String,
    }
    let mut rows: Vec<Row> = paths
        .iter()
        .map(|path| {
            let p = Path::new(path);
            let parent = p
                .parent()
                .map(|pp| pp.to_string_lossy().to_string())
                .unwrap_or_default();
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.clone());
            let parent_key = fs_key_str(&parent);
            let sort_key = path.to_lowercase();
            Row {
                sort_key,
                name,
                path: path.clone(),
                parent,
                parent_key,
            }
        })
        .collect();
    rows.sort_by(|a, b| a.sort_key.cmp(&b.sort_key));

    let mut boxes: Vec<CachedBox> = Vec::new();
    for r in rows {
        match boxes
            .iter_mut()
            .find(|b| fs_key_str(&b.dir) == r.parent_key)
        {
            Some(b) => b.files.push((r.name, r.path)),
            None => boxes.push(CachedBox {
                dir: r.parent,
                files: vec![(r.name, r.path)],
            }),
        }
    }
    boxes.sort_by(|a, b| main_view::dir_box_order(root, &a.dir, &b.dir));
    boxes
}

/// The iced application state.
struct DirWatch {
    model: SessionModel,
    runtime: Option<WatchRuntime>,
    /// Cached grouped+sorted tile layout for the main window (finding 1). Refreshed in `update` by
    /// [`refresh_tile_layout`] whenever the model structure or a layout input changes; read by
    /// `main_view::file_area`.
    tile_layout: TileLayout,

    // main window id (set once the main window opens); closing it exits.
    main_id: Option<window::Id>,
    /// The main window's REAL outer position + inner size, from `window::events()` (Opened / Moved /
    /// Resized) — review B5, DECISIONS R97. Tail placement used to ASSUME a 1920x1040 screen with
    /// the main window centered on it, which put the first tail off-screen on a 1366-wide laptop.
    /// `None` until the first event (or forever on Wayland, which reports no position) — then the
    /// old assumption is the fallback.
    main_rect: Option<Rect>,
    /// Screen size ESTIMATE derived once from the main window's first reported position: the main
    /// window opens `Centered`, so `screen_w ~= 2*x + w` (and the same for height, plus a taskbar
    /// allowance) on the primary monitor. Frozen at first sight so a later drag doesn't skew it.
    /// `None` => `ASSUMED_SCREEN_*`. Logged via `trace_log` (DIRWATCH_DEBUG) so a hardware run can check it.
    screen_est: Option<(f32, f32)>,

    // editable fields (commit on Restart / Stop->Start)
    dir_input: String,
    depth_input: String,
    patterns_input: String,

    active_dir: PathBuf,
    active_patterns: Vec<String>,
    active_depth: i32,
    max_windows: i32,
    /// The status-strip note and its kind (review #3 finding 3 + i38 fix A, DECISIONS R125). `None`
    /// means the strip shows the default `watching …` / `stopped` text.
    note: Option<(NoteKind, String)>,

    /// The live, committed Settings values (step 5, DECISIONS R79). Drives `SessionModel` knobs, the
    /// poll interval, and the dir-box layout. Edited only through the Settings window's OK.
    settings: Settings,
    /// The Settings window's staged edit state + id, present only while the window is open. `None`
    /// means no Settings window; opening while `Some` raises the existing one (single-instance).
    settings_win: Option<SettingsDraft>,

    // open tail windows, keyed by their window id; plus a reverse path->id map for raise-to-front.
    tails: HashMap<window::Id, TailWin>,
    path_to_win: HashMap<String, window::Id>,
    /// The REAL on-disk path behind each display path the model keys on (review #6 finding 5,
    /// DECISIONS R143). The model and every tile are keyed by a `String` made with
    /// `to_string_lossy()`; on Linux a non-UTF-8 name (a Latin-1 byte from an old archive or a
    /// mis-charset mount) turns into U+FFFD there, and reopening the file by that string found
    /// nothing — a tile whose tail said "file missing" forever. Tails now open from this map. Filled
    /// from every path-carrying watch event; cleared with the model on Restart.
    real_paths: HashMap<String, PathBuf>,
    /// Tail window ids we have asked the OS to close (Restart, or programmatic close) but whose
    /// `WindowClosed` has not arrived yet (review #2 finding 18 / i38 fix B, DECISIONS R125). The OS
    /// close is async, so for the frame(s) in between the id is gone from `tails` and `view()` would
    /// fall through to `main_view`, flashing the main UI into the closing window (the .i7-flash
    /// class). `view()` renders a blank placeholder for an id in this set instead; `WindowClosed`
    /// removes it.
    closing: std::collections::HashSet<window::Id>,

    // --- blink state (step 4b, DECISIONS R71) ---
    /// Monotonic origin for the cap-blink clock; `start.elapsed()` gives `now_ms`.
    start: Instant,
    /// Incremented every Tick; feeds `blink::is_bright` so the active-closed tile pulses ~450 ms.
    tick_count: u64,
    /// Last `model.evicted_total()` seen, so a Tick can trace NEW cap evictions under DIRWATCH_DEBUG
    /// (finding 1 cap verification) without logging every tick.
    evicted_seen: u64,
    /// `now_ms` of the last over-cap open attempt (click or auto-open), or `None`. Drives the
    /// cap-reached count blink for `blink::CAP_BLINK_MS` after it (DECISIONS R29 item 5).
    cap_blink_start_ms: Option<u64>,

    /// A pending DEFERRED raise-to-front on an auto-opened tail (DECISIONS R85). Set by
    /// `AutoOpenRealized` to `(tail id, tick_count deadline)`; when the Tick reaches the deadline it
    /// fires `AttemptRaise(id)` and clears this. The short defer (a few Ticks) lets winit finish
    /// SETTLING the realized window before we raise it - firing inline at realization (.i21) let
    /// winit's own post-realization focus handling overwrite the raise, so nothing showed (R84->R85).
    /// Only armed on Windows; harmless `None` elsewhere.
    raise_pending: Option<(window::Id, u64)>,
}

impl DirWatch {
    /// Milliseconds since this app's monotonic origin — the clock for the cap-blink window.
    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    /// The ONE writer of the status-strip note (review #4 finding 7, DECISIONS R128). Until `.i39`
    /// five handlers wrote `note = None` directly — leftovers from when the note was transient text
    /// ("cap reached", "Settings not implemented") — and so a tile click, an over-cap click, Browse
    /// or Settings blanked a live `ERROR: cannot read …` for the rest of the outage and re-logged
    /// the dir-cap note on every tail open. Notes are now set here and cleared only BY KIND.
    fn set_note(&mut self, kind: NoteKind, text: String) {
        self.note = Some((kind, text));
    }

    /// Rebuild the cached tile layout IF the model structure or a layout-affecting input changed
    /// since the last build (finding 1). Cheap to call every tick: the common case is one `u64` and
    /// two field comparisons and an early return. When it does rebuild, it does the sort + grouping
    /// ONCE here (in `update`, off the hot per-frame `view` path) instead of on every frame.
    ///
    /// A — allocation cleanups vs. the pre-split source: the sort key is precomputed once per entry
    /// (a `(lowercased_path)` alongside the path) instead of `to_lowercase()` on every comparison,
    /// and each file's parent key is folded once and carried, instead of re-folding both sides inside
    /// the O(files × dirs) `find` on every candidate. The result is byte-identical grouping/order to
    /// the old code; only the redundant per-comparison allocations are gone.
    fn refresh_tile_layout(&mut self) {
        let rev = self.model.structure_rev();
        let root = self.active_dir.to_string_lossy();
        let tiles_per_box = self.settings.tiles_per_box;
        // Fast path: nothing that affects the STRUCTURE changed — keep the cached layout, colours are
        // recomputed per frame in `file_area`.
        if self.tile_layout.built
            && self.tile_layout.rev == rev
            && self.tile_layout.tiles_per_box == tiles_per_box
            && self.tile_layout.root == root.as_ref()
        {
            return;
        }

        let paths: Vec<String> = self.model.entries().map(|e| e.path.clone()).collect();
        let root_owned = root.to_string();
        let boxes = group_tile_boxes(&paths, &root_owned);

        self.tile_layout = TileLayout {
            boxes,
            rev,
            root: root_owned,
            tiles_per_box,
            built: true,
        };
    }

    /// Clear the note only if it is of `kind`; any other note is left alone. Returns whether it
    /// cleared something.
    fn clear_note_if(&mut self, kind: NoteKind) -> bool {
        if matches!(self.note, Some((k, _)) if k == kind) {
            self.note = None;
            true
        } else {
            false
        }
    }
}

/// The LAST root-readability change seen in one Tick's drain (review #4 finding 6, DECISIONS
/// R128): `Error` and `Recovered` used to be collapsed into two independent booleans applied in a
/// fixed clear-then-set order, so `[Error, Recovered]` in ONE drain (a late Tick — the Browse
/// dialog blocks `update` — or two sweeps inside one 100 ms tick) left a permanent stale ERROR
/// note. Keeping only the last event in drain order applies the outcome of the whole batch.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RootChange {
    Error(String),
    Recovered,
}

#[derive(Debug, Clone)]
enum Message {
    Tick,
    DirChanged(String),
    DepthChanged(String),
    PatternsChanged(String),
    Browse,
    ToggleWatch,
    Restart,
    OpenSettings,
    /// The Settings window's open Task resolved with its realized id (step 5, DECISIONS R79).
    SettingsOpened(window::Id),
    /// A Settings field edited (staged as a string; committed only on OK).
    SettingsMaxWindowsChanged(String),
    SettingsActiveSecsChanged(String),
    SettingsPollMsChanged(String),
    SettingsTilesPerBoxChanged(String),
    /// The auto-open checkbox toggled in Settings.
    SettingsNoOpenToggled(bool),
    /// OK: commit the staged fields to the live settings, apply them, and close the window.
    SettingsOk,
    /// Cancel: discard the staged fields and close the window (live settings untouched).
    SettingsCancel,
    /// A tile was clicked: open (or raise) that file's tail window.
    TileClicked(String),
    /// The main window's open Task resolved.
    MainOpened(window::Id),
    /// A window-level event (Opened / Moved / Resized) for some window id; used to track the MAIN
    /// window's real rect for tail placement (review B5, DECISIONS R97).
    WindowEvent(window::Id, window::Event),
    /// An AUTO-OPENED tail window's open Task resolved - the window is now REALIZED by the OS
    /// (.i15 diagnostic, DECISIONS R76). We request user attention (taskbar flash) HERE, not at
    /// spawn, so the request lands on a window that actually has a taskbar button - testing whether
    /// the .i14 no-flash was a timing problem (attention fired against a not-yet-realized id).
    AutoOpenRealized(window::Id),
    /// The deferred raise-to-front settle delay elapsed for this auto-opened tail (DECISIONS R85):
    /// raise it to the front now (winit has settled the window) via the cross-platform
    /// `deferred_raise` (R101). Only ARMED on Windows (`AutoOpenRealized`); macOS/X11 rely on
    /// `request_user_attention` instead, so the variant is never constructed there.
    #[cfg_attr(not(windows), allow(dead_code))]
    AttemptRaise(window::Id),
    /// A window was closed by the user (OS X). Free the slot if it's a tail.
    WindowClosed(window::Id),
    /// A tail window's scrollable moved; update follow state.
    TailScrolled(window::Id, scrollable::Viewport),
    /// Ctrl-W pressed while `window::Id` is focused: close it if it's a tail window.
    CloseTail(window::Id),
    /// The search query in a tail window changed (recompute matches, jump to the first).
    SearchChanged(window::Id, String),
    /// Next / previous match in a tail window's search (wraps).
    SearchNext(window::Id),
    SearchPrev(window::Id),
    /// Clear a tail window's search (the 'x' button, or Esc while its box has text): empties the
    /// query + matches + highlight and re-focuses the box.
    SearchCleared(window::Id),
    /// Ctrl-F pressed while `window::Id` is focused: focus that tail window's search box.
    FocusSearch(window::Id),
    /// The "jump to bottom & resume following" chip was clicked in a tail window (R173): snap the
    /// view to the bottom and re-engage follow. The chip is shown only while follow is paused.
    ResumeFollow(window::Id),
    /// Enter pressed while `window::Id` is focused: commit Settings if that's the Settings window
    /// (Enter=OK, backlog GUI-D / DECISIONS R79). No-op for any other window.
    EnterPressed(window::Id),
    /// Esc pressed while `window::Id` is focused: cancel Settings if that's the Settings window
    /// (Esc=Cancel). No-op for any other window.
    EscPressed(window::Id),
    /// A fire-and-forget Task completed (e.g. the Windows flash `window::run` callback). Carries no
    /// state; the effect already happened in the callback. Keeps `update` from having to invent a
    /// meaningful message for a side-effect-only Task. Only constructed on the `cfg(windows)` flash
    /// path (R77), so it reads as dead code off Windows - allow it there rather than cfg-splitting the
    /// whole Message enum.
    #[cfg_attr(not(windows), allow(dead_code))]
    Noop,
}

/// Resolve the watched directory from the Directory field (BACKLOG §7, DECISIONS R118).
///
/// Relative input (including empty / `.`) is resolved against `cwd` and normalised lexically by
/// path components; absolute input is normalised as-is. Deliberately NOT `canonicalize`:
/// canonicalize requires the path to already EXIST and prepends a `\\?\` verbatim prefix on
/// Windows — but the watched directory may not exist yet, and the prefix is ugly in the UI. This is
/// pure (no filesystem access, no symlink resolution), so it resolves a not-yet-existing directory
/// and is unit-testable. The result is the single truth shown in the Directory field, the status
/// strip, box headers, tail titles, and `DirWatch_debug.log` (the user's 1-A).
/// Strip ONE matching pair of surrounding double quotes from `s` (after trimming), if present
/// (REVIEW-2026-09-17 finding 8). Windows Explorer's "Copy as path" yields a double-quoted path
/// (`"C:\logs"`) so it survives a command line; pasted into the Directory or Patterns field the
/// quotes became literal path characters — the directory then resolved to `<cwd>\"C:\logs"` and
/// showed `ERROR: cannot read …` with the quotes embedded, and a quoted pattern (`"*.log"`) silently
/// matched nothing. We strip the quotes at RESOLVE time, not while the user types, so the field keeps
/// what they pasted and the cursor is never fought. Only a matching leading+trailing pair is removed;
/// an unbalanced quote is left alone (it may be a real, if unusual, filename character).
fn strip_surrounding_quotes(s: &str) -> &str {
    let t = s.trim();
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        &t[1..t.len() - 1]
    } else {
        t
    }
}

fn resolve_watch_dir(input: &str, cwd: &Path) -> PathBuf {
    let trimmed = strip_surrounding_quotes(input).trim();
    let raw = if trimmed.is_empty() {
        Path::new(".")
    } else {
        Path::new(trimmed)
    };
    // A drive-relative Windows path ("C:foo") carries a Prefix but is not absolute; joining a
    // foreign cwd onto it would be nonsense, so leave anything that already carries a prefix (or is
    // absolute) anchored as typed and let the OS resolve it. On non-Windows there are no prefixes,
    // so this only affects the Windows drive-relative case.
    let anchored = raw.is_absolute()
        || matches!(
            raw.components().next(),
            Some(std::path::Component::Prefix(_))
        );
    let joined = if anchored {
        raw.to_path_buf()
    } else {
        cwd.join(raw)
    };
    normalize_components(&joined)
}

/// Lexical path normalisation: drop `.` components and resolve `..` by popping a preceding normal
/// segment (a leading `..` with nothing to pop is kept). No filesystem access, no symlink
/// resolution — purely textual, so it works for a directory that does not exist yet.
fn normalize_components(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                // Pop a preceding normal segment.
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                // At a root or drive prefix, `..` is a no-op (root's parent is root).
                Some(Component::RootDir) | Some(Component::Prefix(_)) => {}
                // Leading `..` in a relative path with nothing to pop: keep it.
                _ => out.push(".."),
            },
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// The current working directory, or `.` if it can't be read (never panics).
fn current_dir_or_dot() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Build a `WatchConfig` from the current editable fields. The poll interval comes from the LIVE
/// settings (`app.settings.poll_ms`), NOT a hardcoded constant: before step 5 this returned a fixed
/// `250`, so any `--poll-ms` value (and later any Settings poll value) was silently dropped the
/// moment Start/Restart rebuilt the config (the poll-drop bug fixed at R79). Now Start/Restart and
/// the Settings poll-only restart all carry the real committed poll interval.
fn config_from_fields(app: &DirWatch) -> WatchConfig {
    // §7 (R118): resolve the typed directory to an absolute, normalised path at Start/Restart.
    let directory = resolve_watch_dir(&app.dir_input, &current_dir_or_dot());
    // An unparseable depth (an over-long digit string; the field is digits-only) keeps the ACTIVE
    // depth instead of silently becoming 0 (review #2 finding 8).
    let depth = app
        .depth_input
        .trim()
        .parse::<i32>()
        .unwrap_or(app.active_depth)
        .clamp(0, MAX_DEPTH);
    WatchConfig {
        directory,
        patterns: split_patterns(&app.patterns_input),
        poll_interval_ms: app.settings.poll_ms,
        depth,
    }
}

fn apply_opts(model: &mut SessionModel, opts: &ModelOpts) {
    model.no_open = opts.no_open;
    if let Some(secs) = opts.active_seconds {
        model.active_seconds = secs.max(1) as i64;
    }
    if let Some(max) = opts.max_windows {
        model.max_windows = max.clamp(1, dirwatch_core::session::MAX_WINDOWS_CEILING);
    }
}

/// Push the live [`Settings`] onto a `SessionModel` (step 5, DECISIONS R79). The three model-facing
/// tunables — max-windows, active-timeout, auto-open — are plain fields, so this applies them
/// directly with NO re-scan: an OK in the Settings dialog can change the cap or the active timeout
/// on the running model without closing tail windows or losing file state. (Poll-interval and
/// tiles-per-box are NOT model fields; the caller handles those.)
fn apply_settings_to_model(model: &mut SessionModel, s: &Settings) {
    model.max_windows = s
        .max_windows
        .clamp(1, dirwatch_core::session::MAX_WINDOWS_CEILING);
    model.active_seconds = s.active_seconds.max(1);
    model.no_open = s.no_open;
}

/// (Re)start the watch from the current fields. Closing all tail windows on restart matches the
/// .NET Restart (fresh rescan); we close them and clear the maps, then respawn the runtime.
fn start_watch(app: &mut DirWatch) -> Task<Message> {
    app.runtime = None;
    let cfg = config_from_fields(app);
    let mut model = SessionModel::new();
    // Apply the LIVE settings (they may have been changed in the Settings dialog since boot), not
    // just the boot-time CLI opts, so a Restart after a settings change keeps the new knobs
    // (DECISIONS R79). A fresh model + the live settings == the correct state for a rescan.
    apply_settings_to_model(&mut model, &app.settings);
    app.model = model;
    app.active_dir = cfg.directory.clone();
    // §7 (R118), 1-A: the Directory field shows the SAME resolved absolute path the watch uses.
    app.dir_input = cfg.directory.to_string_lossy().to_string();
    app.active_patterns = cfg.patterns.clone();
    app.active_depth = cfg.depth;
    app.max_windows = app.model.max_windows;
    app.runtime = Some(WatchRuntime::start(cfg));

    // Close any open tail windows (Restart = fresh view). The OS close is async, so mark each id as
    // `closing` (review #2 finding 18 / i38 fix B, R125) — `view()` renders a blank placeholder for a
    // closing id instead of falling through to `main_view` and flashing the main UI into the
    // about-to-close window for a frame or two (the .i7-flash class). `WindowClosed` removes the id.
    let mut tasks: Vec<Task<Message>> = Vec::new();
    let ids: Vec<window::Id> = app.tails.keys().copied().collect();
    for id in ids {
        app.closing.insert(id);
        tasks.push(window::close(id));
    }
    app.tails.clear();
    app.path_to_win.clear();
    app.real_paths.clear();
    Task::batch(tasks)
}

fn boot(cfg: WatchConfig, opts: ModelOpts) -> (DirWatch, Task<Message>) {
    // §7 (R118): resolve the launch directory to an absolute, normalised path up front, so the
    // Directory field, status strip, headers, titles and log show one truth from the first frame —
    // not the raw `.` or relative CLI argument until the first Restart.
    let mut cfg = cfg;
    cfg.directory = resolve_watch_dir(&cfg.directory.to_string_lossy(), &current_dir_or_dot());
    let mut model = SessionModel::new();
    apply_opts(&mut model, &opts);
    // Seed the live Settings from the same boot values the model just took, so the dialog reflects
    // what's actually running the first time it opens (DECISIONS R79).
    let settings = Settings::from_boot(&cfg, &opts);
    let mut app = DirWatch {
        model,
        runtime: None,
        tile_layout: TileLayout::default(),
        main_id: None,
        main_rect: None,
        screen_est: None,
        dir_input: cfg.directory.to_string_lossy().to_string(),
        depth_input: cfg.depth.to_string(),
        patterns_input: cfg.patterns.join(";"),
        active_dir: cfg.directory.clone(),
        active_patterns: cfg.patterns.clone(),
        active_depth: cfg.depth,
        max_windows: 10,
        note: None,
        settings,
        settings_win: None,
        tails: HashMap::new(),
        path_to_win: HashMap::new(),
        real_paths: HashMap::new(),
        closing: std::collections::HashSet::new(),
        start: Instant::now(),
        tick_count: 0,
        cap_blink_start_ms: None,
        raise_pending: None,
        evicted_seen: 0,
    };
    trace_log(&format!("log directory: {}", applog::dir().display()));
    // Build the tile-layout cache once up front so the first `view()` (which may run before the first
    // Tick) reads a valid, if empty, layout (finding 1).
    app.refresh_tile_layout();
    // Start the watch runtime (no windows yet).
    let _ = start_watch(&mut app);
    // Open the MAIN window; when it resolves we record its id.
    let (id, open_task) = window::open(window::Settings {
        size: Size::new(MAIN_W, MAIN_H),
        position: window::Position::Centered,
        icon: app_icon(),
        ..Default::default()
    });
    // `window::open` returns the real id synchronously; record it NOW so the window-event
    // subscription (Opened/Moved/Resized, review B5) can attribute events to the main window
    // even if they arrive before the open Task's `MainOpened` confirmation.
    app.main_id = Some(id);
    (app, open_task.map(Message::MainOpened))
}

/// Window titles (DECISIONS R170). The main window and the catch-all show [`build_info::PRODUCT`]
/// ("DirWatch 1.0") — NOT the full [`build_info::BUILD_ID`]: the user specified the full build string is
/// for `--version`/`--help`, logs, and crash stamps ONLY ("that's it"). The `.i58` title bug was
/// exactly this: both these arms returned `BUILD_ID`, so the titlebar carried the whole
/// "DirWatch 1.0 build …iNN" string. `checks.sh` now guards against `BUILD_ID` reappearing in either
/// title arm.
fn title(state: &DirWatch, id: window::Id) -> String {
    if Some(id) == state.main_id {
        build_info::PRODUCT.to_string()
    } else if state.settings_win.as_ref().map(|d| d.id) == Some(id) {
        "Settings - DirWatch".to_string()
    } else if let Some(t) = state.tails.get(&id) {
        // Tail window title: the file name + product name, so it's identifiable in the taskbar.
        let name = Path::new(&t.path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| t.path.clone());
        format!("{name} - DirWatch")
    } else {
        build_info::PRODUCT.to_string()
    }
}

fn theme(_state: &DirWatch, _id: window::Id) -> Theme {
    Theme::Dark
}

/// Pure keyboard-routing decision, split out of the `listen_with` closure so it is UNIT-TESTABLE
/// (the closure itself can't be, same reason `auto_open_action` / `commit_settings` are pure). Maps a
/// KeyPressed's `key` + `modifiers` (for the focused window `id`) to an optional `Message`:
///   * Ctrl-W OR Cmd-W  -> CloseTail(id)   (`update()` acts only if `id` is a tail window)
///   * Ctrl-F OR Cmd-F  -> FocusSearch(id) (`update()` acts only if `id` is a tail window)
///   * Enter -> EnterPressed(id); Esc -> EscPressed(id) (Settings-guarded in `update()`)
///
/// `modifiers.logo()` is iced's cross-platform name for the Command/Super key, so this is ADDITIVE:
/// Ctrl-W/Ctrl-F keep working on every OS (Windows/Linux unaffected), and Cmd-W/Cmd-F newly work on
/// macOS where Command is the native window-close / find modifier (DECISIONS R88, the user's request).
fn key_to_message(
    key: &keyboard::Key,
    modifiers: keyboard::Modifiers,
    id: window::Id,
) -> Option<Message> {
    use keyboard::key::Named;
    // Command (macOS) OR Control (Windows/Linux) — either satisfies the W/F shortcuts.
    let cmd_or_ctrl = modifiers.control() || modifiers.logo();
    match key {
        keyboard::Key::Character(c) if cmd_or_ctrl && c.as_str().eq_ignore_ascii_case("w") => {
            Some(Message::CloseTail(id))
        }
        keyboard::Key::Character(c) if cmd_or_ctrl && c.as_str().eq_ignore_ascii_case("f") => {
            Some(Message::FocusSearch(id))
        }
        keyboard::Key::Named(Named::Enter) => Some(Message::EnterPressed(id)),
        keyboard::Key::Named(Named::Escape) => Some(Message::EscPressed(id)),
        _ => None,
    }
}

fn subscription(_state: &DirWatch) -> Subscription<Message> {
    // Keyboard routing by focused window (`listen_with` hands us the window Id the event was
    // delivered to). The mapping is the pure `key_to_message` above (testable); here we only pick
    // KeyPressed events out of the stream and forward key + modifiers + id to it.
    //   * Ctrl-W / Cmd-W -> CloseTail(id)   (closes it only if it's a tail window; main/settings ignore).
    //   * Ctrl-F / Cmd-F -> FocusSearch(id) (focuses that tail's search box; main/settings ignore).
    //   * Enter -> EnterPressed(id); Esc -> EscPressed(id) (guarded on the Settings window in update()).
    //
    // DECISION (review A3 / C4, DECISIONS R97): `listen_with` delivers EVERY runtime event, including
    // ones a widget already captured (that is why it hands us `status`). We deliberately keep
    // routing captured events too — so Ctrl-W / Ctrl-F / Enter / Esc work while a text box has
    // focus — and instead removed the search box's own `on_submit`, which had made one Enter fire
    // BOTH `SearchNext` (widget) and `EnterPressed` (here) = two steps per press. The `_status`
    // is therefore ignored ON PURPOSE; `key_to_message` is the single Enter/Esc/Ctrl route.
    let keys = event::listen_with(|ev, _status, id| match ev {
        event::Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) => {
            key_to_message(&key, modifiers, id)
        }
        _ => None,
    });
    Subscription::batch([
        time::every(Duration::from_millis(TICK_MS)).map(|_| Message::Tick),
        window::close_events().map(Message::WindowClosed),
        // Opened / Moved / Resized for the main window's real rect (review B5).
        window::events().map(|(id, ev)| Message::WindowEvent(id, ev)),
        keys,
    ])
}

fn update(state: &mut DirWatch, message: Message) -> Task<Message> {
    match message {
        Message::Tick => {
            state.tick_count = state.tick_count.wrapping_add(1);
            let mut tasks: Vec<Task<Message>> = Vec::new();
            // Deferred raise-to-front (DECISIONS R85): if a raise is armed and its short settle delay
            // has elapsed, fire AttemptRaise now (a separate event-loop turn, after winit settled the
            // window) and disarm.
            if let Some((rid, deadline)) = state.raise_pending {
                if state.tick_count >= deadline {
                    state.raise_pending = None;
                    tasks.push(Task::done(Message::AttemptRaise(rid)));
                }
            }
            // Drain watch events into the model, collecting files that WANT to auto-open (step 4b).
            let mut to_open: Vec<String> = Vec::new();
            // The LAST of Error/Recovered in drain order (finding 6): what the root looks like NOW.
            let mut root_change: Option<RootChange> = None;
            if let Some(rt) = &state.runtime {
                for ev in rt.try_drain() {
                    match ev {
                        // Root unreadable (review B2): surface it in the status strip instead of
                        // the indistinguishable "no matching files yet".
                        WatchEvent::Error(msg) => root_change = Some(RootChange::Error(msg)),
                        // ...and clear it when the root reads again (review #2 finding 2).
                        WatchEvent::Recovered => root_change = Some(RootChange::Recovered),
                        other => {
                            remember_real_path(&mut state.real_paths, &other);
                            if let Some(path) = apply_event(&mut state.model, other) {
                                to_open.push(path);
                            }
                        }
                    }
                }
            }
            // Status-strip note precedence (i38 fix A, DECISIONS R125; single writer since R128).
            // The note carries its KIND (NoteKind), so each block acts on the kind — never on the
            // text. Order: the root's latest state (a Recovered clears only a WatchError note; an
            // Error takes precedence and replaces the note); then the dir-cap note fills in when
            // nothing else stands, logged only on a real transition INTO the cap state.
            let cap_kind_now = matches!(state.note, Some((NoteKind::DirCap, _)));
            let mut watch_recovered = false;
            match root_change {
                // (a) Recovered clears ONLY a real watch-error note. The cap note is latched config
                //     state (cleared by Restart / reset(), not by the root reading again), and a
                //     Plain note ("stopped") is not the root's business — both are left alone.
                Some(RootChange::Recovered) => {
                    watch_recovered = state.clear_note_if(NoteKind::WatchError);
                    if watch_recovered {
                        err_log("watch recovered: directory reads again");
                    }
                }
                // (b) A live root error outranks everything and replaces the note.
                Some(RootChange::Error(msg)) => {
                    err_log(&format!("watch error: {msg}"));
                    state.set_note(NoteKind::WatchError, format!("ERROR: {msg}"));
                }
                None => {}
            }
            // (c) Directory-box cap (R122): if the model dropped directories past MAX_DIR_BOXES, show
            //     the cap note — only while a watch is RUNNING (a stopped watch keeps its "stopped"
            //     note, review #4 finding 7) and only when NO other note stands (a real
            //     root-unreadable error outranks it). Log ONLY on a real transition into the cap
            //     state: if a recovered error just uncovered a still-latched overflow, restore the
            //     cap note SILENTLY (set, don't re-log) — that is fix A.
            if state.runtime.is_some()
                && state.model.dir_overflow()
                && state.note.is_none()
                && !cap_kind_now
            {
                let msg = format!(
                    "{}-directory limit reached — some folders are not shown; narrow the directory or reduce Depth",
                    dirwatch_core::session::MAX_DIR_BOXES
                );
                if !watch_recovered {
                    err_log(&format!("dir cap: {msg}"));
                }
                state.set_note(NoteKind::DirCap, format!("ERROR: {msg}"));
            }
            // (d) File-tile cap (MAX_FILES, finding 1): shown only when a new file was REFUSED because
            //     every tile is an open window (a normal at-cap eviction is silent — the cap working).
            //     Ranks below the dir cap and a root error; set only when no higher note stands. Latched
            //     like the dir cap; logged once on the transition in.
            let file_cap_now = matches!(state.note, Some((NoteKind::FileCap, _)));
            if state.runtime.is_some()
                && state.model.file_overflow()
                && state.note.is_none()
                && !file_cap_now
            {
                let msg = format!(
                    "{}-file limit reached — every tile is an open window, so new files are not shown; close some windows or narrow the patterns",
                    dirwatch_core::session::MAX_FILES
                );
                if !watch_recovered {
                    err_log(&format!("file cap: {msg}"));
                }
                state.set_note(NoteKind::FileCap, format!("ERROR: {msg}"));
            }
            // Auto-open each qualifying file (same cap gate + spawn path as a click). Done after the
            // drain loop so the model mutation and the window spawning don't interleave the borrow.
            for path in to_open {
                tasks.push(auto_open(state, path));
            }
            // Trace any NEW cap evictions applied while draining this tick's events (finding 1 cap
            // verification): a probe / gate step asserts the cap is EVICTING (making room), not just
            // refusing. Logged once per tick with the delta, under DIRWATCH_DEBUG.
            let evicted_now = state.model.evicted_total();
            if evicted_now != state.evicted_seen {
                trace_log(&format!(
                    "file cap: {} eviction(s) this tick ({} total), tiles held at {}",
                    evicted_now.wrapping_sub(state.evicted_seen),
                    evicted_now,
                    state.model.entry_count()
                ));
                state.evicted_seen = evicted_now;
            }
            // Prune the tiles of files gone for the whole MISSING_PRUNE_GRACE_SECS (finding 1 growth
            // path): a rotating directory stays bounded and shows only live files, without waiting to
            // hit the MAX_FILES cap. Runs before the layout rebuild so removals show this tick. Skipped
            // while the root is unreadable — "we can't see the directory" is not "the file is gone"
            // (mirrors the core sweep's own !root_down guard); otherwise a share blip past the core's
            // Missing marker would wipe tiles a user still wants.
            let root_down = matches!(state.note, Some((NoteKind::WatchError, _)));
            if !root_down {
                let removed = state.model.prune_missing(now_secs(), prune_grace_secs());
                if !removed.is_empty() {
                    // DIRWATCH_DEBUG trace so a probe / gate step can assert the prune fired without
                    // waiting for a tile to visibly vanish (finding 1 prune verification).
                    trace_log(&format!(
                        "prune: removed {} tile(s) missing >= {}s: {}",
                        removed.len(),
                        prune_grace_secs(),
                        removed.join(", ")
                    ));
                }
                for disp in removed {
                    let key = fs_key_str(&disp);
                    state.real_paths.remove(&key);
                    // Leave any OPEN tail window alone: it is keyed by window id and lives on its own
                    // reader (which reports "missing" itself); prune_missing never removed an open
                    // entry's tile out from under a window because a pruned entry is not open here.
                }
            }
            // Rebuild the cached tile layout only if the model structure or a layout input changed
            // this tick (finding 1). Cheap no-op on an idle tick; the sort+grouping runs here, off
            // the per-frame `view` path, instead of on every frame.
            state.refresh_tile_layout();
            // Drain each open tail's BACKGROUND stream; append new text; snap to bottom if following.
            // The reading happens off-thread, so a big file's initial load never blocks here. Appends
            // go through `append_text`, which keeps `line_starts` current so the virtualized view can
            // slice the visible lines cheaply (DECISIONS R69).
            for tw in state.tails.values_mut() {
                let mut appended = false;
                let old_len = tw.text.len();
                for c in tw.stream.try_drain() {
                    // The reader's first present poll — including the EMPTY chunk a 0-byte file
                    // sends (review #2 finding 4) — ends "loading..." and carries the label.
                    if c.ready {
                        tw.loading = false;
                    }
                    if !c.encoding_label.is_empty() {
                        tw.encoding = c.encoding_label;
                    }
                    if c.rotated {
                        append_text(tw, "\n--- file rotated/truncated ---\n");
                        appended = true;
                    }
                    if c.missing {
                        append_text(tw, "\n--- file missing ---\n");
                        appended = true;
                    }
                    if c.reappeared {
                        append_text(tw, "\n--- file reappeared ---\n");
                        appended = true;
                    }
                    if let Some(n) = c.skipped_bytes {
                        // The reader began part-way in because the history exceeded the load
                        // bound (review #2 finding 5) — the R104 marker semantics, in bytes. AFTER
                        // the rotated/reappeared marker: the skip belongs to the NEW file (review
                        // #4 finding 19).
                        append_text(tw, &format!("--- earlier {n} bytes skipped ---\n"));
                        appended = true;
                    }
                    // Open/read failure (review B2): ONE marker per episode (edge-triggered in the
                    // reader thread), so a locked file no longer sits on "loading..." forever.
                    if let Some(err) = c.error {
                        append_text(tw, &format!("\n--- {err} ---\n"));
                        appended = true;
                    }
                    if let Some(newtext) = c.new_text {
                        append_text(tw, &newtext);
                        appended = true;
                    }
                }
                if appended {
                    tw.loading = false;
                    // Enforce the scrollback cap (BACKLOG §4, R104) AFTER all appends this tick. A
                    // trim shifts every byte offset, so `old_len` and the existing match ranges are
                    // invalid: fall back to a full match refresh in that (rare) case; otherwise
                    // EXTEND for the appended bytes only (review B3): O(chunk), not a rescan per tick.
                    //
                    // The scrollback trim MUST run every append (it bounds the buffer), whether or not
                    // a search rescan is pending; capture whether it trimmed.
                    let trimmed = cap_scrollback(tw);
                    if trimmed {
                        // Trace-only (R146): lets a gate run assert the bound from DirWatch.log.
                        trace_log(&format!(
                            "scrollback trim for {}: dropped {} lines {} bytes so far, buffer now {} bytes",
                            tw.path, tw.dropped_lines, tw.dropped_bytes, tw.text.len()
                        ));
                    }
                    // DEBOUNCE (R162): if a query change is PENDING a deferred rescan, `matches` still
                    // holds the OLD query's result while `search_query` is already the NEW one — so
                    // neither an extend (wrong query) nor a per-append full refresh (would negate the
                    // debounce on a large buffer) is correct. Skip the match update: the pending rescan
                    // recomputes the new query over the whole (now-appended, maybe-trimmed) buffer when
                    // it fires. Otherwise: a trim invalidated every byte offset -> full refresh; else
                    // EXTEND the appended bytes only (review B3): O(chunk), not a rescan per tick.
                    if tw.search_rescan_due.is_some() {
                        // The deferred rescan owns the next scan. But a trim (R104) shifted every
                        // byte offset, so the matches we're holding for the OLD query are now stale
                        // against the trimmed buffer — rendering them slices a line mid-char (panic)
                        // or highlights the wrong bytes and lies about the count (REVIEW-2026-09-18
                        // A1). The pending rescan hasn't run yet (up to DEBOUNCE_TICKS away, and a
                        // fresh keystroke re-arms it), so drop them now; the rescan repopulates them
                        // correctly a moment later. No trim => the offsets are still valid, keep them.
                        if trimmed {
                            tw.matches.clear();
                            tw.current_match = None;
                        }
                    } else if trimmed {
                        tw.refresh_matches();
                    } else {
                        tw.extend_matches(old_len);
                    }
                    if tw.following {
                        tasks.push(snap_to_bottom(tw.scroll_id.clone()));
                    }
                }
            }
            // DEBOUNCE (DECISIONS R162): run any deferred search rescan whose deadline this Tick has
            // reached. Collect the due ids first (immutable borrow), then apply (mutable). A large
            // tail's keystroke burst re-armed the deadline each keystroke; only the final, un-superseded
            // deadline reaches here, so the burst collapses to ONE scan — off the per-keystroke path.
            let due: Vec<window::Id> = state
                .tails
                .iter()
                .filter(|(_, tw)| search::rescan_due(tw.search_rescan_due, state.tick_count))
                .map(|(id, _)| *id)
                .collect();
            for id in due {
                if let Some(tw) = state.tails.get_mut(&id) {
                    tw.search_rescan_due = None;
                    trace_log(&format!(
                        "search debounce: deferred rescan fired for {} ({} bytes, query {:?})",
                        tw.path,
                        tw.text.len(),
                        tw.search_query
                    ));
                }
                tasks.push(apply_search_scan(state, id));
            }
            Task::batch(tasks)
        }
        Message::DirChanged(s) => {
            state.dir_input = s;
            Task::none()
        }
        Message::DepthChanged(s) => {
            state.depth_input = s.chars().filter(|c| c.is_ascii_digit()).collect();
            Task::none()
        }
        Message::PatternsChanged(s) => {
            state.patterns_input = s;
            Task::none()
        }
        Message::Browse => {
            let mut dlg = rfd::FileDialog::new().set_title("Choose a directory to watch");
            let start = state.dir_input.trim();
            if !start.is_empty() && Path::new(start).is_dir() {
                dlg = dlg.set_directory(start);
            }
            if let Some(picked) = dlg.pick_folder() {
                state.dir_input = picked.to_string_lossy().to_string();
            }
            // (No note change: a live ERROR/cap note is not Browse's to clear — review #4 finding 7.)
            Task::none()
        }
        Message::ToggleWatch => {
            if state.runtime.is_some() {
                state.runtime = None;
                state.set_note(NoteKind::Plain, "stopped".to_string());
                Task::none()
            } else {
                // Start: a fresh watch, so every note (error, cap, "stopped") is stale.
                state.note = None;
                start_watch(state)
            }
        }
        Message::Restart => {
            // Restart: a fresh watch (the model is reset, the cap latch cleared) — every note is stale.
            state.note = None;
            start_watch(state)
        }
        Message::OpenSettings => open_settings(state),
        Message::SettingsOpened(id) => {
            // The window is realized. The draft's id was bound at spawn (`window::open` returns the
            // real id synchronously); it is NOT rebound here — a LATE `SettingsOpened` from a window
            // already closed and re-opened used to overwrite the live draft's id with a dead one,
            // leaving Settings unreachable for the session (review #4 finding 15).
            if state.settings_win.as_ref().map(|d| d.id) != Some(id) {
                trace_log(&format!(
                    "SettingsOpened id={id:?} for a window that is no longer the live draft; ignored"
                ));
            }
            Task::none()
        }
        Message::SettingsMaxWindowsChanged(s) => {
            if let Some(d) = state.settings_win.as_mut() {
                d.max_windows = digits(&s);
            }
            Task::none()
        }
        Message::SettingsActiveSecsChanged(s) => {
            if let Some(d) = state.settings_win.as_mut() {
                d.active_seconds = digits(&s);
            }
            Task::none()
        }
        Message::SettingsPollMsChanged(s) => {
            if let Some(d) = state.settings_win.as_mut() {
                d.poll_ms = digits(&s);
            }
            Task::none()
        }
        Message::SettingsTilesPerBoxChanged(s) => {
            if let Some(d) = state.settings_win.as_mut() {
                d.tiles_per_box = digits(&s);
            }
            Task::none()
        }
        Message::SettingsNoOpenToggled(v) => {
            if let Some(d) = state.settings_win.as_mut() {
                d.no_open = v;
            }
            Task::none()
        }
        Message::SettingsOk => settings_ok(state),
        Message::SettingsCancel => settings_cancel(state),
        Message::TileClicked(path) => tile_clicked(state, path),
        Message::MainOpened(id) => {
            state.main_id = Some(id);
            Task::none()
        }
        Message::WindowEvent(id, ev) => {
            // Track the MAIN window's real rect for tail placement (review B5). Only the main window
            // matters; tail/settings events are ignored.
            if Some(id) == state.main_id {
                match ev {
                    window::Event::Opened { position, size } => {
                        if let Some(p) = position {
                            state.main_rect = Some(Rect {
                                x: p.x,
                                y: p.y,
                                w: size.width,
                                h: size.height,
                            });
                            // First sight: the window opened Centered, so the primary screen is
                            // about twice the margin plus the window (+ ~40 px taskbar allowance
                            // on the height, which the centering already accounted for).
                            if state.screen_est.is_none() {
                                let est = (2.0 * p.x + size.width, 2.0 * p.y + size.height);
                                state.screen_est = Some(est);
                                trace_log(&format!(
                                    "main window opened at ({}, {}) size {}x{}; screen estimate {}x{}",
                                    p.x, p.y, size.width, size.height, est.0, est.1
                                ));
                            }
                        } else {
                            trace_log("main window opened; no position reported (Wayland?) - placement falls back to the assumed screen");
                        }
                    }
                    window::Event::Moved(p) => {
                        let r = state.main_rect.get_or_insert(Rect {
                            x: 0.0,
                            y: 0.0,
                            w: MAIN_W,
                            h: MAIN_H,
                        });
                        r.x = p.x;
                        r.y = p.y;
                    }
                    window::Event::Resized(sz) => {
                        if let Some(r) = state.main_rect.as_mut() {
                            r.w = sz.width;
                            r.h = sz.height;
                        }
                    }
                    _ => {}
                }
            }
            Task::none()
        }
        Message::AutoOpenRealized(id) => {
            // The auto-opened tail window is now realized by the OS. FLASH the taskbar NOW (R76/R77).
            // The RAISE is DEFERRED a short beat (R85) then brings the window to the front via winit's
            // `gain_focus` — the raise winner V1 (DECISIONS R101), the same path a CLICK uses.
            if state.tails.contains_key(&id) {
                #[cfg(windows)]
                {
                    state.raise_pending = Some((id, state.tick_count + RAISE_DEFER_TICKS));
                    trace_log(&format!(
                        "AutoOpenRealized id={id:?}: flashing now; deferred raise armed (gain_focus)"
                    ));
                }
                flash_or_attention(id)
            } else {
                trace_log(&format!(
                    "AutoOpenRealized id={id:?}: tail no longer open; skipping attention"
                ));
                Task::none()
            }
        }
        Message::AttemptRaise(id) => {
            // Deferred raise fires now (R85): bring the auto-opened tail to the front (R101).
            if !state.tails.contains_key(&id) {
                Task::none()
            } else {
                deferred_raise(id)
            }
        }
        Message::Noop => Task::none(),
        Message::WindowClosed(id) => {
            if Some(id) == state.main_id {
                // Main window closed: exit the whole app (daemon otherwise runs forever).
                return iced::exit();
            }
            // Settings window closed by its X: drop the draft (== Cancel; live settings untouched).
            if state.settings_win.as_ref().map(|d| d.id) == Some(id) {
                state.settings_win = None;
                return Task::none();
            }
            if let Some(tw) = state.tails.remove(&id) {
                state.model.mark_closed(&tw.path);
                state.path_to_win.remove(&fs_key_str(&tw.path));
            }
            // Fix B (R125): the async close has completed; stop treating this id as closing.
            state.closing.remove(&id);
            Task::none()
        }
        Message::TailScrolled(id, vp) => {
            if let Some(tw) = state.tails.get_mut(&id) {
                // Follow while within a couple of lines of the bottom; pause when scrolled up.
                // ABSOLUTE pixels, not a relative fraction (review A5, DECISIONS R99 reversing R65's
                // `>= 0.995`): on a 250k-line file 0.5 % was ~1,200 lines, so scrolling up 30
                // pages still counted as "following" and the next append yanked the view back.
                let abs_y = vp.absolute_offset().y;
                let gap = follow_gap_px(abs_y, vp.bounds().height, vp.content_bounds().height);
                let was = tw.following;
                tw.following = is_following(gap);
                if was != tw.following {
                    trace_log(&format!(
                        "follow {} (abs_y={abs_y:.0} viewport_h={:.0} content_h={:.0} gap={gap:.0}px)",
                        if tw.following { "RESUMED" } else { "PAUSED" },
                        vp.bounds().height,
                        vp.content_bounds().height
                    ));
                }
                // Record the absolute vertical offset so the virtualized view knows which line band
                // is on screen (DECISIONS R69).
                tw.scroll_y = abs_y;
            }
            Task::none()
        }
        Message::CloseTail(id) => {
            // Ctrl-W / Cmd-W: only act on tail windows (never the main window). Closing the window fires a
            // WindowClosed event, which frees the cap slot + recolors the tile - identical to the X.
            if state.tails.contains_key(&id) {
                window::close(id)
            } else {
                Task::none()
            }
        }
        Message::SearchChanged(id, q) => {
            if let Some(tw) = state.tails.get_mut(&id) {
                tw.search_query = q;
                // DEBOUNCE (DECISIONS R162): an empty query (cleared box) always applies inline —
                // clearing is not a scan. A non-empty query scans inline when the buffer is small
                // (instant; no added latency), else defers to the debounce Tick so a typing burst
                // over a large tail collapses to one scan instead of freezing the GUI per keystroke
                // (MAINPC .i54: 277 ms @48 MB, 377 ms @64 MB per keystroke).
                if tw.search_query.is_empty() || search::scan_inline(tw.text.len()) {
                    tw.search_rescan_due = None; // any pending deferral is now moot
                    return apply_search_scan(state, id);
                }
                // Large buffer: defer. Re-arm the deadline (a later keystroke pushes it out). The
                // highlight/count keep the previous query's result until the rescan lands — the
                // ~200 ms debounce feel. No scan on this keystroke.
                tw.search_rescan_due = Some(state.tick_count + search::DEBOUNCE_TICKS);
            }
            Task::none()
        }
        Message::SearchNext(id) => search_step_or_force(state, id),
        Message::SearchPrev(id) => search_step(state, id, false),
        Message::SearchCleared(id) => clear_search(state, id),
        Message::FocusSearch(id) => {
            // Ctrl-F / Cmd-F: focus the tail window's search box (no-op for main/settings windows).
            if let Some(tw) = state.tails.get(&id) {
                focus(tw.search_id.clone())
            } else {
                Task::none()
            }
        }
        Message::ResumeFollow(id) => {
            // The resume chip (R173): re-engage follow and snap to the bottom in one click. Set
            // `following` here so the header LED + the chip's own visibility flip THIS frame, before
            // the snap's `TailScrolled` lands (which would recompute the same value from the new
            // offset anyway). No-op for a main/settings window id.
            if let Some(tw) = state.tails.get_mut(&id) {
                tw.following = true;
                snap_to_bottom(tw.scroll_id.clone())
            } else {
                Task::none()
            }
        }
        Message::EnterPressed(id) => {
            // Enter=OK when the Settings window is focused; Enter=next-match when a TAIL window is
            // focused (standard find UX); no-op elsewhere.
            if state.settings_win.as_ref().map(|d| d.id) == Some(id) {
                settings_ok(state)
            } else if state.tails.contains_key(&id) {
                search_step_or_force(state, id)
            } else {
                Task::none()
            }
        }
        Message::EscPressed(id) => {
            // Esc=Cancel for the Settings window; Esc clears the search box for a focused TAIL window
            // (when it has text - no-op if already empty; the always-visible bar stays). No-op else.
            if state.settings_win.as_ref().map(|d| d.id) == Some(id) {
                settings_cancel(state)
            } else if state
                .tails
                .get(&id)
                .is_some_and(|tw| !tw.search_query.is_empty())
            {
                clear_search(state, id)
            } else {
                Task::none()
            }
        }
    }
}

/// Digits-only filter for the numeric Settings fields (mirrors the Depth field's `DepthChanged`
/// sanitization): a numeric edit box only accepts ASCII digits, so a stray letter never reaches
/// `commit_settings`. An empty string is legal mid-edit; `commit_settings` keeps the previous value
/// for an unparseable (e.g. empty) field.
fn digits(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_digit()).collect()
}

/// Open the Settings window, or raise it if already open (single-instance, the user's choice #2). Seeds
/// the draft from the LIVE settings so the fields show what's actually running (DECISIONS R79).
fn open_settings(state: &mut DirWatch) -> Task<Message> {
    if let Some(d) = &state.settings_win {
        // Already open: raise the existing one rather than spawn a second (the app is foreground, so
        // gain_focus is the right raise here — same as the click-to-front tail path).
        return window::gain_focus(d.id);
    }
    let (id, open_task) = window::open(window::Settings {
        size: Size::new(SETTINGS_W, SETTINGS_H),
        position: window::Position::Centered,
        icon: app_icon(),
        ..Default::default()
    });
    state.settings_win = Some(SettingsDraft::from_live(id, &state.settings));
    open_task.map(Message::SettingsOpened)
}

/// OK: commit the staged fields into the live settings, apply them, and close the window. Applies
/// LIVE without disturbing open tail windows or file state (DECISIONS R79):
///   * max-windows / active-timeout / auto-open -> `SessionModel` fields (no re-scan).
///   * tiles-per-box -> stored; the next `view` re-flows the grid off it (no restart).
///   * poll-interval -> if it CHANGED, restart ONLY the watch runtime thread with the new poll (the
///     tail windows and the model are left intact); if unchanged, the runtime is not touched.
fn settings_ok(state: &mut DirWatch) -> Task<Message> {
    let Some(draft) = state.settings_win.take() else {
        return Task::none();
    };
    let committed = commit_settings(&draft, &state.settings);
    let poll_changed = committed.poll_ms != state.settings.poll_ms;
    state.settings = committed;

    // Model-facing knobs apply live, no re-scan.
    apply_settings_to_model(&mut state.model, &state.settings);
    state.max_windows = state.model.max_windows;

    // Poll interval takes effect LIVE (REVIEW-2026-09-17 finding 6): the watch runtime reads its
    // cadence from a shared atomic, so a poll change updates the running thread in place instead of
    // tearing it down and respawning it. The old restart dropped everything queued in the event
    // channel AND ran a discovery-only first sweep on the replacement, so a file written in that
    // window never became Activity/unread. Storing into the atomic keeps the model, the receiver,
    // and the started state — nothing is lost. The Settings dialog can only change the five tunables
    // (see commit_settings); directory/patterns/depth come from Start/Restart alone, so no other
    // Settings field ever needs a real restart. The OS close of the Settings window is async: mark
    // the id `closing` so `view()` renders blank for its last frame(s) instead of falling through to
    // `main_view` (the fix-B flash class, review #4 finding 8).
    state.closing.insert(draft.id);
    let tasks = vec![window::close(draft.id)];
    if poll_changed {
        let ms = state.settings.poll_ms as u64;
        // Open tails follow the new cadence live (R144) — never restarted (that re-delivers the file).
        for tw in state.tails.values().filter(|tw| tw.stream.poll_ms() != ms) {
            tw.stream.set_poll_ms(ms);
        }
        // The watch runtime follows it live too (finding 6) — never restarted (that dropped events
        // and re-discovered). No-op if the watch is currently stopped (runtime None).
        if let Some(rt) = &state.runtime {
            rt.set_poll_ms(ms);
        }
    }
    Task::batch(tasks)
}

/// Cancel: discard the staged fields and close the window. Live settings are untouched.
fn settings_cancel(state: &mut DirWatch) -> Task<Message> {
    match state.settings_win.take() {
        Some(draft) => {
            state.closing.insert(draft.id); // blank until WindowClosed (finding 8)
            window::close(draft.id)
        }
        None => Task::none(),
    }
}

/// Apply the current query to a tail: recompute matches, select the FIRST, and scroll it into view
/// (review B7). Shared by the inline `SearchChanged` path (small buffer) and the debounce `Tick` path
/// (large buffer, DECISIONS R162), so both produce identical results — the only difference is WHEN.
/// No-op if the id isn't a tail.
fn apply_search_scan(state: &mut DirWatch, id: window::Id) -> Task<Message> {
    if let Some(tw) = state.tails.get_mut(&id) {
        tw.refresh_matches();
        tw.current_match = search::step_match(None, tw.matches.len(), true);
        return jump_to_current_match(state, id, true);
    }
    Task::none()
}

/// Clear a tail window's search (the 'x' button or Esc; DECISIONS R81): empty the query, matches, and
/// current selection so the highlight disappears, then re-focus the box so the user can type a fresh
/// search immediately. Also drops any pending debounce rescan (R162) so it can't fire after a clear.
/// No-op if the id isn't a tail.
fn clear_search(state: &mut DirWatch, id: window::Id) -> Task<Message> {
    if let Some(tw) = state.tails.get_mut(&id) {
        tw.search_query.clear();
        tw.matches.clear();
        tw.current_match = None;
        tw.search_rescan_due = None;
        focus(tw.search_id.clone())
    } else {
        Task::none()
    }
}

/// Step a tail window's search to the next/prev match (with wrap) and scroll it into view (search,
/// DECISIONS R80). No-op if the id isn't a tail or there are no matches.
fn search_step(state: &mut DirWatch, id: window::Id, forward: bool) -> Task<Message> {
    let next = match state.tails.get(&id) {
        Some(tw) => search::step_match(tw.current_match, tw.matches.len(), forward),
        None => return Task::none(),
    };
    if let Some(tw) = state.tails.get_mut(&id) {
        tw.current_match = next;
        // DIAG (review A3 verification): one line per step so a hardware run shows whether ONE
        // Enter produced ONE step ("1 of N" -> "2 of N"), the double-fire the .i28 review found.
        trace_log(&format!(
            "search step {} -> {:?} of {} (query={:?})",
            if forward { "next" } else { "prev" },
            next.map(|i| i + 1),
            tw.matches.len(),
            tw.search_query
        ));
    }
    jump_to_current_match(state, id, false)
}

/// Enter / Next on a tail window (DECISIONS R165). If a large-buffer debounce rescan is PENDING
/// (R162), the displayed matches belong to the PREVIOUS query — stepping them would advance through a
/// query the user has already edited past. So force the pending scan to run NOW (drop the deadline +
/// `apply_search_scan`, which refreshes for the CURRENT query, selects the first match, and jumps),
/// instead of waiting out the ~200 ms debounce. With no rescan pending this is an ordinary forward
/// `search_step`. Prev (`<`) does NOT force (the user's scope call, R165) — it steps the current list.
fn search_step_or_force(state: &mut DirWatch, id: window::Id) -> Task<Message> {
    let pending = state
        .tails
        .get(&id)
        .is_some_and(|tw| tw.search_rescan_due.is_some());
    if pending {
        if let Some(tw) = state.tails.get_mut(&id) {
            tw.search_rescan_due = None; // this Enter/Next owns the scan now
            trace_log(&format!(
                "search force-now: Enter/Next ran the pending rescan for {} ({} bytes, query {:?})",
                tw.path,
                tw.text.len(),
                tw.search_query
            ));
        }
        apply_search_scan(state, id)
    } else {
        search_step(state, id, true)
    }
}

/// Pixels between the bottom of the viewport and the bottom of the content (review A5). Pure so it
/// is unit-testable; the GUI feeds it the scrollable `Viewport`'s absolute offset, bounds height,
/// and content height.
fn follow_gap_px(abs_y: f32, viewport_h: f32, content_h: f32) -> f32 {
    (content_h - (abs_y + viewport_h)).max(0.0)
}

/// Follow (auto-scroll) is engaged while the viewport bottom is within `FOLLOW_SLACK_LINES` lines
/// of the content bottom — an ABSOLUTE test that means the same thing on a 100-line file and a
/// 250k-line file (DECISIONS R99).
const FOLLOW_SLACK_LINES: f32 = 2.0;
fn is_following(gap_px: f32) -> bool {
    gap_px <= FOLLOW_SLACK_LINES * TAIL_LINE_H
}

/// Scroll a tail window so its CURRENT match line is in view (search, DECISIONS R80). `from_query`
/// distinguishes a query edit (jump to the first match) from an explicit next/prev; both just scroll
/// to `current_match`. Selecting a match PAUSES follow (`following=false`) so the auto-scroll-to-
/// bottom doesn't immediately yank the view off the match; the user returns to follow by scrolling
/// back to the bottom. No match (or none selected) => no scroll, follow left as-is.
fn jump_to_current_match(state: &mut DirWatch, id: window::Id, _from_query: bool) -> Task<Message> {
    let Some(tw) = state.tails.get_mut(&id) else {
        return Task::none();
    };
    let Some(ci) = tw.current_match else {
        return Task::none();
    };
    let Some(m) = tw.matches.get(ci).copied() else {
        return Task::none();
    };
    let line = tw.line_of(m.start);
    // Center-ish: put the match line a few lines down from the top of the viewport when possible.
    let target_y = (line as f32 * TAIL_LINE_H - 3.0 * TAIL_LINE_H).max(0.0);
    tw.following = false;
    tw.scroll_y = target_y;
    // Vertical only: leave any horizontal scroll where the user put it.
    scroll_to(
        tw.scroll_id.clone(),
        iced::widget::scrollable::AbsoluteOffset::<Option<f32>> {
            x: None,
            y: Some(target_y),
        },
    )
}

/// Follow = snap to the BOTTOM only. With the tail scrollable now two-directional (review A4),
/// `snap_to_end` would also fling the view to the far RIGHT of the longest line on every append
/// (caught in the .i29 headless tail render); snapping `y` alone keeps the horizontal position.
fn snap_to_bottom(id: iced::widget::Id) -> Task<Message> {
    snap_to(
        id,
        RelativeOffset::<Option<f32>> {
            x: None,
            y: Some(1.0),
        },
    )
}

/// Handle a tile CLICK: open the file's tail window, raise it if already open, or blink the cap.
fn tile_clicked(state: &mut DirWatch, path: String) -> Task<Message> {
    match state.model.try_open(&path) {
        OpenResult::AlreadyOpen => {
            if let Some(id) = state.path_to_win.get(&fs_key_str(&path)) {
                window::gain_focus(*id)
            } else {
                Task::none()
            }
        }
        OpenResult::BlockedByCap => {
            trigger_cap_blink(state);
            Task::none()
        }
        OpenResult::Opened => {
            // Click path: the app is already foreground, so no attention/raise needed on open -
            // discard the open Task's id payload (we already registered with the same id).
            let (_id, open_task) = spawn_tail_window(state, path);
            open_task.discard()
        }
    }
}

/// Whether the verbose operational trace is enabled (review B6, DECISIONS R103). The trace grew
/// `DirWatch_debug.log` on every auto-open / follow flip / placement, unbounded across a
/// long-running session. It is now SILENT by default and enabled only when `DIRWATCH_DEBUG` is set
/// to a non-empty, non-"0" value — an ENV VAR, not a CLI flag, so it works identically from cmd,
/// PowerShell, and a double-click launch (a GUI-subsystem app has no command line on double-click),
/// and it can be turned on to reproduce a LIVE symptom that a single run would miss.
/// Checked once and cached. Real errors (`err_log`) are always logged regardless.
fn trace_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var_os("DIRWATCH_DEBUG")
            .map(|v| !(v.is_empty() || v == "0"))
            .unwrap_or(false)
    })
}

/// Append a build-ID-stamped line to `DirWatch.log` in the log directory (`applog`, DECISIONS
/// R127 — per-user log dir or `--log-dir`; was `DirWatch_debug.log` beside the exe). The
/// `runtests` scripts collect this build's lines from there into the artifact zip. Best-effort; a
/// write failure is swallowed. `tag` distinguishes always-on `ERR` lines from env-gated `DIAG`
/// trace lines.
fn write_log_line(tag: &str, msg: &str) {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    applog::append_line(
        applog::LOG_FILE,
        &format!("{tag} {} [{ms}ms] {msg}", build_info::BUILD_ID),
    );
}

/// A real failure/error — ALWAYS logged (review B6, DECISIONS R103). Use for conditions a user or
/// the user would want a record of even in normal operation: watch/read errors, I/O failures.
pub(crate) fn err_log(msg: &str) {
    write_log_line("ERR", msg);
}

/// The verbose operational trace — logged ONLY when `DIRWATCH_DEBUG` is set (review B6, R103).
/// Silent by default so the log does not grow in normal use; turn it on to instrument a live
/// symptom. Use for the auto-open/raise/placement/follow/search diagnostics.
pub(crate) fn trace_log(msg: &str) {
    if trace_enabled() {
        write_log_line("DIAG", msg);
    }
}

/// What the auto-open path should do about the freshly-opened window, given the cap-gate result.
/// A PURE decision, split out so the choice is unit-testable off-hardware (the taskbar flash itself
/// cannot be - no foreground-lock/taskbar under xvfb; DECISIONS R73/R75). Mirrors the R71
/// auto-open-decision test pattern (assert the model-facing decision, leave the window to xvfb).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoOpenAction {
    /// Opened: spawn the window and REQUEST USER ATTENTION (taskbar flash) - see below.
    FlashAttention,
    /// Over cap: blink the count, spawn nothing.
    BlinkCap,
    /// Already open (cap gate is the single authority; the wants-open check already excluded these).
    Nothing,
}

/// PURE: map the cap-gate result to the auto-open action. No `state`, no iced - just the decision.
fn auto_open_action(result: OpenResult) -> AutoOpenAction {
    match result {
        OpenResult::Opened => AutoOpenAction::FlashAttention,
        OpenResult::BlockedByCap => AutoOpenAction::BlinkCap,
        OpenResult::AlreadyOpen => AutoOpenAction::Nothing,
    }
}

/// AUTO-OPEN a file's tail window on activity (step 4b, DECISIONS R71). Called from the Tick when
/// `mark_activity` reported the file WANTS to open (not `--no-open`, not user-closed, not already
/// open). Goes through the SAME `try_open` cap gate as a click, so an auto-open past the cap blinks
/// the count exactly like an over-cap click; `AlreadyOpen` is a no-op (the wants-open check already
/// excluded open files, but the cap gate is the single authority).
///
/// FLASH, don't raise (DECISIONS R75/R76). .i13 tried `window::gain_focus` to raise the tail; Windows
/// foreground-lock blocked it (R73). .i14 switched to `request_user_attention(Critical)` (the
/// OS-sanctioned taskbar flash for a background app) but batched it against the just-created id AT
/// SPAWN - and on the user's Windows it did NOT flash (taskbar button appeared immediately but never
/// flashed - R75 hardware result). DIAGNOSIS (.i15): the attention request likely fired before the
/// window was REALIZED, so winit had no live window to flash. .i15 defers it: the open Task resolves
/// (payload = the window id) only once the window is realized, mapped to `AutoOpenRealized`, which
/// requests attention THEN (`update`, below). `trace_log` records both the deferral and the realized
/// request so the user's next run is conclusive (ATTEMPT 2; Instrument-don't-guess). The CLICK path keeps
/// `gain_focus` and needs no attention (the app is already foreground). the user's choice A (R73).
fn auto_open(state: &mut DirWatch, path: String) -> Task<Message> {
    match auto_open_action(state.model.try_open(&path)) {
        AutoOpenAction::FlashAttention => {
            let (id, open_task) = spawn_tail_window(state, path);
            trace_log(&format!(
                "auto_open: spawned tail id={id:?}; deferring attention until AutoOpenRealized"
            ));
            // Request attention only AFTER the window is realized (open Task resolves with its id).
            open_task.map(Message::AutoOpenRealized)
        }
        AutoOpenAction::BlinkCap => {
            // The write happened and nobody saw it: the file is UNREAD, exactly as when the user
            // had closed it (review #4 finding 5 — it used to decay to Idle after `active_seconds`).
            state.model.mark_unread_if_closed(&path);
            trigger_cap_blink(state);
            Task::none()
        }
        AutoOpenAction::Nothing => Task::none(),
    }
}

/// Ask for the user's attention on a realized auto-opened tail window (DECISIONS R77).
///
/// ON WINDOWS: iced/winit's `request_user_attention` is a silent no-op on the user's build (proven by
/// the .i15 DIAG log, R76 - it fired against the realized window and nothing flashed). So we call the
/// Win32 `FlashWindowEx` directly: fetch the window's raw `HWND` via iced's public `window::run`
/// (which hands the callback a `&dyn Window: HasWindowHandle`), pattern-match `RawWindowHandle::Win32`
/// (the typed, documented ecosystem contract - not an undocumented "the u64 is the HWND" cast), and
/// flash the taskbar button with `FLASHW_TRAY | FLASHW_TIMERNOFG` (flash until the app is foregrounded).
/// The effect happens inside the callback; the Task resolves to `Noop`.
///
/// ON EVERYTHING ELSE (macOS/X11): keep `request_user_attention(Critical)` - it WORKS there (dock
/// bounce / WM urgency hint), so the Win32 crate is never linked on those targets (cfg-gated dep).
#[cfg(windows)]
fn flash_or_attention(id: window::Id) -> Task<Message> {
    use window::raw_window_handle::RawWindowHandle;
    // FLASH the taskbar inline at realization (the RAISE is deferred to `deferred_raise` a few Ticks
    // later, DECISIONS R85). The flash is the cue AND the fallback: a SUCCESSFUL later raise cancels
    // the flash (that's `FLASHW_TIMERNOFG` behavior - it flashes until the window is foregrounded), so
    // when the raise works the flash quietly stops, and when the raise is refused the flash remains.
    window::run(id, move |win: &dyn window::Window| {
        match win.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => flash_taskbar(h.hwnd.get()),
            other => trace_log(&format!(
                "AutoOpenRealized id={id:?}: no Win32 handle (got {other:?}); no flash"
            )),
        }
    })
    .map(|()| Message::Noop)
}

/// Ticks to wait after realization before the DEFERRED raise fires (DECISIONS R85). At 100 ms/tick,
/// 3 ticks == ~300 ms - long enough for winit to finish settling the realized window (so our raise
/// isn't overwritten, the .i21 failure) but short enough to feel immediate.
#[cfg(windows)]
const RAISE_DEFER_TICKS: u64 = 3;

/// The DEFERRED raise entry point (DECISIONS R85, R101). Runs on a LATER event-loop turn than
/// realization, so winit has settled the window. Brings the auto-opened tail to the front with
/// winit's own `gain_focus` — the SAME path a CLICK uses, which always worked. The .i23 variant
/// matrix (R86/R100) proved every candidate performed identically on the user's Windows (all reported
/// `is_foreground=true`, and V1 visibly raised in the desktop screenshot), so the simplest —
/// `gain_focus` alone, no Win32, no `unsafe` — is the winner (R101). Same call on every platform:
/// gain_focus is winit's cross-platform "focus this window" and works on Windows, macOS, and X11.
fn deferred_raise(id: window::Id) -> Task<Message> {
    window::gain_focus(id)
}

/// Call Win32 `FlashWindowEx(FLASHW_TRAY | FLASHW_TIMERNOFG)` on the given HWND value. `hwnd_isize`
/// is the `NonZeroIsize` HWND out of `raw-window-handle`'s `Win32WindowHandle`. One contained
/// `unsafe` FFI call; a failed flash is logged, not fatal. (DECISIONS R77.)
#[cfg(windows)]
fn flash_taskbar(hwnd_isize: isize) {
    use std::ffi::c_void;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        FlashWindowEx, FLASHWINFO, FLASHWINFO_FLAGS, FLASHW_TIMERNOFG, FLASHW_TRAY,
    };
    let info = FLASHWINFO {
        cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
        hwnd: HWND(hwnd_isize as *mut c_void),
        dwFlags: FLASHWINFO_FLAGS(FLASHW_TRAY.0 | FLASHW_TIMERNOFG.0),
        uCount: 0,
        dwTimeout: 0,
    };
    // SAFETY: `info` is a fully-initialized FLASHWINFO with a valid cbSize and an HWND obtained from
    // the realized window's own handle this same turn; FlashWindowEx reads it by const pointer and
    // does not retain it. The return BOOL is the window's prior foreground state, not an error code.
    let ret = unsafe { FlashWindowEx(&info) };
    trace_log(&format!(
        "FlashWindowEx(FLASHW_TRAY|FLASHW_TIMERNOFG) called on hwnd={hwnd_isize:#x}; ret={}",
        ret.as_bool()
    ));
}

/// Non-Windows: `request_user_attention` works (macOS dock bounce / X11 urgency hint). (R77.)
#[cfg(not(windows))]
fn flash_or_attention(id: window::Id) -> Task<Message> {
    trace_log(&format!(
        "AutoOpenRealized id={id:?}: requesting user attention (Critical) [non-windows path]"
    ));
    window::request_user_attention(id, Some(window::UserAttention::Critical))
}

/// Record an over-cap open attempt so the `Open Windows: N of M` count blinks for
/// `blink::CAP_BLINK_MS` (DECISIONS R29 item 5 / R71). Replaces the old transient text note
/// (the user's choice 3A) - the blinking count IS the cap warning.
fn trigger_cap_blink(state: &mut DirWatch) {
    state.cap_blink_start_ms = Some(state.now_ms());
    // The blinking count IS the warning; the text note is untouched (review #4 finding 7).
}

/// Spawn a tail window for `path` (the caller has already `try_open`ed and gotten `Opened`). Shared
/// by the click and auto-open paths so there is ONE window-spawn mechanism, not two that could
/// drift. Edge-aware + cascade placement; registers the `TailWin` synchronously with the real id
/// (DECISIONS R66 - avoids the .i7 flash/crash).
///
/// Returns the real window id AND the RAW open Task (its payload is the same id, resolved once the
/// window is REALIZED by the OS). The click path discards it (`.discard()`); the auto-open path maps
/// it to `AutoOpenRealized` so it can request attention AFTER realization (.i15, DECISIONS R76).
/// Where to open the next tail window (review #2 finding 10, DECISIONS R118). Pure, so unit-testable.
///
/// A screen estimate is trustworthy only when it is WIDER than the main window and positive. A bogus
/// estimate — e.g. the main window on a left-hand monitor at negative x makes the `2*x + w` estimate
/// come out `<= MAIN_W` or negative — would make [`next_position`] treat the screen as tiny and clamp
/// every tail onto the primary monitor's corner, far from the file list. When the estimate is
/// unusable but the REAL main rect is known, we anchor an assumed-size screen AT the main window so
/// placement lands beside it and the on-screen clamp can't drag the tail onto another monitor. With
/// neither, we fall back to the assumed screen centred as before.
fn tail_position(
    screen_est: Option<(f32, f32)>,
    main_rect: Option<Rect>,
    open_index: usize,
) -> (f32, f32) {
    let usable = screen_est.filter(|&(w, h)| w > MAIN_W && h > 0.0);
    let (screen, main) = match (usable, main_rect) {
        (Some((sw, sh)), Some(m)) => (
            Rect {
                x: 0.0,
                y: 0.0,
                w: sw,
                h: sh,
            },
            m,
        ),
        (Some((sw, sh)), None) => (
            Rect {
                x: 0.0,
                y: 0.0,
                w: sw,
                h: sh,
            },
            Rect {
                x: (sw - MAIN_W) / 2.0,
                y: (sh - MAIN_H) / 2.0,
                w: MAIN_W,
                h: MAIN_H,
            },
        ),
        // Estimate unusable, real main rect known: anchor the assumed-size screen AT the main window
        // so `next_position` places beside it and the clamp keeps it on the main's monitor.
        (None, Some(m)) => (
            Rect {
                x: m.x,
                y: m.y,
                w: ASSUMED_SCREEN_W,
                h: ASSUMED_SCREEN_H,
            },
            m,
        ),
        (None, None) => (
            Rect {
                x: 0.0,
                y: 0.0,
                w: ASSUMED_SCREEN_W,
                h: ASSUMED_SCREEN_H,
            },
            Rect {
                x: (ASSUMED_SCREEN_W - MAIN_W) / 2.0,
                y: (ASSUMED_SCREEN_H - MAIN_H) / 2.0,
                w: MAIN_W,
                h: MAIN_H,
            },
        ),
    };
    next_position(PlaceInput {
        screen,
        win_w: TAIL_W,
        win_h: TAIL_H,
        main,
        open_index,
    })
}

fn spawn_tail_window(state: &mut DirWatch, path: String) -> (window::Id, Task<window::Id>) {
    // open_index = tails already open (before this one).
    let idx = state.tails.len();
    // REAL main-window rect + estimated screen when known (review B5, DECISIONS R97); a bogus screen
    // estimate is rejected and placement falls back to the main window (finding 10, R118). Pure +
    // unit-tested in `tail_position`.
    let (px, py) = tail_position(state.screen_est, state.main_rect, idx);
    trace_log(&format!(
        "placing tail #{idx} at ({px:.0}, {py:.0}); main={} screen_est={}",
        match state.main_rect {
            Some(m) => format!("({:.0},{:.0} {:.0}x{:.0}) real", m.x, m.y, m.w, m.h),
            None => "assumed".to_string(),
        },
        match state.screen_est {
            Some((w, h)) if w > MAIN_W && h > 0.0 => format!("{w:.0}x{h:.0} usable"),
            Some((w, h)) => format!("{w:.0}x{h:.0} REJECTED (<= main width) -> beside main"),
            None => "assumed".to_string(),
        }
    ));
    // window::open hands back the REAL id synchronously (it is `Id::unique()` internally), and iced
    // renders the window with that id. So we register the TailWin NOW, before the window first
    // renders - otherwise `view` can't find it in `tails` on the first frame and falls through to
    // `main_view`, flashing the main window's contents into the new window (the .i7 flash).
    // Registering synchronously also closes the gap where `open_count` (bumped by try_open) and the
    // `tails` map disagreed - the likely source of the .i7 crash. (DECISIONS R66.)
    let (id, open_task) = window::open(window::Settings {
        size: Size::new(TAIL_W, TAIL_H),
        position: window::Position::Specific(Point::new(px, py)),
        icon: app_icon(),
        ..Default::default()
    });
    // Spawn the BACKGROUND reader; the window shows "loading..." until the first chunk. No
    // synchronous read here, so opening a large file is instant and non-blocking.
    // Open by the REAL path (R143) — the display string is lossy for a non-UTF-8 name — at the
    // watch poll interval (R144).
    let real = state
        .real_paths
        .get(&path)
        .cloned()
        .unwrap_or_else(|| PathBuf::from(&path));
    let stream = TailStream::start(real, state.settings.poll_ms as u64);
    state.tails.insert(
        id,
        TailWin {
            path: path.clone(),
            stream,
            text: String::new(),
            line_starts: Vec::new(),
            encoding: String::new(),
            loading: true,
            following: true,
            scroll_y: 0.0,
            scroll_id: iced::widget::Id::unique(),
            search_query: String::new(),
            matches: Vec::new(),
            current_match: None,
            search_rescan_due: None,
            search_id: iced::widget::Id::unique(),
            dropped_lines: 0,
            dropped_bytes: 0,
            has_drop_marker: false,
            pending_cr: false,
        },
    );
    state.path_to_win.insert(fs_key_str(&path), id);
    (id, open_task)
}

/// Record the real `PathBuf` behind an event's display string (R143), so a tail can open the file
/// even when its name is not valid UTF-8. A no-op for the path-less events.
fn remember_real_path(map: &mut HashMap<String, PathBuf>, ev: &WatchEvent) {
    let p = match ev {
        WatchEvent::Discovered(p)
        | WatchEvent::Activity(p)
        | WatchEvent::Missing(p)
        | WatchEvent::Reappeared(p) => p,
        WatchEvent::Error(_) | WatchEvent::Recovered => return,
    };
    let display = p.to_string_lossy();
    // Only names that are NOT round-trippable need remembering; a clean path reopens by its string.
    if Path::new(display.as_ref()) != p.as_path() {
        map.entry(display.into_owned()).or_insert_with(|| p.clone());
    }
}

/// Apply one watch event to the model. Returns `Some(path)` when the event is activity on a file
/// that WANTS to auto-open (step 4b): `mark_activity` reported wants-open (not `--no-open`, not
/// user-closed, not already open). The Tick collects those and calls `auto_open`. A write to a
/// CLOSED file that does NOT auto-open marks it unread, so it renders `Unread` once its active
/// window decays (the closed-and-changed state).
fn apply_event(model: &mut SessionModel, ev: WatchEvent) -> Option<String> {
    let now = now_secs();
    match ev {
        WatchEvent::Discovered(p) => {
            // A brand-new dir past MAX_DIR_BOXES is dropped inside add_or_drop (which latches
            // dir_overflow); nothing more to do here. get_or_add now returns Option (review #3
            // finding 6, R124) — the drop case is simply ignored. A sweep that LISTS the file is
            // proof it exists: clear Missing (review #4 finding 16 — after a Settings poll-only
            // restart the fresh service reports a known-but-Missing file as Discovered only).
            if let Some(e) = model.get_or_add(&p.to_string_lossy()) {
                SessionModel::clear_missing(e);
            }
            None
        }
        WatchEvent::Activity(p) => activity(model, &p.to_string_lossy(), now),
        WatchEvent::Missing(p) => {
            model.mark_missing(&p.to_string_lossy(), now);
            None
        }
        WatchEvent::Reappeared(p) => {
            // Reappeared clears Missing and nothing else (review #2 finding 2, DECISIONS R111):
            // the sweep sends a SEPARATE Activity when the stamp really changed, so treating the
            // reappearance itself as a write made every file auto-open after a root blip.
            // get_or_add now returns Option (review #3 finding 6, R124): a reappeared file whose dir
            // was dropped for the cap has no entry to clear — correct, it was never shown.
            if let Some(e) = model.get_or_add(&p.to_string_lossy()) {
                SessionModel::clear_missing(e);
            }
            None
        }
        // Handled by the Tick before reaching here (status-strip note); no-ops on the model.
        WatchEvent::Error(_) | WatchEvent::Recovered => None,
    }
}

/// Shared activity handling: mark the write, and either report it wants to auto-open, or (if it
/// stays closed) mark it unread. Returns the path to auto-open, if any.
fn activity(model: &mut SessionModel, path: &str, now: i64) -> Option<String> {
    if model.mark_activity(path, now) {
        Some(path.to_string())
    } else {
        // Not auto-opening. If it's a closed file that changed, it's now unread (renders Unread
        // after its active window decays). mark_unread_if_closed no-ops for an open file.
        model.mark_unread_if_closed(path);
        None
    }
}

/// Which view a window id gets. PURE (no widgets) so the dispatch — including the "closing window
/// renders blank, never main_view" rule — is unit-testable (review #4 test-quality note: the
/// `.i38` test for fix B never called `view()`, and the Settings analogue slipped through).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewKind {
    Tail,
    Settings,
    /// A window we've asked the OS to close (tail on Restart/Ctrl-W, Settings on OK/Cancel), whose
    /// `WindowClosed` hasn't arrived yet (review #2 finding 18 / i38 fix B, R125; review #4 finding
    /// 8). Rendered blank for its last frame(s) instead of falling through to the main UI.
    Closing,
    Main,
}

fn view_kind(state: &DirWatch, id: window::Id) -> ViewKind {
    if state.tails.contains_key(&id) {
        ViewKind::Tail
    } else if state.settings_win.as_ref().map(|d| d.id) == Some(id) {
        ViewKind::Settings
    } else if state.closing.contains(&id) {
        ViewKind::Closing
    } else {
        ViewKind::Main
    }
}

/// Per-window view: main window vs. a tail window vs. Settings vs. a closing placeholder.
fn view(state: &DirWatch, id: window::Id) -> Element<'_, Message> {
    match view_kind(state, id) {
        ViewKind::Tail => tail_view(id, &state.tails[&id]),
        ViewKind::Settings => settings_view(state, state.settings_win.as_ref().unwrap()),
        ViewKind::Closing => container(text("")).into(),
        ViewKind::Main => main_view(state),
    }
}

/// Run the iced DAEMON (multi-window). The first window is opened from `boot`.
pub fn run(cfg: WatchConfig, opts: ModelOpts) -> iced::Result {
    iced::daemon(move || boot(cfg.clone(), opts.clone()), update, view)
        .title(title)
        .theme(theme)
        .subscription(subscription)
        .run()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A TailWin with no live stream, for exercising the pure line-index logic (`append_text`,
    /// `line_count`, `slice_text`) off-hardware. The stream points at a path that never exists; its
    /// reader thread just reports "missing" and is dropped (joined) when the struct drops. We only
    /// test the text/line bookkeeping, never the stream.
    fn fixture() -> TailWin {
        let dir = std::env::temp_dir().join(format!("dwgui_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let _ = applog::init(Some(&dir.join("logs")));
        TailWin {
            path: "test".into(),
            stream: TailStream::start(
                PathBuf::from("/nonexistent/dirwatch-test-path-that-never-exists"),
                100,
            ),
            text: String::new(),
            line_starts: Vec::new(),
            encoding: String::new(),
            loading: true,
            following: true,
            scroll_y: 0.0,
            scroll_id: iced::widget::Id::unique(),
            search_query: String::new(),
            matches: Vec::new(),
            current_match: None,
            search_rescan_due: None,
            search_id: iced::widget::Id::unique(),
            dropped_lines: 0,
            dropped_bytes: 0,
            has_drop_marker: false,
            pending_cr: false,
        }
    }
    #[test]
    fn append_builds_line_index_incrementally() {
        let mut tw = fixture();
        append_text(&mut tw, "alpha\nbeta\n");
        // line_starts: [0, 6, 11]; text ends in '\n' so the final offset is a sentinel.
        assert_eq!(tw.line_starts, vec![0, 6, 11]);
        assert_eq!(tw.line_count(), 2);

        // A second append continues the index without rescanning.
        append_text(&mut tw, "gamma\n");
        assert_eq!(tw.line_starts, vec![0, 6, 11, 17]);
        assert_eq!(tw.line_count(), 3);
    }
    #[test]
    fn line_count_handles_no_trailing_newline() {
        let mut tw = fixture();
        append_text(&mut tw, "a\nb\nc"); // no trailing '\n'
        assert_eq!(tw.line_starts, vec![0, 2, 4]);
        assert_eq!(tw.line_count(), 3);
    }

    // --- review 2026-09-16 finding 1 (DECISIONS R138): line-ending normalisation ---
    /// The pure normaliser, exercised against the review's probe strings. The property under test:
    /// the output contains a `\n` for exactly each LOGICAL line break and NO other char the shaper
    /// would treat as a paragraph separator (`\r`, U+0085, U+2028, U+2029, U+001C..=U+001E).
    fn norm(s: &str) -> String {
        let mut out = String::new();
        let pc = normalize_line_endings(s, false, &mut out);
        // A trailing held-back `\r` (pending) is not yet emitted; for these single-shot probes we
        // flush it as a lone CR so the assertions see the whole input.
        if pc {
            out.push(CR_PLACEHOLDER);
        }
        out
    }
    #[test]
    fn normalize_crlf_becomes_lf_only() {
        assert_eq!(norm("a\r\nb\r\nc"), "a\nb\nc");
        assert_eq!(norm("é\r\nb\r\nc"), "é\nb\nc"); // the non-ASCII case that doubled before
        assert_eq!(norm("a\r\nb\u{fffd}\r\nc"), "a\nb\u{fffd}\nc"); // the U+FFFD DirWatch inserts
    }
    #[test]
    fn normalize_lone_cr_becomes_a_single_visible_cell_not_a_break() {
        // A bare-CR progress line must stay ONE logical line: no `\n`, no `\r` in the output.
        let o = norm("x\rprogress\ry");
        assert!(!o.contains('\n'));
        assert!(!o.contains('\r'));
        assert_eq!(o, format!("x{CR}progress{CR}y", CR = CR_PLACEHOLDER));
    }
    #[test]
    fn normalize_neutralises_other_paragraph_separators_and_c0_controls() {
        // NEL, LS, PS, U+001C..1E, and a stray C0 control (BEL) must NOT become breaks.
        for sep in [
            "\u{85}", "\u{2028}", "\u{2029}", "\u{1c}", "\u{1d}", "\u{1e}", "\u{7}",
        ] {
            let o = norm(&format!("a{sep}b"));
            assert!(!o.contains('\n'), "sep {sep:?} leaked a newline");
            assert!(!o.contains('\r'), "sep {sep:?} leaked a CR");
            assert_eq!(o, "a\u{fffd}b", "sep {sep:?} not neutralised");
        }
        // \t is left intact (the shaper does not break on it).
        assert_eq!(norm("a\tb"), "a\tb");
    }
    #[test]
    fn append_crlf_indexes_one_line_per_logical_line() {
        let mut tw = fixture();
        append_text(&mut tw, "é\r\nb\r\n");
        assert_eq!(tw.text, "é\nb\n");
        assert_eq!(tw.line_count(), 2, "CRLF must be one row per line, not two");
    }
    #[test]
    fn append_handles_a_crlf_split_across_two_chunks() {
        // The reader's 4 MB pieces (or a decode boundary) can split `\r`|`\n`. The pending-CR flag
        // must join them into a single `\n`, not a lone-CR placeholder + an empty line.
        let mut tw = fixture();
        append_text(&mut tw, "é\r"); // ends on a bare \r — held back
        append_text(&mut tw, "\nb\r\n"); // opens with the partner \n
        assert_eq!(tw.text, "é\nb\n");
        assert_eq!(tw.line_count(), 2);
    }
    #[test]
    fn scrollback_cap_drops_oldest_lines_and_rebuilds_the_index() {
        // BACKLOG §4 / DECISIONS R104: past the cap, drop whole oldest lines, insert a marker,
        // rebuild line_starts. Tested with a tiny cap so no 64 MB allocation is needed.
        let mut tw = fixture();
        // 10 lines of "lineNN\n" (7 bytes each) = 70 bytes.
        for i in 0..10 {
            append_text(&mut tw, &format!("line{i:02}\n"));
        }
        assert_eq!(tw.text.len(), 70);
        let before_lines = tw.line_count();

        // Cap 40, trim to 20: must drop enough oldest lines to get at/under 20 bytes of kept tail.
        let trimmed = cap_scrollback_to(&mut tw, 40, 20);
        assert!(trimmed, "over-cap buffer must trim");

        // The buffer now starts with the dropped-lines marker and keeps only the newest lines.
        assert!(
            tw.text.starts_with("--- earlier "),
            "trimmed buffer must start with the dropped-lines marker, got: {:?}",
            &tw.text[..tw.text.len().min(30)]
        );
        assert!(tw.text.contains("dropped ---\n"));
        // The most recent line survived; an early line did not.
        assert!(tw.text.contains("line09\n"), "newest line must be kept");
        assert!(!tw.text.contains("line00\n"), "oldest line must be dropped");
        // Kept tail (excluding the marker line) is at/under the trim target.
        let tail_bytes = tw.text.len() - (tw.text.find('\n').unwrap() + 1);
        assert!(
            tail_bytes <= 20,
            "kept tail {tail_bytes} must be <= trim_to"
        );

        // line_starts is consistent with the rebuilt buffer: first is 0, each follows a '\n',
        // and the count matches an independent scan.
        assert_eq!(tw.line_starts[0], 0);
        let independent: Vec<usize> = std::iter::once(0)
            .chain(
                tw.text
                    .bytes()
                    .enumerate()
                    .filter(|&(_, b)| b == b'\n')
                    .map(|(i, _)| i + 1),
            )
            .collect();
        assert_eq!(tw.line_starts, independent, "index must match a fresh scan");
        assert!(tw.line_count() < before_lines);
    }
    #[test]
    fn scrollback_cap_is_a_noop_under_the_cap() {
        let mut tw = fixture();
        append_text(&mut tw, "small\npayload\n");
        let saved = tw.text.clone();
        let saved_idx = tw.line_starts.clone();
        assert!(!cap_scrollback_to(&mut tw, 1024, 512));
        assert_eq!(tw.text, saved);
        assert_eq!(tw.line_starts, saved_idx);
    }
    #[test]
    fn append_across_a_line_boundary_stays_consistent() {
        // Simulate a chunk that ends mid-line, then the rest arrives.
        let mut tw = fixture();
        append_text(&mut tw, "he");
        append_text(&mut tw, "llo\nwor");
        append_text(&mut tw, "ld\n");
        assert_eq!(tw.text, "hello\nworld\n");
        assert_eq!(tw.line_count(), 2);
        assert_eq!(tw.slice_text(0, 2), "hello\nworld");
    }
    #[test]
    fn slice_text_returns_exactly_the_requested_lines() {
        let mut tw = fixture();
        append_text(&mut tw, "l0\nl1\nl2\nl3\nl4\n");
        assert_eq!(tw.line_count(), 5);
        // A middle band, no leading/trailing blank line.
        assert_eq!(tw.slice_text(1, 4), "l1\nl2\nl3");
        // From the top.
        assert_eq!(tw.slice_text(0, 2), "l0\nl1");
        // To the end (last == total): includes the final real line, no trailing blank.
        assert_eq!(tw.slice_text(3, 5), "l3\nl4");
    }
    #[test]
    fn slice_text_keeps_every_blank_line_in_the_band() {
        // REGRESSION (review #2 finding 3): a band ending in blank lines rendered fewer rows than
        // `visible.rs` sized spacers for ("l0\nl1\n\n\n\nl5" sliced [0,5) came back as "l0\nl1").
        // The invariant: the slice for [first,last) has exactly last-first rows.
        let mut tw = fixture();
        append_text(&mut tw, "l0\nl1\n\n\n\nl5\nl6\n"); // lines 2,3,4 are blank
        assert_eq!(tw.line_count(), 7);
        for first in 0..7 {
            for last in (first + 1)..=7 {
                let s = tw.slice_text(first, last);
                assert_eq!(
                    s.split('\n').count(),
                    last - first,
                    "slice [{first},{last}) = {s:?}"
                );
            }
        }
        assert_eq!(tw.slice_text(0, 5), "l0\nl1\n\n\n");
        assert_eq!(tw.slice_text(2, 5), "\n\n");
    }

    // --- .i33 (review #1 B12 closed, DECISIONS R115): the per-line render bound ---
    #[test]
    fn slice_text_empty_and_out_of_range_are_safe() {
        let mut tw = fixture();
        append_text(&mut tw, "only\n");
        assert_eq!(tw.slice_text(0, 0), "");
        assert_eq!(tw.slice_text(5, 9), ""); // past the end
    }
    #[test]
    fn empty_buffer_has_zero_lines() {
        let tw = fixture();
        assert_eq!(tw.line_count(), 0);
        assert_eq!(tw.slice_text(0, 1), "");
    }

    // --- step 5b (.i18): tail search line mapping (DECISIONS R80) ---
    #[test]
    fn line_of_maps_byte_offset_to_its_line() {
        // "l0\nl1\nl2\n" -> line_starts [0,3,6,9]. Offsets within each line map to that line index.
        let mut tw = fixture();
        append_text(&mut tw, "l0\nl1\nl2\n");
        assert_eq!(tw.line_of(0), 0); // start of l0
        assert_eq!(tw.line_of(1), 0); // inside l0
        assert_eq!(tw.line_of(3), 1); // start of l1
        assert_eq!(tw.line_of(5), 1); // inside l1
        assert_eq!(tw.line_of(6), 2); // start of l2
    }
    #[test]
    fn clearing_the_query_empties_matches_and_selection() {
        // Through `update(SearchCleared)` (review #4 test-quality note: the old test copied
        // `clear_search`'s three field writes by hand).
        let (mut app, id) = app_with_tail("error a\nerror b\n");
        let _ = update(&mut app, Message::SearchChanged(id, "error".into()));
        assert_eq!(app.tails[&id].matches.len(), 2);
        assert_eq!(app.tails[&id].current_match, Some(0));
        let _ = update(&mut app, Message::SearchCleared(id));
        let tw = &app.tails[&id];
        assert!(tw.search_query.is_empty());
        assert!(tw.matches.is_empty());
        assert_eq!(tw.current_match, None);
        // Esc with an EMPTY query is a no-op (the bar stays; nothing to clear).
        let _ = update(&mut app, Message::EscPressed(id));
        assert!(app.tails[&id].search_query.is_empty());
    }
    #[test]
    fn small_buffer_search_scans_inline_no_debounce() {
        // Below the 4 MB threshold, a keystroke scans immediately (as before): matches present, no
        // deferred rescan armed. (DECISIONS R162.)
        let (mut app, id) = app_with_tail("error one\nerror two\nok\n");
        assert!(app.tails[&id].text.len() < search::DEBOUNCE_THRESHOLD_BYTES);
        let _ = update(&mut app, Message::SearchChanged(id, "error".into()));
        let tw = &app.tails[&id];
        assert_eq!(tw.matches.len(), 2, "small buffer scans inline immediately");
        assert_eq!(tw.current_match, Some(0));
        assert_eq!(
            tw.search_rescan_due, None,
            "no debounce armed below the threshold"
        );
    }

    #[test]
    fn large_buffer_search_is_debounced_then_fires_on_tick() {
        // At/above the threshold, a keystroke DEFERS: the query is stored and a rescan deadline is
        // armed, but no scan runs yet (matches stay empty for the new query). A Tick reaching the
        // deadline runs the scan. (DECISIONS R162 — this is the per-keystroke freeze removed.)
        let mut big = String::with_capacity(search::DEBOUNCE_THRESHOLD_BYTES + 4096);
        // One matching line, then filler past the threshold so find_matches would have real work.
        big.push_str("needle here\n");
        while big.len() < search::DEBOUNCE_THRESHOLD_BYTES + 2048 {
            big.push_str("just a plain filler line with no hits at all in it whatsoever ok\n");
        }
        let (mut app, id) = app_with_tail(&big);
        assert!(app.tails[&id].text.len() >= search::DEBOUNCE_THRESHOLD_BYTES);

        let t0 = app.tick_count;
        let _ = update(&mut app, Message::SearchChanged(id, "needle".into()));
        let tw = &app.tails[&id];
        assert_eq!(tw.search_query, "needle");
        assert!(
            tw.matches.is_empty(),
            "large-buffer keystroke does NOT scan inline"
        );
        assert_eq!(
            tw.search_rescan_due,
            Some(t0 + search::DEBOUNCE_TICKS),
            "a rescan deadline is armed DEBOUNCE_TICKS out"
        );

        // Ticks advance to the deadline; the deferred scan then fires exactly once.
        for _ in 0..search::DEBOUNCE_TICKS {
            let _ = update(&mut app, Message::Tick);
        }
        let tw = &app.tails[&id];
        assert_eq!(tw.matches.len(), 1, "the deferred rescan found the match");
        assert_eq!(tw.current_match, Some(0));
        assert_eq!(
            tw.search_rescan_due, None,
            "the deadline is cleared once it fires"
        );
    }

    #[test]
    fn a_large_buffer_keystroke_burst_rearms_and_scans_once() {
        // Rapid keystrokes each re-arm the deadline; only the final one's deadline reaches a Tick, so
        // the burst collapses to a single scan (the whole point of the debounce). (DECISIONS R162.)
        let mut big = String::with_capacity(search::DEBOUNCE_THRESHOLD_BYTES + 4096);
        big.push_str("alpha\n");
        while big.len() < search::DEBOUNCE_THRESHOLD_BYTES + 2048 {
            big.push_str("filler line here with nothing to match on this row at all fine\n");
        }
        let (mut app, id) = app_with_tail(&big);

        // Three keystrokes "a","al","alpha", each on a separate turn; the deadline moves forward each
        // time. No Tick between them, so no scan runs during the burst.
        for q in ["a", "al", "alpha"] {
            let _ = update(&mut app, Message::SearchChanged(id, q.into()));
            assert!(
                app.tails[&id].matches.is_empty(),
                "no scan during the burst"
            );
        }
        let armed = app.tails[&id]
            .search_rescan_due
            .expect("a deadline is armed after the burst");
        assert_eq!(armed, app.tick_count + search::DEBOUNCE_TICKS);

        for _ in 0..search::DEBOUNCE_TICKS {
            let _ = update(&mut app, Message::Tick);
        }
        let tw = &app.tails[&id];
        assert_eq!(tw.search_query, "alpha");
        assert_eq!(tw.matches.len(), 1, "one scan, for the FINAL query only");
        assert_eq!(tw.search_rescan_due, None);
    }

    #[test]
    fn clearing_a_pending_debounce_cancels_the_rescan() {
        // If the user clears the box while a large-buffer rescan is pending, the deferred scan must
        // NOT fire afterwards. (DECISIONS R162.)
        let mut big = String::with_capacity(search::DEBOUNCE_THRESHOLD_BYTES + 4096);
        big.push_str("zed\n");
        while big.len() < search::DEBOUNCE_THRESHOLD_BYTES + 2048 {
            big.push_str("a filler row with no match content on it here at all right ok\n");
        }
        let (mut app, id) = app_with_tail(&big);
        let _ = update(&mut app, Message::SearchChanged(id, "zed".into()));
        assert!(app.tails[&id].search_rescan_due.is_some());
        let _ = update(&mut app, Message::SearchCleared(id));
        assert_eq!(
            app.tails[&id].search_rescan_due, None,
            "clear cancels the pending rescan"
        );
        for _ in 0..(search::DEBOUNCE_TICKS + 1) {
            let _ = update(&mut app, Message::Tick);
        }
        let tw = &app.tails[&id];
        assert!(tw.search_query.is_empty());
        assert!(
            tw.matches.is_empty(),
            "no deferred scan fired after the clear"
        );
    }

    #[test]
    fn refresh_matches_recomputes_and_clamps_current() {
        // A query over the buffer finds matches; refresh keeps current_match valid as text grows.
        let mut tw = fixture();
        append_text(&mut tw, "alpha beta alpha\n");
        tw.search_query = "alpha".to_string();
        tw.current_match = Some(1); // second match selected
        tw.refresh_matches();
        assert_eq!(tw.matches.len(), 2);
        assert_eq!(tw.current_match, Some(1)); // still valid
                                               // The line_of the second match is line 0 (single line).
        let m1 = tw.matches[1];
        assert_eq!(tw.line_of(m1.start), 0);
    }

    // --- step 4b: auto-open decision (apply_event / activity) ---
    // apply_event returns Some(path) when the file WANTS to auto-open, driving the Tick's auto_open.
    // These exercise the model-facing decision off-hardware; the window spawning itself is xvfb.
    const T0: i64 = 1_752_496_800;
    #[test]
    fn activity_on_a_new_file_wants_to_auto_open() {
        let mut m = SessionModel::new();
        let ev = WatchEvent::Activity(PathBuf::from("/w/a.log"));
        assert_eq!(apply_event(&mut m, ev), Some("/w/a.log".to_string()));
    }
    #[test]
    fn discovered_never_auto_opens() {
        // Initial discovery of an existing (dormant) file must NOT open a window.
        let mut m = SessionModel::new();
        let ev = WatchEvent::Discovered(PathBuf::from("/w/a.log"));
        assert_eq!(apply_event(&mut m, ev), None);
    }
    #[test]
    fn activity_on_an_open_file_does_not_reopen() {
        let mut m = SessionModel::new();
        assert_eq!(m.try_open("/w/a.log"), OpenResult::Opened);
        let ev = WatchEvent::Activity(PathBuf::from("/w/a.log"));
        assert_eq!(apply_event(&mut m, ev), None); // already open -> no auto-open
    }
    #[test]
    fn activity_on_a_user_closed_file_does_not_reopen_but_marks_unread() {
        // Closed-stays-closed: a write to a user-closed file must NOT auto-reopen it, and it becomes
        // unread (renders Unread after any active window decays). `apply_event` stamps the activity
        // with the REAL clock (now_secs()), so query the vis well AFTER that to get past the active
        // window - use now_secs() + a large offset, not the fixed T0 baseline.
        let mut m = SessionModel::new();
        m.try_open("/w/a.log");
        m.mark_closed("/w/a.log");
        let ev = WatchEvent::Activity(PathBuf::from("/w/a.log"));
        assert_eq!(apply_event(&mut m, ev), None);
        let later = now_secs() + 10_000;
        // After the active window decays, it's Unread (not Idle) - the unread flag was set.
        assert_eq!(m.vis("/w/a.log", later), ButtonVis::Unread);
    }
    #[test]
    fn no_open_mode_suppresses_auto_open() {
        let mut m = SessionModel::new();
        m.no_open = true;
        let ev = WatchEvent::Activity(PathBuf::from("/w/a.log"));
        assert_eq!(apply_event(&mut m, ev), None);
    }
    #[test]
    fn reappeared_file_clears_missing_but_is_not_itself_activity() {
        // .i32 (review #2 finding 2, DECISIONS R111): Reappeared used to be treated as a write,
        // so after a root blip every file auto-opened. Now it only clears Missing; the sweep sends
        // a separate Activity when the stamp really changed, and THAT still auto-opens.
        let mut m = SessionModel::new();
        m.get_or_add("/w/a.log");
        m.mark_missing("/w/a.log", 0);
        let ev = WatchEvent::Reappeared(PathBuf::from("/w/a.log"));
        assert_eq!(
            apply_event(&mut m, ev),
            None,
            "reappearance alone must not auto-open"
        );
        // missing cleared: not rendered Missing anymore.
        assert_ne!(m.vis("/w/a.log", T0), ButtonVis::Missing);
        // A real write after the return still wants to open.
        let ev = WatchEvent::Activity(PathBuf::from("/w/a.log"));
        assert_eq!(apply_event(&mut m, ev), Some("/w/a.log".to_string()));
    }

    // --- .i14: auto-open FLASH decision (DECISIONS R75) ---
    // The taskbar flash itself is hardware-only (no foreground-lock/taskbar under xvfb), but the
    // DECISION - a fresh auto-open requests attention (flash) rather than raising focus, an over-cap
    // one blinks the cap, an already-open one does nothing - is pure and pinned here. This is the
    // permanent regression test for the .i13 raise-to-front FAIL (R73): if a future edit reverted
    // FlashAttention back to a focus-raise, this test fails.
    #[test]
    fn fresh_auto_open_flashes_attention_not_focus() {
        assert_eq!(
            auto_open_action(OpenResult::Opened),
            AutoOpenAction::FlashAttention
        );
    }
    #[test]
    fn over_cap_auto_open_blinks_the_cap() {
        assert_eq!(
            auto_open_action(OpenResult::BlockedByCap),
            AutoOpenAction::BlinkCap
        );
    }
    #[test]
    fn already_open_auto_open_does_nothing() {
        assert_eq!(
            auto_open_action(OpenResult::AlreadyOpen),
            AutoOpenAction::Nothing
        );
    }

    // --- step 4b: blink colors ---
    #[test]
    fn only_active_closed_blinks_others_are_steady() {
        // Every non-active-closed state renders the same color on both phases; active-closed differs.
        for v in [
            ButtonVis::Idle,
            ButtonVis::Open,
            ButtonVis::ActiveOpen,
            ButtonVis::Unread,
            ButtonVis::Missing,
        ] {
            assert_eq!(
                tile_fill(v, true),
                tile_fill(v, false),
                "{v:?} must not change with blink phase"
            );
        }
        assert_ne!(
            tile_fill(ButtonVis::ActiveClosed, true),
            tile_fill(ButtonVis::ActiveClosed, false),
            "active-closed must differ between bright and dim"
        );
        // Unread and active-closed share the BRIGHT amber (blink is the only difference, choice 4A).
        assert_eq!(
            tile_fill(ButtonVis::Unread, true),
            tile_fill(ButtonVis::ActiveClosed, true)
        );
    }

    // --- .i13: directory box ordering (root first, then hierarchical) ---
    /// A live `Settings` baseline for the commit tests.
    fn live() -> Settings {
        Settings {
            max_windows: 10,
            active_seconds: 5,
            no_open: false,
            poll_ms: 250,
            tiles_per_box: 4,
        }
    }
    /// A draft whose string fields mirror a `Settings`, so a test can tweak one field.
    fn draft_of(s: &Settings) -> SettingsDraft {
        SettingsDraft::from_live(window::Id::unique(), s)
    }
    #[test]
    fn commit_passes_valid_values_through_unchanged() {
        let prev = live();
        let committed = commit_settings(&draft_of(&prev), &prev);
        assert_eq!(committed, prev);
    }
    #[test]
    fn commit_clamps_max_windows_to_1_50() {
        let prev = live();
        let mut d = draft_of(&prev);
        d.max_windows = "9999".to_string();
        assert_eq!(
            commit_settings(&d, &prev).max_windows,
            dirwatch_core::session::MAX_WINDOWS_CEILING
        );
        d.max_windows = "0".to_string();
        assert_eq!(commit_settings(&d, &prev).max_windows, 1);
    }
    #[test]
    fn commit_floors_active_secs_at_1() {
        let prev = live();
        let mut d = draft_of(&prev);
        d.active_seconds = "0".to_string();
        assert_eq!(commit_settings(&d, &prev).active_seconds, 1);
    }
    #[test]
    fn commit_floors_poll_ms_at_100() {
        let prev = live();
        let mut d = draft_of(&prev);
        d.poll_ms = "50".to_string();
        assert_eq!(commit_settings(&d, &prev).poll_ms, POLL_MS_FLOOR);
        // A value at/above the floor is kept verbatim.
        d.poll_ms = "500".to_string();
        assert_eq!(commit_settings(&d, &prev).poll_ms, 500);
    }
    #[test]
    fn commit_ceilings_poll_ms_and_active_secs_instead_of_wrapping() {
        // review #2 finding 8: `4294967396` (2^32 + 100) used to commit as 100 ms via `as u32`.
        let prev = live();
        let mut d = draft_of(&prev);
        d.poll_ms = "4294967396".to_string();
        assert_eq!(commit_settings(&d, &prev).poll_ms, POLL_MS_CEILING);
        d.poll_ms = "10000000000".to_string();
        assert_eq!(commit_settings(&d, &prev).poll_ms, POLL_MS_CEILING);
        let mut d = draft_of(&prev);
        d.active_seconds = "999999999".to_string();
        assert_eq!(
            commit_settings(&d, &prev).active_seconds,
            ACTIVE_SECS_CEILING
        );
    }
    #[test]
    fn commit_clamps_tiles_per_box_to_2_10() {
        let prev = live();
        let mut d = draft_of(&prev);
        d.tiles_per_box = "1".to_string();
        assert_eq!(commit_settings(&d, &prev).tiles_per_box, TILES_PER_BOX_MIN);
        d.tiles_per_box = "99".to_string();
        assert_eq!(commit_settings(&d, &prev).tiles_per_box, TILES_PER_BOX_MAX);
    }
    #[test]
    fn commit_unparseable_field_keeps_previous_value() {
        // An empty or non-numeric field must NOT snap a good value to a default - it keeps `prev`.
        let prev = live();
        let mut d = draft_of(&prev);
        d.max_windows = "".to_string(); // user cleared the box mid-edit
        d.poll_ms = "".to_string();
        let committed = commit_settings(&d, &prev);
        assert_eq!(committed.max_windows, prev.max_windows);
        assert_eq!(committed.poll_ms, prev.poll_ms);
    }
    #[test]
    fn commit_carries_the_auto_open_toggle() {
        let prev = live(); // no_open = false (auto-open enabled)
        let mut d = draft_of(&prev);
        d.no_open = true; // user unchecked auto-open
        assert!(commit_settings(&d, &prev).no_open);
    }
    #[test]
    fn apply_settings_to_model_sets_the_three_model_knobs_live() {
        // OK applies max-windows / active-timeout / auto-open to the running model with no re-scan.
        let mut m = SessionModel::new();
        let s = Settings {
            max_windows: 7,
            active_seconds: 12,
            no_open: true,
            poll_ms: 300,
            tiles_per_box: 6,
        };
        apply_settings_to_model(&mut m, &s);
        assert_eq!(m.max_windows, 7);
        assert_eq!(m.active_seconds, 12);
        assert!(m.no_open);
    }
    #[test]
    fn from_boot_seeds_live_settings_from_cfg_and_opts() {
        // Opening Settings the first time must reflect what's actually running: the boot cfg's poll
        // and the CLI opts' knobs, with the same clamps.
        let cfg = WatchConfig {
            directory: PathBuf::from("/w"),
            patterns: vec!["*.log".into()],
            poll_interval_ms: 250,
            depth: 1,
        };
        let opts = ModelOpts {
            no_open: true,
            active_seconds: Some(9),
            max_windows: Some(3),
        };
        let s = Settings::from_boot(&cfg, &opts);
        assert_eq!(s.max_windows, 3);
        assert_eq!(s.active_seconds, 9);
        assert!(s.no_open);
        assert_eq!(s.poll_ms, 250);
        assert_eq!(s.tiles_per_box, TILES_PER_BOX_DEFAULT);
    }
    #[test]
    fn from_boot_floors_a_below_floor_poll() {
        // A boot poll below the floor (shouldn't happen via main.rs, but be defensive) is floored.
        let cfg = WatchConfig {
            directory: PathBuf::from("/w"),
            patterns: vec![],
            poll_interval_ms: 10,
            depth: 0,
        };
        let s = Settings::from_boot(&cfg, &ModelOpts::default());
        assert_eq!(s.poll_ms, POLL_MS_FLOOR);
    }
    #[test]
    fn digits_filter_strips_non_ascii_digits() {
        assert_eq!(digits("12a3"), "123");
        assert_eq!(digits("  4 5 "), "45");
        assert_eq!(digits("abc"), "");
    }

    // --- keyboard routing (Ctrl/Cmd W + F) — DECISIONS R88 -------------------------------------
    // Guards the pure `key_to_message` decision: both Ctrl (Windows/Linux) and Cmd/logo (macOS)
    // must trigger close/focus; a bare letter must not; Enter/Esc must still route after the refactor.
    fn ch(s: &str) -> keyboard::Key {
        keyboard::Key::Character(s.into())
    }
    #[test]
    fn ctrl_w_and_cmd_w_both_close_the_focused_tail() {
        let id = window::Id::unique();
        // Ctrl-W (Windows/Linux) and Cmd-W (macOS) BOTH map to CloseTail(id).
        for m in [keyboard::Modifiers::CTRL, keyboard::Modifiers::LOGO] {
            match key_to_message(&ch("w"), m, id) {
                Some(Message::CloseTail(got)) => {
                    assert_eq!(got, id, "CloseTail carries the focused id")
                }
                other => panic!("expected CloseTail for modifiers {m:?}, got {other:?}"),
            }
        }
        // Uppercase (Shift held) still matches — the guard is case-insensitive.
        assert!(matches!(
            key_to_message(&ch("W"), keyboard::Modifiers::LOGO, id),
            Some(Message::CloseTail(_))
        ));
    }
    #[test]
    fn ctrl_f_and_cmd_f_both_focus_search() {
        let id = window::Id::unique();
        for m in [keyboard::Modifiers::CTRL, keyboard::Modifiers::LOGO] {
            match key_to_message(&ch("f"), m, id) {
                Some(Message::FocusSearch(got)) => {
                    assert_eq!(got, id, "FocusSearch carries the focused id")
                }
                other => panic!("expected FocusSearch for modifiers {m:?}, got {other:?}"),
            }
        }
    }
    #[test]
    fn bare_w_or_f_without_a_modifier_does_nothing() {
        let id = window::Id::unique();
        // Typing 'w' or 'f' into a search box (no Ctrl/Cmd) must NOT close/focus.
        assert!(key_to_message(&ch("w"), keyboard::Modifiers::empty(), id).is_none());
        assert!(key_to_message(&ch("f"), keyboard::Modifiers::empty(), id).is_none());
        // Shift alone is not a command modifier either.
        assert!(key_to_message(&ch("w"), keyboard::Modifiers::SHIFT, id).is_none());
    }
    #[test]
    fn enter_and_esc_still_route_after_the_refactor() {
        use keyboard::key::Named;
        let id = window::Id::unique();
        assert!(matches!(
            key_to_message(
                &keyboard::Key::Named(Named::Enter),
                keyboard::Modifiers::empty(),
                id
            ),
            Some(Message::EnterPressed(_))
        ));
        assert!(matches!(
            key_to_message(
                &keyboard::Key::Named(Named::Escape),
                keyboard::Modifiers::empty(),
                id
            ),
            Some(Message::EscPressed(_))
        ));
    }

    // --- .i29 (review A3 / A5 / B5 / B7, DECISIONS R97-R99) ---
    /// A booted app state with one registered tail window (no OS window — `window::open` only
    /// mints an id and a Task outside a running daemon), for driving `update` in-process.
    fn app_with_tail(text: &str) -> (DirWatch, window::Id) {
        let dir = std::env::temp_dir().join(format!("dwgui_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Keep the test suite's ERR/DIAG lines out of the developer's real per-user log (the first
        // `init` in the process wins; every logging test goes through here or `fixture`).
        let _ = applog::init(Some(&dir.join("logs")));
        let (mut app, _task) = boot(
            WatchConfig {
                directory: dir,
                patterns: vec![],
                poll_interval_ms: 1000,
                depth: 0,
            },
            ModelOpts::default(),
        );
        let id = window::Id::unique();
        let mut tw = fixture();
        tw.path = "/w/t.log".into();
        append_text(&mut tw, text);
        app.tails.insert(id, tw);
        (app, id)
    }
    #[test]
    fn recovered_event_clears_the_error_note_and_reappeared_does_not_open_windows() {
        // review #2 finding 2, driven through `update(Tick)` with an injected event stream: an
        // Error sets the note, Recovered clears it, and a Reappeared file (no Activity) opens NO
        // window — on .i31 it auto-opened.
        let (mut app, _id) = app_with_tail("x\n");
        let (tx, rx) = std::sync::mpsc::channel();
        app.runtime = Some(WatchRuntime::from_receiver(rx));
        app.model.get_or_add("/w/other.log");
        app.model.mark_missing("/w/other.log", 0);
        let tails_before = app.tails.len();

        tx.send(WatchEvent::Error("cannot read /w: gone".to_string()))
            .unwrap();
        let _ = update(&mut app, Message::Tick);
        assert_eq!(
            app.note,
            Some((
                NoteKind::WatchError,
                "ERROR: cannot read /w: gone".to_string()
            ))
        );

        tx.send(WatchEvent::Recovered).unwrap();
        tx.send(WatchEvent::Reappeared(PathBuf::from("/w/other.log")))
            .unwrap();
        let _ = update(&mut app, Message::Tick);
        assert_eq!(app.note, None, "Recovered clears the ERROR note");
        assert_eq!(
            app.tails.len(),
            tails_before,
            "Reappeared alone opens nothing"
        );
        assert_ne!(app.model.vis("/w/other.log", T0), ButtonVis::Missing);

        // A non-error note (Plain) is left alone by a recovery.
        app.note = Some((NoteKind::Plain, "stopped".to_string()));
        tx.send(WatchEvent::Recovered).unwrap();
        let _ = update(&mut app, Message::Tick);
        assert_eq!(
            app.note,
            Some((NoteKind::Plain, "stopped".to_string())),
            "Recovered leaves a non-error note alone"
        );
    }
    #[test]
    fn cap_note_survives_a_real_root_outage_without_churn() {
        // i38 fix A (R125): with the dir-cap latched, a REAL root outage (Error) that then recovers
        // must NOT re-log or replace the cap note each cycle. Before i38 each blip logged
        // watch-error + watch-recovered + dir-cap; the dir-cap re-log is the bug. After i38 the cap
        // note is restored SILENTLY on recovery, and stays DirCap throughout.
        let (mut app, _id) = app_with_tail("x\n");
        let (tx, rx) = std::sync::mpsc::channel();
        app.runtime = Some(WatchRuntime::from_receiver(rx));
        // Force the model over the directory-box cap so dir_overflow() latches true.
        for i in 0..(dirwatch_core::session::MAX_DIR_BOXES + 1) {
            app.model.get_or_add(&format!("/w/d{i}/a.log"));
        }
        assert!(app.model.dir_overflow(), "precondition: cap latched");

        // First tick with no events surfaces the cap note (a genuine transition -> DirCap).
        let _ = update(&mut app, Message::Tick);
        assert!(
            matches!(app.note, Some((NoteKind::DirCap, _))),
            "cap note shown"
        );

        // Now three outage cycles: root gone, then back. The note must end as DirCap each time, and
        // never become a stale None or flip kinds spuriously.
        for _ in 0..3 {
            tx.send(WatchEvent::Error("cannot read /w: gone".to_string()))
                .unwrap();
            let _ = update(&mut app, Message::Tick);
            assert!(
                matches!(app.note, Some((NoteKind::WatchError, _))),
                "a real error outranks the cap note while the root is unreadable"
            );
            tx.send(WatchEvent::Recovered).unwrap();
            let _ = update(&mut app, Message::Tick);
            assert!(
                matches!(app.note, Some((NoteKind::DirCap, _))),
                "on recovery the still-latched cap note is restored (silently)"
            );
        }
    }
    #[test]
    fn file_cap_note_shows_only_when_a_file_is_refused_all_windows_open() {
        // finding 1: the file-cap note appears only on a REFUSAL (every tile an open window), not on a
        // normal eviction. Drive file_overflow() true directly (filling 1000 open windows in a unit
        // test is wasteful) and confirm the Tick surfaces a FileCap note while a watch runs.
        let (mut app, _id) = app_with_tail("x\n");
        let (_tx, rx) = std::sync::mpsc::channel();
        app.runtime = Some(WatchRuntime::from_receiver(rx));
        app.model.max_windows = dirwatch_core::session::MAX_FILES as i32 + 5;
        for i in 0..dirwatch_core::session::MAX_FILES {
            let p = format!("/w/f{i:05}.log");
            assert_eq!(app.model.try_open(&p), OpenResult::Opened);
        }
        // The refusal latches file_overflow.
        assert!(app.model.get_or_add("/w/new.log").is_none());
        assert!(app.model.file_overflow());
        let _ = update(&mut app, Message::Tick);
        assert!(
            matches!(app.note, Some((NoteKind::FileCap, _))),
            "file-cap refusal surfaces a FileCap note"
        );
    }
    #[test]
    fn a_file_missing_past_the_grace_has_its_tile_pruned_on_tick() {
        // finding 1 growth path: a file gone for the whole MISSING_PRUNE_GRACE_SECS is dropped from
        // the model on a Tick, so a rotating directory does not grow without bound. Simulated by
        // stamping missing_since far enough in the past via mark_missing with an old `now`, then
        // ticking (the Tick prunes with the real wall clock, which is well past the injected stamp).
        let (mut app, _id) = app_with_tail("x\n");
        let (_tx, rx) = std::sync::mpsc::channel();
        app.runtime = Some(WatchRuntime::from_receiver(rx));
        app.model.get_or_add("/w/gone.log");
        // Missing since the epoch-ish past: now - missing_since >> one hour on any real clock.
        app.model.mark_missing("/w/gone.log", 0);
        assert_eq!(app.model.entry_count(), 1);
        let _ = update(&mut app, Message::Tick);
        assert_eq!(
            app.model.entry_count(),
            0,
            "a file Missing far past the grace is pruned on Tick"
        );
    }
    #[test]
    fn prune_is_skipped_while_the_root_is_unreadable() {
        // The prune must honour the same guard as the core sweep: while the root is unreadable, a
        // Missing tile is NOT pruned (a share blip is not "the file is gone").
        let (mut app, _id) = app_with_tail("x\n");
        let (tx, rx) = std::sync::mpsc::channel();
        app.runtime = Some(WatchRuntime::from_receiver(rx));
        app.model.get_or_add("/w/gone.log");
        app.model.mark_missing("/w/gone.log", 0);
        // Raise a root error so the note is WatchError this tick.
        tx.send(WatchEvent::Error("cannot read /w: gone".to_string()))
            .unwrap();
        let _ = update(&mut app, Message::Tick);
        assert!(matches!(app.note, Some((NoteKind::WatchError, _))));
        assert_eq!(
            app.model.entry_count(),
            1,
            "nothing pruned while the root is unreadable"
        );
    }
    #[test]
    fn restart_marks_tails_closing_and_view_renders_blank_for_them() {
        // i38 fix B (R125): Restart asks the OS to close the open tails; until WindowClosed arrives
        // those ids are `closing`, and view() must render a blank placeholder for them (not
        // main_view, which would flash the main UI into the closing window).
        let (mut app, id) = app_with_tail("x\n");
        assert!(app.tails.contains_key(&id));
        let _ = start_watch(&mut app);
        assert!(
            app.closing.contains(&id),
            "the closed tail id is marked closing until WindowClosed"
        );
        assert!(!app.tails.contains_key(&id), "tails cleared on Restart");
        // ...and the VIEW for that id is the blank placeholder, not the main UI (review #4
        // test-quality note: the "renders blank" half used to be untested).
        assert_eq!(view_kind(&app, id), ViewKind::Closing);

        // WindowClosed removes it from the closing set.
        let _ = update(&mut app, Message::WindowClosed(id));
        assert!(!app.closing.contains(&id), "WindowClosed clears closing");
        assert_eq!(
            view_kind(&app, id),
            ViewKind::Main,
            "an unknown id falls through to main"
        );
    }
    /// A booted app with one tail whose stream is a channel the test owns, so `update(Tick)` can be
    /// driven with an exact `TailChunk` sequence (review #4 test-quality note: the old test
    /// re-implemented the Tick's per-chunk logic instead of calling it).
    fn app_with_injected_tail() -> (
        DirWatch,
        window::Id,
        std::sync::mpsc::Sender<crate::runtime::TailChunk>,
    ) {
        let (mut app, id) = app_with_tail("");
        let (tx, rx) = std::sync::mpsc::channel();
        let tw = app.tails.get_mut(&id).unwrap();
        tw.stream = TailStream::from_receiver(rx);
        tw.loading = true;
        (app, id, tx)
    }
    #[test]
    fn empty_ready_chunk_ends_loading_and_a_skipped_marker_is_rendered() {
        // review #2 findings 4 and 5, GUI side, driven THROUGH `update(Tick)`: a `ready` chunk with
        // no text drops the "loading..." placeholder (an empty file renders blank); a
        // `skipped_bytes` chunk adds the R104-style marker AFTER any rotated/reappeared marker
        // (review #4 finding 19) and before the text that follows.
        let (mut app, id, tx) = app_with_injected_tail();
        assert!(app.tails[&id].loading);
        tx.send(crate::runtime::TailChunk {
            ready: true,
            encoding_label: "UTF-8/ASCII".to_string(),
            ..Default::default()
        })
        .unwrap();
        let _ = update(&mut app, Message::Tick);
        assert!(
            !app.tails[&id].loading,
            "ready ends loading even with no text"
        );
        assert_eq!(app.tails[&id].encoding, "UTF-8/ASCII");
        assert_eq!(app.tails[&id].text, "");

        tx.send(crate::runtime::TailChunk {
            rotated: true,
            skipped_bytes: Some(12345),
            new_text: Some("tail-of-file\n".to_string()),
            ..Default::default()
        })
        .unwrap();
        let _ = update(&mut app, Message::Tick);
        assert_eq!(
            app.tails[&id].text,
            "\n--- file rotated/truncated ---\n--- earlier 12345 bytes skipped ---\ntail-of-file\n",
            "rotated marker first, then the skip that belongs to the NEW file, then its text"
        );
    }
    #[test]
    fn one_enter_in_a_tail_window_steps_exactly_one_match() {
        // review A3: the message stream for ONE Enter is now exactly [EnterPressed] (the search
        // box's `on_submit` is gone), and EnterPressed steps once: None -> 1 of N, then 2 of N.
        let (mut app, id) = app_with_tail("error a\nerror b\nerror c\n");
        let _ = update(&mut app, Message::SearchChanged(id, "error".into()));
        assert_eq!(app.tails[&id].matches.len(), 3);
        // B7: typing already selected the FIRST match.
        assert_eq!(app.tails[&id].current_match, Some(0));
        let enter = key_to_message(
            &keyboard::Key::Named(keyboard::key::Named::Enter),
            keyboard::Modifiers::empty(),
            id,
        );
        assert!(matches!(enter, Some(Message::EnterPressed(_))));
        let _ = update(&mut app, enter.unwrap());
        assert_eq!(
            app.tails[&id].current_match,
            Some(1),
            "one Enter == one step"
        );
        let _ = update(&mut app, Message::EnterPressed(id));
        assert_eq!(app.tails[&id].current_match, Some(2));
        let _ = update(&mut app, Message::EnterPressed(id));
        assert_eq!(app.tails[&id].current_match, Some(0), "wraps");
    }
    #[test]
    fn typing_a_query_selects_and_pauses_on_the_first_match() {
        // review B7: SearchChanged used to leave current_match = None (so nothing scrolled).
        let (mut app, id) = app_with_tail("a\nb\nerror here\n");
        let _ = update(&mut app, Message::SearchChanged(id, "error".into()));
        let tw = &app.tails[&id];
        assert_eq!(tw.current_match, Some(0));
        assert!(!tw.following, "jumping to a match pauses follow");
        assert_eq!(
            tw.scroll_y, 0.0,
            "line 2 - 3 context lines clamps to the top"
        );
        // Empty query: no selection, no matches.
        let _ = update(&mut app, Message::SearchChanged(id, String::new()));
        assert_eq!(app.tails[&id].current_match, None);
        assert!(app.tails[&id].matches.is_empty());
    }
    #[test]
    fn resume_follow_reengages_following_from_a_paused_tail() {
        // R173: the resume chip. A search-jump pauses follow (verified by the test above); clicking
        // the chip (Message::ResumeFollow) must set following back to true so the header LED flips
        // this frame and the chip hides. (The snap-to-bottom task it also emits moves the view; the
        // scroll position itself is exercised by the headless render, not this pure-state test.)
        let (mut app, id) = app_with_tail("a\nb\nerror here\n");
        let _ = update(&mut app, Message::SearchChanged(id, "error".into()));
        assert!(
            !app.tails[&id].following,
            "precondition: search-jump paused follow"
        );
        let _ = update(&mut app, Message::ResumeFollow(id));
        assert!(
            app.tails[&id].following,
            "ResumeFollow must re-engage following"
        );
    }
    #[test]
    fn follow_uses_an_absolute_pixel_gap_not_a_fraction() {
        // review A5 / DECISIONS R99: 1,000 lines up on a 250k-line file is NOT following, even
        // though it is 99.6% of the way down (which the old `>= 0.995` rule accepted).
        let content_h = 250_000.0 * TAIL_LINE_H;
        let viewport_h = 360.0;
        let at_bottom = content_h - viewport_h;
        assert!(is_following(follow_gap_px(
            at_bottom, viewport_h, content_h
        )));
        // One line up: still following (within the 2-line slack).
        assert!(is_following(follow_gap_px(
            at_bottom - TAIL_LINE_H,
            viewport_h,
            content_h
        )));
        // Three lines up: paused.
        assert!(!is_following(follow_gap_px(
            at_bottom - 3.0 * TAIL_LINE_H,
            viewport_h,
            content_h
        )));
        // 1,000 lines up (relative offset 0.996): paused.
        let up = at_bottom - 1000.0 * TAIL_LINE_H;
        assert!(
            up / at_bottom > 0.995,
            "sanity: the old rule would call this 'following'"
        );
        assert!(!is_following(follow_gap_px(up, viewport_h, content_h)));
        // Content shorter than the viewport: gap 0 => following.
        assert!(is_following(follow_gap_px(0.0, 360.0, 90.0)));
    }
    #[test]
    fn main_window_opened_event_sets_the_real_rect_and_a_screen_estimate() {
        // review B5: a Centered main window at (243, 84) sized 880x600 implies a 1366x768 screen.
        let (mut app, _id) = app_with_tail("x\n");
        let main = app.main_id.expect("boot records the main id synchronously");
        let _ = update(
            &mut app,
            Message::WindowEvent(
                main,
                window::Event::Opened {
                    position: Some(Point::new(243.0, 84.0)),
                    size: Size::new(880.0, 600.0),
                },
            ),
        );
        assert_eq!(app.screen_est, Some((1366.0, 768.0)));
        let r = app.main_rect.unwrap();
        assert_eq!((r.x, r.y, r.w, r.h), (243.0, 84.0, 880.0, 600.0));
        // A move updates the rect but NOT the frozen screen estimate.
        let _ = update(
            &mut app,
            Message::WindowEvent(main, window::Event::Moved(Point::new(10.0, 20.0))),
        );
        assert_eq!(app.main_rect.unwrap().x, 10.0);
        assert_eq!(app.screen_est, Some((1366.0, 768.0)));
        // Placement now stays ON that 1366-wide screen: the first tail can't sit past x = 846.
        let (px, _py) = next_position(PlaceInput {
            screen: Rect {
                x: 0.0,
                y: 0.0,
                w: 1366.0,
                h: 768.0,
            },
            win_w: TAIL_W,
            win_h: TAIL_H,
            main: Rect {
                x: 243.0,
                y: 84.0,
                w: 880.0,
                h: 600.0,
            },
            open_index: 0,
        });
        assert!(
            px + TAIL_W <= 1366.0,
            "tail at x={px} would be off a 1366-wide screen"
        );
    }
    #[test]
    fn events_for_other_windows_do_not_touch_the_main_rect() {
        let (mut app, tail_id) = app_with_tail("x\n");
        let _ = update(
            &mut app,
            Message::WindowEvent(tail_id, window::Event::Moved(Point::new(5.0, 5.0))),
        );
        assert!(app.main_rect.is_none());
    }
    #[test]
    fn path_to_win_uses_the_shared_fs_key_rule() {
        // review A1: the GUI's path->window map must fold case exactly like the core.
        let (mut app, _id) = app_with_tail("x\n");
        let wid = window::Id::unique();
        app.path_to_win.insert(fs_key_str("/w/Mixed.LOG"), wid);
        let hit = app.path_to_win.get(&fs_key_str("/w/mixed.log")).copied();
        if dirwatch_core::fskey::fs_is_case_insensitive() {
            assert_eq!(hit, Some(wid));
        } else {
            assert_eq!(hit, None);
        }
    }
    #[test]
    fn resolve_watch_dir_makes_relative_absolute_against_cwd() {
        let cwd = Path::new("/home/mike/logs");
        assert_eq!(
            resolve_watch_dir("sub", cwd),
            PathBuf::from("/home/mike/logs/sub")
        );
    }
    #[test]
    fn resolve_watch_dir_dot_and_empty_are_the_cwd() {
        let cwd = Path::new("/home/mike/logs");
        assert_eq!(
            resolve_watch_dir(".", cwd),
            PathBuf::from("/home/mike/logs")
        );
        assert_eq!(
            resolve_watch_dir("   ", cwd),
            PathBuf::from("/home/mike/logs")
        );
    }
    #[test]
    fn resolve_watch_dir_strips_trailing_separator_and_dot_segments() {
        let cwd = Path::new("/home/mike");
        assert_eq!(
            resolve_watch_dir("logs/", cwd),
            PathBuf::from("/home/mike/logs")
        );
        assert_eq!(
            resolve_watch_dir("./logs/./a/..", cwd),
            PathBuf::from("/home/mike/logs")
        );
    }
    #[test]
    fn resolve_watch_dir_leaves_absolute_unchanged_but_normalised() {
        let cwd = Path::new("/home/mike");
        assert_eq!(
            resolve_watch_dir("/var/log", cwd),
            PathBuf::from("/var/log")
        );
        assert_eq!(
            resolve_watch_dir("/var/log/../log/app", cwd),
            PathBuf::from("/var/log/app")
        );
    }
    #[test]
    fn group_tile_boxes_matches_the_pre_cache_grouping_and_order() {
        // finding 1: the cached layout must group + order tiles exactly as the old per-frame
        // `file_area` did. Files sort by lowercased path; boxes group by parent and order root-first
        // then hierarchical; the first-seen casing is the box display string.
        let root = "/w";
        let paths = vec![
            "/w/b.log".to_string(),
            "/w/sub/z.log".to_string(),
            "/w/a.log".to_string(),
            "/w/sub/a.log".to_string(),
        ];
        let boxes = group_tile_boxes(&paths, root);
        // Two boxes: the root, then the subdir.
        assert_eq!(boxes.len(), 2);
        assert_eq!(boxes[0].dir, "/w"); // root box first
        assert_eq!(boxes[1].dir, "/w/sub"); // then the subdir
                                            // Files within each box are in lowercased-path order.
        let root_names: Vec<&str> = boxes[0].files.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(root_names, vec!["a.log", "b.log"]);
        let sub_names: Vec<&str> = boxes[1].files.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(sub_names, vec!["a.log", "z.log"]);
        // Empty input yields no boxes (the "no matching files yet" path).
        assert!(group_tile_boxes(&[], root).is_empty());
    }

    #[test]
    fn resolve_watch_dir_strips_surrounding_quotes_from_copy_as_path() {
        // finding 8: Windows Explorer "Copy as path" yields a double-quoted path; pasted in, the
        // quotes used to become literal path characters and the directory resolved to a bogus
        // `<cwd>\"…"`. A matching surrounding pair is now stripped, so a pasted absolute path stays
        // absolute (and normalised) instead of being joined onto the cwd.
        let cwd = Path::new("/home/mike");
        assert_eq!(
            resolve_watch_dir("\"/var/log\"", cwd),
            PathBuf::from("/var/log")
        );
        // A quoted RELATIVE path still resolves against the cwd, minus the quotes.
        assert_eq!(
            resolve_watch_dir("\"logs\"", cwd),
            PathBuf::from("/home/mike/logs")
        );
        // An unbalanced quote is NOT stripped (it may be a genuine, if odd, name character).
        assert_eq!(
            resolve_watch_dir("\"logs", cwd),
            PathBuf::from("/home/mike/\"logs")
        );
    }
    #[test]
    fn split_patterns_strips_surrounding_quotes_per_pattern() {
        // finding 8: a pasted quoted pattern used to match nothing silently.
        assert_eq!(split_patterns("\"*.log\""), vec!["*.log".to_string()]);
        // Quotes stripped per element in a list; unquoted elements untouched.
        assert_eq!(
            split_patterns("\"*.log\"; *.txt"),
            vec!["*.log".to_string(), "*.txt".to_string()]
        );
        // An unbalanced quote is left as-is.
        assert_eq!(split_patterns("\"*.log"), vec!["\"*.log".to_string()]);
    }
    #[cfg(windows)]
    #[test]
    fn resolve_watch_dir_windows_relative_joins_cwd() {
        let cwd = Path::new(r"C:\Users\mike");
        assert_eq!(
            resolve_watch_dir(r"logs\app", cwd),
            PathBuf::from(r"C:\Users\mike\logs\app")
        );
    }
    #[cfg(windows)]
    #[test]
    fn resolve_watch_dir_preserves_windows_drive_relative() {
        // "C:foo" carries a Prefix but is not absolute; we anchor it as typed (never join a foreign
        // cwd onto it) and let Windows resolve it against that drive's own current directory.
        let cwd = Path::new(r"D:\work");
        let got = resolve_watch_dir("C:foo", cwd);
        assert!(got.to_string_lossy().starts_with("C:"));
        assert!(!got.to_string_lossy().to_lowercase().contains("work"));
    }

    // ---- finding 10: tail_position (DECISIONS R118) ------------------------------------------
    #[test]
    fn tail_position_uses_a_usable_screen_estimate() {
        let main = Rect {
            x: 40.0,
            y: 40.0,
            w: MAIN_W,
            h: MAIN_H,
        };
        let (x, y) = tail_position(Some((1920.0, 1040.0)), Some(main), 0);
        // First tail opens just right of the main window, at its top.
        assert_eq!((x, y), (40.0 + MAIN_W + 8.0, 40.0));
    }
    #[test]
    fn tail_position_rejects_bogus_estimate_and_places_beside_main() {
        // Main on a left-hand monitor at negative x: 2*x + w = -2320 (<= MAIN_W), an unusable
        // estimate. The tail must open BESIDE the real main window.
        let main = Rect {
            x: -1600.0,
            y: 100.0,
            w: MAIN_W,
            h: MAIN_H,
        };
        let bogus = Some((2.0 * -1600.0 + MAIN_W, 1040.0));
        let (x, y) = tail_position(bogus, Some(main), 0);
        assert_eq!((x, y), (-1600.0 + MAIN_W + 8.0, 100.0));
    }
    #[test]
    fn tail_position_negative_x_main_stays_on_its_monitor() {
        // Regression for finding 10: the pre-R118 code fed the bogus estimate to next_position as the
        // screen and clamped the tail to x >= 0 (the primary monitor). Now x stays negative — on the
        // same monitor as the main window.
        let main = Rect {
            x: -1600.0,
            y: 100.0,
            w: MAIN_W,
            h: MAIN_H,
        };
        let (x, _) = tail_position(Some((-2320.0, 1040.0)), Some(main), 0);
        assert!(
            x < 0.0,
            "tail should stay on the main window's monitor, got x={x}"
        );
    }
    #[test]
    fn tail_position_falls_back_when_nothing_known() {
        let (x, y) = tail_position(None, None, 0);
        assert!(x >= 0.0 && y >= 0.0);
    }

    // ---- finding 16: shared constants (DECISIONS R118) ---------------------------------------
    #[test]
    fn gui_and_blink_tick_periods_match() {
        // The blink phase math mirrors the GUI tick; pin them equal so a change to one can't silently
        // desync the cadence.
        assert_eq!(TICK_MS, blink::TICK_MS);
    }
    #[test]
    fn poll_floor_is_the_shared_core_constant() {
        assert_eq!(POLL_MS_FLOOR, dirwatch_core::POLL_INTERVAL_FLOOR_MS);
    }

    // --- .i40 (review #4 findings 5, 6, 7, 8, 9, 15, 16; DECISIONS R128) ---
    #[test]
    fn cap_blocked_auto_open_marks_the_file_unread() {
        // REGRESSION (review #4 finding 5): a write whose auto-open the WINDOW cap blocked was never
        // marked unread, so after `active_seconds` the tile went Idle although the file changed and
        // nobody saw it. On .i39: `tile after decay: Idle (expected Unread)`.
        let (mut app, _id) = app_with_tail("x\n");
        // The fixture tail is inserted directly; tell the model it holds a slot, then cap at 1.
        assert_eq!(app.model.try_open("/w/t.log"), OpenResult::Opened);
        app.model.max_windows = 1; // the one open tail fills the cap
        app.settings.max_windows = 1;
        app.model.active_seconds = 10;
        let (tx, rx) = std::sync::mpsc::channel();
        app.runtime = Some(WatchRuntime::from_receiver(rx));
        tx.send(WatchEvent::Discovered(PathBuf::from("/w/closed.log")))
            .unwrap();
        tx.send(WatchEvent::Activity(PathBuf::from("/w/closed.log")))
            .unwrap();
        let _ = update(&mut app, Message::Tick);
        assert_eq!(app.tails.len(), 1, "cap held: no second window");
        assert!(app.cap_blink_start_ms.is_some(), "the cap blinked");
        let now = now_secs();
        assert_eq!(
            app.model.vis_of("/w/closed.log", now),
            ButtonVis::ActiveClosed
        );
        assert_eq!(
            app.model.vis_of("/w/closed.log", now + 60),
            ButtonVis::Unread,
            "after the active window decays the tile reads Unread, not Idle"
        );
    }
    #[test]
    fn error_then_recovered_in_one_tick_leaves_no_stale_error_note() {
        // REGRESSION (review #4 finding 6): `[Error, Recovered]` drained in ONE Tick (a late tick —
        // the Browse dialog blocks update — or two sweeps in one tick) left a permanent ERROR note.
        let (mut app, _id) = app_with_tail("x\n");
        let (tx, rx) = std::sync::mpsc::channel();
        app.runtime = Some(WatchRuntime::from_receiver(rx));
        tx.send(WatchEvent::Error("cannot read /w: blip".into()))
            .unwrap();
        tx.send(WatchEvent::Recovered).unwrap();
        let _ = update(&mut app, Message::Tick);
        assert_eq!(app.note, None, "the batch's outcome is 'readable': no note");
        // The reverse order in one batch ends in an error note (the root IS unreadable now).
        tx.send(WatchEvent::Recovered).unwrap();
        tx.send(WatchEvent::Error("cannot read /w: gone".into()))
            .unwrap();
        let _ = update(&mut app, Message::Tick);
        assert!(matches!(app.note, Some((NoteKind::WatchError, _))));
        // And a later Recovered on its own clears it.
        tx.send(WatchEvent::Recovered).unwrap();
        let _ = update(&mut app, Message::Tick);
        assert_eq!(app.note, None);
    }
    #[test]
    fn tile_click_browse_and_settings_do_not_clear_a_live_watch_error_note() {
        // REGRESSION (review #4 finding 7): opening a tail (or Browse/Settings/an over-cap click)
        // blanked the note; the Error is edge-triggered so nothing restored it — the strip read
        // `watching …` for the rest of the outage. On .i39: `note after click + 2 ticks: None`.
        let mut app = app_with_tail("x\n").0;
        let (tx, rx) = std::sync::mpsc::channel();
        app.runtime = Some(WatchRuntime::from_receiver(rx));
        app.model.get_or_add("/w/a.log");
        tx.send(WatchEvent::Error("cannot read /w: gone".into()))
            .unwrap();
        let _ = update(&mut app, Message::Tick);
        let err = app.note.clone();
        assert!(matches!(err, Some((NoteKind::WatchError, _))));
        let _ = update(&mut app, Message::TileClicked("/w/a.log".into()));
        let _ = update(&mut app, Message::Tick);
        assert_eq!(app.note, err, "a tile click leaves the error note alone");
        // An over-cap click blinks the count and leaves the note alone too.
        app.model.max_windows = 1;
        let _ = update(&mut app, Message::TileClicked("/w/b.log".into()));
        assert!(app.cap_blink_start_ms.is_some());
        assert_eq!(
            app.note, err,
            "an over-cap click leaves the error note alone"
        );
        // Opening Settings leaves it alone (Browse is not testable headlessly: it opens an OS dialog;
        // its `note = None` was removed by the same change and it now touches no note at all).
        let _ = update(&mut app, Message::OpenSettings);
        assert_eq!(
            app.note, err,
            "opening Settings leaves the error note alone"
        );
    }
    #[test]
    fn tile_open_does_not_drop_or_relog_the_dir_cap_note() {
        // REGRESSION (review #4 finding 7): with the cap latched, opening a tail blanked the cap note
        // and the next tick restored it WITH a fresh `dir cap` log line — one re-log per tail open.
        let (mut app, _id) = app_with_tail("x\n");
        let (_tx, rx) = std::sync::mpsc::channel();
        app.runtime = Some(WatchRuntime::from_receiver(rx));
        for i in 0..(dirwatch_core::session::MAX_DIR_BOXES + 1) {
            app.model.get_or_add(&format!("/w/d{i}/a.log"));
        }
        let _ = update(&mut app, Message::Tick);
        let cap = app.note.clone();
        assert!(matches!(cap, Some((NoteKind::DirCap, _))));
        let _ = update(&mut app, Message::TileClicked("/w/d0/a.log".into()));
        assert_eq!(
            app.note, cap,
            "the cap note survives a tail open (no blank frame)"
        );
        let _ = update(&mut app, Message::Tick);
        assert_eq!(
            app.note, cap,
            "…and the next tick has nothing to restore (so nothing to re-log)"
        );
    }
    #[test]
    fn stop_keeps_its_stopped_note_while_the_cap_is_latched() {
        // REGRESSION (review #4 finding 7): block (c) ran with no runtime, overwriting "stopped" with
        // the cap note (and logging `dir cap` again) one tick after Stop.
        let (mut app, _id) = app_with_tail("x\n");
        for i in 0..(dirwatch_core::session::MAX_DIR_BOXES + 1) {
            app.model.get_or_add(&format!("/w/d{i}/a.log"));
        }
        let _ = update(&mut app, Message::Tick);
        assert!(matches!(app.note, Some((NoteKind::DirCap, _))));
        let _ = update(&mut app, Message::ToggleWatch); // Stop
        assert!(app.runtime.is_none());
        assert_eq!(app.note, Some((NoteKind::Plain, "stopped".to_string())));
        let _ = update(&mut app, Message::Tick);
        let _ = update(&mut app, Message::Tick);
        assert_eq!(
            app.note,
            Some((NoteKind::Plain, "stopped".to_string())),
            "a stopped watch keeps its note; the cap note waits for the next Start"
        );
    }
    #[test]
    fn settings_ok_and_cancel_render_blank_until_window_closed() {
        // REGRESSION (review #4 finding 8): OK/Cancel dropped the draft and asked the OS to close,
        // but did not mark the id `closing`, so `view()` fell through to `main_view` for the last
        // frame(s) — the fix-B flash, on the Settings window this time.
        for ok in [true, false] {
            let (mut app, _id) = app_with_tail("x\n");
            let _ = update(&mut app, Message::OpenSettings);
            let sid = app.settings_win.as_ref().unwrap().id;
            assert_eq!(view_kind(&app, sid), ViewKind::Settings);
            let _ = update(
                &mut app,
                if ok {
                    Message::SettingsOk
                } else {
                    Message::SettingsCancel
                },
            );
            assert!(app.settings_win.is_none());
            assert_eq!(
                view_kind(&app, sid),
                ViewKind::Closing,
                "{} must render blank, never main_view, until WindowClosed",
                if ok { "OK" } else { "Cancel" }
            );
            let _ = update(&mut app, Message::WindowClosed(sid));
            assert!(!app.closing.contains(&sid));
        }
    }
    #[test]
    fn a_late_settings_opened_for_a_dead_window_does_not_rebind_the_live_draft() {
        // REGRESSION (review #4 finding 15): Open -> Cancel -> Open, then the FIRST window's
        // `SettingsOpened` arrives late and used to overwrite the live draft's id with the dead
        // one — Enter/Esc/OK/Cancel on the live window did nothing for the rest of the session.
        let (mut app, _id) = app_with_tail("x\n");
        let _ = update(&mut app, Message::OpenSettings);
        let dead = app.settings_win.as_ref().unwrap().id;
        let _ = update(&mut app, Message::SettingsCancel);
        let _ = update(&mut app, Message::WindowClosed(dead));
        let _ = update(&mut app, Message::OpenSettings);
        let live = app.settings_win.as_ref().unwrap().id;
        assert_ne!(live, dead);
        let _ = update(&mut app, Message::SettingsOpened(dead)); // the stale confirmation
        assert_eq!(
            app.settings_win.as_ref().unwrap().id,
            live,
            "live draft keeps its id"
        );
        // Esc on the live window still cancels it.
        let _ = update(&mut app, Message::EscPressed(live));
        assert!(
            app.settings_win.is_none(),
            "Esc reached the live Settings window"
        );
    }
    #[test]
    fn scrollback_cap_bounds_a_single_over_cap_line_by_bytes() {
        // REVISED at .i48 (review #6 finding 6, DECISIONS R146; was review #4 finding 9's "the
        // newest line survives whatever its size"): a single terminated line over the cap is now
        // cut by BYTES from its front to the trim target — its NEWEST bytes survive, the buffer is
        // bounded, and the marker reports bytes. The line count stays 1 (+ the marker row).
        let mut tw = fixture();
        append_text(&mut tw, &format!("{}\n", "x".repeat(100)));
        assert!(
            cap_scrollback_to(&mut tw, 40, 20),
            "over the cap: byte-trimmed"
        );
        assert!(
            tw.text.starts_with("--- earlier 81 bytes dropped ---\n"),
            "{:?}",
            tw.text
        );
        assert!(tw.text.ends_with(&format!("{}\n", "x".repeat(19))));
        assert_eq!(tw.line_count(), 2, "marker row + the (cut) line");
        assert_eq!(tw.line_starts[0], 0);
        // Under the cap afterwards: NO trim on a small append (no per-append buffer copy).
        append_text(&mut tw, "yy\n");
        assert!(!cap_scrollback_to(&mut tw, 60, 20));
        // A UTF-8 char straddling the cut moves the cut forward to a boundary, never mid-char.
        let mut tw = fixture();
        append_text(&mut tw, &"é".repeat(50)); // 100 bytes, no terminator
        assert!(cap_scrollback_to(&mut tw, 40, 21));
        assert!(tw.text.ends_with(&"é".repeat(10)), "{:?}", tw.text);
        assert!(std::str::from_utf8(tw.text.as_bytes()).is_ok());
    }
    #[test]
    fn scrollback_cap_finishes_the_byte_bound_in_the_same_call_after_a_line_cut() {
        // Caught by the .i48 gate's zero-filled step: the reader's `--- earlier N bytes skipped ---`
        // line followed by a giant NUL line arrived in ONE tick; the line cut dropped the marker line,
        // returned, and the giant line stayed at 3x the cap until the next append. One call must
        // reach the bound.
        let mut tw = fixture();
        append_text(&mut tw, "--- earlier 12345 bytes skipped ---\n");
        append_text(&mut tw, &"g".repeat(200));
        assert!(cap_scrollback_to(&mut tw, 40, 20));
        assert!(
            tw.text.len() <= 40 + 48,
            "bounded in one call: {} bytes",
            tw.text.len()
        );
        assert!(
            tw.text
                .starts_with("--- earlier 1 lines and 180 bytes dropped ---\n"),
            "{:?}",
            &tw.text[..48]
        );
        assert!(tw.text.ends_with(&"g".repeat(20)));
    }
    #[test]
    fn scrollback_cap_does_not_rethrash_on_an_unterminated_giant_last_line() {
        // REGRESSION (review #4 finding 9) REVISED for R146: with a giant unterminated last line
        // over the cap the first trim drops the real lines; the giant line is then cut by bytes to
        // the trim target, the marker carries BOTH counts, and appends that stay under the cap do
        // NOT trim (a trim always removes at least cap - trim_to bytes, never a no-gain copy).
        let mut tw = fixture();
        for i in 0..3 {
            append_text(&mut tw, &format!("l{i}\n"));
        }
        append_text(&mut tw, &"g".repeat(60)); // giant, unterminated, over the cap on its own
        assert!(
            cap_scrollback_to(&mut tw, 40, 20),
            "first trim drops the three real lines AND cuts the giant line to the target"
        );
        assert!(
            tw.text
                .starts_with("--- earlier 3 lines and 40 bytes dropped ---\n"),
            "{:?}",
            tw.text
        );
        assert!(tw.text.ends_with(&"g".repeat(20)));
        // Under the cap now: a second check is a no-op.
        assert!(!cap_scrollback_to(&mut tw, 100, 20));
        // Now under the cap (marker + 20 bytes < 100): small appends do not trim.
        for _ in 0..5 {
            append_text(&mut tw, "gg");
            assert!(
                !cap_scrollback_to(&mut tw, 100, 20),
                "under the cap: no re-trim, no buffer copy"
            );
        }
        // Once real lines exist again the totals carry on.
        append_text(&mut tw, "\nm1\nm2\n");
        assert!(cap_scrollback_to(&mut tw, 40, 20));
        assert!(
            tw.text
                .starts_with("--- earlier 4 lines and 40 bytes dropped ---\n")
                || tw
                    .text
                    .starts_with("--- earlier 5 lines and 40 bytes dropped ---\n"),
            "running totals continue: {:?}",
            &tw.text[..48]
        );
        assert_eq!(tw.line_starts[0], 0);
    }

    // --- .i48 (review #6 findings 5, 6, 7; DECISIONS R143/R145/R146) ---
    #[test]
    fn normaliser_fast_and_slow_paths_agree() {
        // R145: `first_special` (the byte scan) must classify exactly what the char loop rewrites.
        // Every chunk is run BOTH ways: through `normalize_line_endings` (fast path when clean) and
        // through a forced slow path (a leading pending-CR makes it walk every char), compared
        // after the placeholder the pending CR adds.
        let cases = [
            "plain ascii\nlines\n",
            "café — smart ‘quotes’ … done\n",
            "a\r\nb\r\n",
            "lone\rcr\r",
            "nel\u{85}here\u{2028}and\u{2029}there\u{1c}\u{1d}\u{1e}\n",
            "\x00\x01\x07\x08\x0b\x0c\x1b[31mred\x1b[0m\x7f\n",
            "tab\tkept\n",
            "\u{FEFF}bom mid\n",
            "emoji 👨‍👩‍👧‍👦 ok\n",
            "\r",
            "x\u{2027}y\u{2030}z", // near-misses of the U+2028/2029 byte pattern
            "\u{E2}\u{80}",        // the bytes C3 A2, C2 80 — not the E2 80 lead sequence
        ];
        for c in cases {
            let mut fast = String::new();
            let pf = normalize_line_endings(c, false, &mut fast);
            let mut slow = String::new();
            let ps = normalize_line_endings(c, true, &mut slow); // forced slow path
            let slow = slow
                .strip_prefix(CR_PLACEHOLDER)
                .map(str::to_string)
                .unwrap_or(slow);
            assert_eq!(fast, slow, "fast vs slow differ for {c:?}");
            assert_eq!(pf, ps, "pending-CR result differs for {c:?}");
        }
        // An empty chunk keeps whatever pending-CR state it was given.
        let mut e = String::new();
        assert!(!normalize_line_endings("", false, &mut e));
        assert!(normalize_line_endings("", true, &mut e));
        assert!(e.is_empty());
        // The fast path is taken for a clean chunk (no special byte at all).
        assert_eq!(first_special("plain\nlines\twith\ttabs\n"), None);
        assert_eq!(first_special("café\n"), None);
        assert_eq!(first_special("a\r\n"), Some(1));
        assert_eq!(first_special("nel\u{85}"), Some(3));
        assert_eq!(first_special("x\u{2029}"), Some(1));
    }
    #[test]
    #[cfg(unix)]
    fn a_non_utf8_display_path_still_opens_the_real_file() {
        // R143: the model keys on the lossy string; the tail must open the REAL path.
        use std::os::unix::ffi::OsStrExt;
        let mut map: HashMap<String, PathBuf> = HashMap::new();
        let real = PathBuf::from(std::ffi::OsStr::from_bytes(b"/w/dir\xe9/inner.log"));
        remember_real_path(&mut map, &WatchEvent::Discovered(real.clone()));
        let display = real.to_string_lossy().to_string();
        assert!(display.contains('\u{FFFD}'));
        assert_eq!(map.get(&display), Some(&real));
        // A clean path is not remembered (it round-trips through its string).
        remember_real_path(
            &mut map,
            &WatchEvent::Activity(PathBuf::from("/w/clean.log")),
        );
        assert!(!map.contains_key("/w/clean.log"));
        // Path-less events are ignored.
        remember_real_path(&mut map, &WatchEvent::Recovered);
        assert_eq!(map.len(), 1);
    }
    #[test]
    fn restart_clears_the_real_path_map_with_the_model() {
        let (mut app, _id) = app_with_tail("x\n");
        app.real_paths
            .insert("/w/x\u{FFFD}.log".to_string(), PathBuf::from("/w/x.log"));
        let _ = update(&mut app, Message::Restart);
        assert!(app.real_paths.is_empty());
    }
    #[test]
    fn settings_poll_change_retunes_open_tails_live() {
        // R144: an OK with a new poll interval changes every open tail stream's cadence in place.
        let (mut app, id) = app_with_tail("x\n");
        assert_eq!(app.tails[&id].stream.poll_ms(), 100);
        let _ = update(&mut app, Message::OpenSettings);
        let _ = update(&mut app, Message::SettingsPollMsChanged("2000".to_string()));
        let _ = update(&mut app, Message::SettingsOk);
        assert_eq!(app.settings.poll_ms, 2000);
        assert_eq!(app.tails[&id].stream.poll_ms(), 2000);
    }
    #[test]
    fn discovered_clears_missing_for_a_known_file() {
        // REGRESSION (review #4 finding 16 — the concrete symptom of B10): after a Settings
        // poll-only restart the fresh service lists a known-but-Missing file as Discovered only;
        // the tile stayed red. A sweep that lists the file is proof it exists.
        let mut model = SessionModel::new();
        model.get_or_add("/w/a.log");
        model.mark_missing("/w/a.log", 0);
        assert_eq!(model.vis_of("/w/a.log", T0), ButtonVis::Missing);
        let opened = apply_event(
            &mut model,
            WatchEvent::Discovered(PathBuf::from("/w/a.log")),
        );
        assert_eq!(opened, None, "Discovered never auto-opens");
        assert_ne!(model.vis_of("/w/a.log", T0), ButtonVis::Missing);
    }

    #[test]
    fn enter_forces_a_pending_rescan_to_run_now() {
        // REGRESSION (REVIEW-2026-09-18 A2 / DECISIONS R165, .i57): while a large-buffer rescan is
        // PENDING, `matches` hold the PREVIOUS query. Enter (and Next) must FORCE the pending scan to
        // run immediately for the CURRENT query — not step the stale previous-query matches. Verifies
        // the deadline is cleared, the matches are recomputed for the new query, and the first match
        // is selected. (No trim needed here — this is the pending-window behaviour, not A1's trim.)
        let (mut app, id) = app_with_tail("");
        {
            let tw = app.tails.get_mut(&id).unwrap();
            // >4 MB so a keystroke defers (scan_inline is false). Interleave "err" and "warn" so the
            // two queries have DIFFERENT, non-empty match sets — proving the scan ran for the NEW one.
            let block = "err on this line\nwarn on this other line here padding padding padding\n";
            let mut s = String::with_capacity(5 * 1024 * 1024);
            while s.len() < 5 * 1024 * 1024 {
                s.push_str(block);
            }
            append_text(tw, &s);
            assert!(
                !search::scan_inline(tw.text.len()),
                "buffer is over the defer threshold"
            );
        }

        // First query "err": defers, then let the deferred scan populate matches.
        let _ = update(&mut app, Message::SearchChanged(id, "err".into()));
        assert!(
            app.tails[&id].search_rescan_due.is_some(),
            "large buffer defers"
        );
        for _ in 0..search::DEBOUNCE_TICKS {
            let _ = update(&mut app, Message::Tick);
        }
        let err_count = app.tails[&id].matches.len();
        assert!(err_count > 0, "the deferred 'err' scan populated matches");

        // Re-arm with a DIFFERENT query "warn": pending again, matches still hold the OLD "err" set.
        let _ = update(&mut app, Message::SearchChanged(id, "warn".into()));
        assert!(
            app.tails[&id].search_rescan_due.is_some(),
            "re-armed pending for the new query"
        );
        assert_eq!(
            app.tails[&id].matches.len(),
            err_count,
            "matches still the previous query's until a scan runs"
        );

        // Enter: force the pending scan NOW.
        let _ = update(&mut app, Message::EnterPressed(id));
        let tw = &app.tails[&id];
        assert_eq!(
            tw.search_rescan_due, None,
            "Enter cleared the pending deadline (forced now)"
        );
        assert!(
            !tw.matches.is_empty(),
            "forced scan repopulated for the new query"
        );
        // Every match now denotes the NEW query "warn", not "err".
        let all_warn = tw
            .matches
            .iter()
            .all(|m| tw.text[m.start..m.end].eq_ignore_ascii_case("warn"));
        assert!(
            all_warn,
            "forced scan recomputed for the CURRENT query 'warn'"
        );
        assert_eq!(
            tw.current_match,
            Some(0),
            "forced scan selects the first match (R165 = choice A)"
        );

        // Next (the '>' button message) also forces: re-arm and confirm.
        let _ = update(&mut app, Message::SearchChanged(id, "err".into()));
        assert!(
            app.tails[&id].search_rescan_due.is_some(),
            "re-armed for Next test"
        );
        let _ = update(&mut app, Message::SearchNext(id));
        assert_eq!(
            app.tails[&id].search_rescan_due, None,
            "Next also forced the pending scan"
        );
        assert!(app.tails[&id]
            .matches
            .iter()
            .all(|m| app.tails[&id].text[m.start..m.end].eq_ignore_ascii_case("err")));
    }

    #[test]
    fn trim_during_pending_debounce_drops_stale_matches() {
        // REGRESSION (REVIEW-2026-09-18 finding A1, .i56 fix): a >64 MB tail being searched arms a
        // pending debounced rescan (R162); an append tick trips the real scrollback trim (R104),
        // which shifts every byte offset. The pending branch used to KEEP the pre-trim matches, so
        // `matches` held pre-trim offsets against the post-trim buffer -> render_pieces slices a
        // line mid-char (panic) or highlights the wrong bytes and lies about the count. The fix
        // clears matches/current_match when a trim happens during the pending window; the deferred
        // rescan repopulates them. This test FAILS on shipped .i55 (100 000 stale matches) and
        // PASSES on .i56 (0 stale). It needs no hardware. (Probe: review-2026-09-18-probe.rs #3.)
        let (mut app, id, tx) = app_with_injected_tail();

        // Build a >64 MB buffer: mostly filler, with 'é' lines sprinkled throughout (every block) so
        // that whatever the trim's byte shift, some stale "ééé" offset can land inside an é run in
        // the trimmed buffer -> a non-char-boundary slice. Odd-length filler varies the parity.
        {
            let tw = app.tails.get_mut(&id).unwrap();
            let filler = "plain filler line with no match here at all whatsoever ok fine yesx\n"; // 67 B
            let e_line = "ééééééééééééééééééééééééééééééééééééééé\n"; // 37 é (74 B) + \n = 75 B
            let mut s = String::with_capacity(66 * 1024 * 1024);
            while s.len() < 65 * 1024 * 1024 {
                for _ in 0..9 {
                    s.push_str(filler);
                }
                s.push_str(e_line);
            }
            append_text(tw, &s);
            assert!(tw.text.len() > dirwatch_core::tail::SCROLLBACK_CAP_BYTES);
        }

        // Search "éé": large buffer, so this DEFERS (arms a pending rescan). To have matches present
        // during the pending window, let the deferred scan run once (populate), then re-arm with
        // another keystroke so a rescan is pending again while matches are populated.
        let _ = update(&mut app, Message::SearchChanged(id, "éé".into()));
        assert!(
            app.tails[&id].search_rescan_due.is_some(),
            "large buffer defers"
        );
        for _ in 0..search::DEBOUNCE_TICKS {
            let _ = update(&mut app, Message::Tick);
        }
        assert!(
            !app.tails[&id].matches.is_empty(),
            "the deferred scan populated matches"
        );

        // Re-arm: another keystroke on the large buffer -> pending again, matches still populated.
        let _ = update(&mut app, Message::SearchChanged(id, "ééé".into()));
        assert!(
            app.tails[&id].search_rescan_due.is_some(),
            "re-armed pending"
        );
        assert!(!app.tails[&id].matches.is_empty());

        // Inject a SMALL append (odd-length payload) so the buffer exceeds the cap and the Tick
        // trims; the odd length nudges the trim geometry so stale offsets could land mid-multibyte.
        tx.send(crate::runtime::TailChunk {
            new_text: Some("z\n".to_string()),
            ..Default::default()
        })
        .unwrap();
        let len_before_tick = app.tails[&id].text.len();
        let _ = update(&mut app, Message::Tick); // drains, trims, skips refresh (pending) — but clears on trim
        let tw = &app.tails[&id];
        assert!(
            tw.text.len() < len_before_tick,
            "the trim ran (buffer shrank)"
        );

        // POST-FIX ASSERTION: after a trim during the pending window, NO match may be a stale
        // offset — every retained match must be a valid char boundary AND correctly denote the
        // query, or (the fix's behaviour) the list is empty, awaiting the pending rescan.
        let q = tw.search_query.clone();
        let n_boundary_bad = tw
            .matches
            .iter()
            .filter(|m| m.start < tw.text.len() && !tw.text.is_char_boundary(m.start))
            .count();
        let n_wrong_text = tw
            .matches
            .iter()
            .filter(|m| {
                m.end <= tw.text.len()
                    && tw.text.is_char_boundary(m.start)
                    && tw.text.is_char_boundary(m.end)
                    && !tw.text[m.start..m.end].eq_ignore_ascii_case(&q)
            })
            .count();
        let n_out_of_range = tw.matches.iter().filter(|m| m.end > tw.text.len()).count();
        assert_eq!(
            n_boundary_bad + n_wrong_text + n_out_of_range,
            0,
            "a trim during a pending debounce must not leave stale search-match offsets \
             ({} non-char-boundary, {} wrong-text, {} out-of-range, {} total matches, text_len={})",
            n_boundary_bad,
            n_wrong_text,
            n_out_of_range,
            tw.matches.len(),
            tw.text.len()
        );
        // The fix clears on trim, so the list is empty until the pending rescan fires.
        assert!(
            tw.matches.is_empty() && tw.current_match.is_none(),
            "clear-on-trim: matches/current_match dropped, awaiting the deferred rescan"
        );
    }

    /// The `.i58` title defect (DECISIONS R169/R170): the MAIN-window title, and the catch-all arm,
    /// must show the product name "DirWatch 1.0" — NOT the full `build_info::BUILD_ID` build string,
    /// which the user specified is for `--version`/`--help`, logs, and crash stamps only. `boot()` sets
    /// `main_id` synchronously, so the main arm is exercised directly; a fresh unique id (not main,
    /// not Settings, not a tail) exercises the catch-all `else`.
    #[test]
    fn main_and_catchall_titles_show_product_name_not_the_build_string() {
        let (app, _task) = boot(
            WatchConfig {
                directory: std::env::temp_dir().join(format!("dwtitle_{}", std::process::id())),
                patterns: vec![],
                poll_interval_ms: 1000,
                depth: 0,
            },
            ModelOpts::default(),
        );
        let main = app.main_id.expect("boot sets main_id synchronously");

        // The exact requirement: the main-window title reads "DirWatch 1.0".
        assert_eq!(title(&app, main), "DirWatch 1.0");
        assert_eq!(title(&app, main), build_info::PRODUCT);

        // And it must NOT be the full build string — the regression this guards.
        assert_ne!(
            title(&app, main),
            build_info::BUILD_ID,
            "main title must not carry the full build string (the .i58 bug)"
        );
        assert!(
            !title(&app, main).contains("build "),
            "main title must not contain the 'build …' serial"
        );

        // The catch-all `else` (an id that is neither main, Settings, nor a tail) must behave the same.
        let orphan = window::Id::unique();
        assert_eq!(title(&app, orphan), "DirWatch 1.0");
        assert_ne!(title(&app, orphan), build_info::BUILD_ID);
    }
}
