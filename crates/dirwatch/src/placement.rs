//! Tail-window placement: edge-aware first window + cascade with wrap.
//!
//! Pure geometry, no iced types, so it is UNIT-TESTABLE off-hardware (the one part of the
//! multi-window work that can be verified without a real screen). Mirrors the intent of the retired
//! NWG `next_position` (DECISIONS entries 33/35d, R19): the FIRST tail window is placed edge-aware -
//! toward whichever side of the screen has room - and each SUBSEQUENT window cascades diagonally
//! from the last, wrapping back near the start so a long session's windows never march off-screen.

/// A screen rectangle / window size in logical pixels. Kept as a plain tuple-y struct so tests
/// don't need iced.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Inputs for placing the Nth tail window.
#[derive(Debug, Clone, Copy)]
pub struct PlaceInput {
    /// The usable screen area (monitor work area).
    pub screen: Rect,
    /// The tail window's size.
    pub win_w: f32,
    pub win_h: f32,
    /// The main window's rect, so the first tail window can avoid covering it and pick the side
    /// with room.
    pub main: Rect,
    /// How many tail windows are already open (0 for the first). Drives the cascade step + wrap.
    pub open_index: usize,
}

/// Per-window cascade offset and how many steps before wrapping (so windows stay on-screen).
const CASCADE_STEP: f32 = 30.0;
const CASCADE_WRAP: usize = 12;

/// Compute the top-left position for the next tail window.
///
/// FIRST window (`open_index == 0`): edge-aware. Prefer the space to the RIGHT of the main window;
/// if there isn't room for the tail window there, use the space to the LEFT; if neither side fits,
/// fall back to just inside the screen's top-left. This reproduces the NWG "opens LEFT when no room
/// right" behavior.
///
/// SUBSEQUENT windows: start from the first window's anchor and cascade diagonally by
/// `CASCADE_STEP * (open_index mod CASCADE_WRAP)`, clamped so the whole window stays within the
/// screen (wrap keeps it bounded; the clamp is a final safety net).
pub fn next_position(input: PlaceInput) -> (f32, f32) {
    let (ax, ay) = anchor(input);

    let step = CASCADE_STEP * (input.open_index % CASCADE_WRAP) as f32;
    let mut x = ax + step;
    let mut y = ay + step;

    // Keep the whole window on-screen.
    let max_x = input.screen.x + input.screen.w - input.win_w;
    let max_y = input.screen.y + input.screen.h - input.win_h;
    x = x.clamp(input.screen.x, max_x.max(input.screen.x));
    y = y.clamp(input.screen.y, max_y.max(input.screen.y));
    (x, y)
}

/// The anchor (where window 0 goes / cascade starts): edge-aware around the main window.
fn anchor(input: PlaceInput) -> (f32, f32) {
    let screen_right = input.screen.x + input.screen.w;
    let main_right = input.main.x + input.main.w;

    // Room to the RIGHT of the main window?
    let room_right = screen_right - main_right;
    if room_right >= input.win_w {
        // Place just right of the main window, vertically aligned to its top.
        return (main_right + 8.0, input.main.y);
    }
    // Room to the LEFT of the main window?
    let room_left = input.main.x - input.screen.x;
    if room_left >= input.win_w {
        return (input.main.x - input.win_w - 8.0, input.main.y);
    }
    // Neither side fits: top-left of the screen, nudged in.
    (input.screen.x + 20.0, input.screen.y + 20.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen() -> Rect {
        Rect {
            x: 0.0,
            y: 0.0,
            w: 1920.0,
            h: 1080.0,
        }
    }

    fn main_left() -> Rect {
        // Main window near the left, leaving room on the right.
        Rect {
            x: 40.0,
            y: 40.0,
            w: 880.0,
            h: 600.0,
        }
    }

    #[test]
    fn first_window_opens_to_the_right_when_there_is_room() {
        let p = next_position(PlaceInput {
            screen: screen(),
            win_w: 500.0,
            win_h: 400.0,
            main: main_left(),
            open_index: 0,
        });
        // Just right of the main window (main_right 920 + 8), at the main's top.
        assert_eq!(p, (928.0, 40.0));
    }

    #[test]
    fn first_window_opens_left_when_no_room_right() {
        // Main window pushed far right so there's no room on the right for the tail window.
        let main = Rect {
            x: 1400.0,
            y: 50.0,
            w: 480.0,
            h: 600.0,
        };
        let p = next_position(PlaceInput {
            screen: screen(),
            win_w: 500.0,
            win_h: 400.0,
            main,
            open_index: 0,
        });
        // No room right (screen_right 1920 - main_right 1880 = 40 < 500): open LEFT of the main.
        // main.x 1400 - win_w 500 - 8 = 892.
        assert_eq!(p, (892.0, 50.0));
    }

    #[test]
    fn cascade_offsets_each_subsequent_window() {
        let base = PlaceInput {
            screen: screen(),
            win_w: 500.0,
            win_h: 400.0,
            main: main_left(),
            open_index: 0,
        };
        let p0 = next_position(base);
        let p1 = next_position(PlaceInput {
            open_index: 1,
            ..base
        });
        let p2 = next_position(PlaceInput {
            open_index: 2,
            ..base
        });
        assert_eq!(p1.0 - p0.0, CASCADE_STEP);
        assert_eq!(p1.1 - p0.1, CASCADE_STEP);
        assert_eq!(p2.0 - p0.0, 2.0 * CASCADE_STEP);
    }

    #[test]
    fn cascade_wraps_so_windows_do_not_march_off_screen() {
        let base = PlaceInput {
            screen: screen(),
            win_w: 500.0,
            win_h: 400.0,
            main: main_left(),
            open_index: 0,
        };
        // Index 0 and index CASCADE_WRAP land on the same offset (mod wrap == 0).
        let p0 = next_position(base);
        let pw = next_position(PlaceInput {
            open_index: CASCADE_WRAP,
            ..base
        });
        assert_eq!(p0, pw);
    }

    #[test]
    fn windows_stay_within_the_screen() {
        // A huge index would cascade off-screen without the wrap+clamp; assert it stays inside.
        let p = next_position(PlaceInput {
            screen: screen(),
            win_w: 500.0,
            win_h: 400.0,
            main: main_left(),
            open_index: 9999,
        });
        assert!(p.0 >= 0.0 && p.0 <= 1920.0 - 500.0);
        assert!(p.1 >= 0.0 && p.1 <= 1080.0 - 400.0);
    }
}
