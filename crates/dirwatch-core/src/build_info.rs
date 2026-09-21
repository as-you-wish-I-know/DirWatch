//! Single source of truth for the build identifier.
//!
//! Ported faithfully from the .NET `BuildInfo` (behavioral spec, build 2026-07-14.9).
//! Printed by CLI output (--version), shown in the GUI, and stamped into every debug
//! log so any artifact ties unambiguously to the build that produced it. Keep this until a
//! release build explicitly drops it (collaboration prompt: embedded build ID).

/// Product version.
pub const VERSION: &str = "1.0.0";

/// Product name + major line, for window TITLES (DECISIONS R170). This is what a titlebar or
/// taskbar entry shows — NOT the full [`BUILD_ID`], which the user specified is for `--version`/`--help`,
/// logs, and crash stamps ONLY. Kept here beside `VERSION`/`BUILD_ID` so the three can never drift:
/// bumping the major line updates the title in one place.
pub const PRODUCT: &str = "DirWatch 1.0";

/// Human-readable build ID. Bump the trailing serial on every new build so a stale `test/`
/// directory is caught by an ID mismatch.
///
/// NOTE: this is the RUST PORT. The `.iN` serials mark the CROSS-PLATFORM iced port line
/// (DECISIONS R56): NWG was retired and the GUI rebuilt in iced, so the serial scheme changes
/// from `.rNN` (NWG) to `.iN` (iced). It stays sortable — `2026-07-16.r38` precedes
/// `2026-07-27.i1` lexically by date. The NWG `.r38` build remains available to the user as the
/// Windows fallback until the iced port reaches parity and its own sign-off.
pub const BUILD_ID: &str = "DirWatch 1.0 build 2026-09-20.i61";

/// The banner string shown to identify the build.
pub fn banner() -> &'static str {
    BUILD_ID
}
