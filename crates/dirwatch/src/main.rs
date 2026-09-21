//! DirWatch entrypoint (GUI + CLI).
//!
//! CROSS-PLATFORM (PORT-PLAN-crossplatform): the GUI is now iced, which runs on Windows, macOS,
//! and Linux from one codebase. native-windows-gui and all its Win32 machinery — the deep
//! subclassing, the `PrintWindow` screenshot harness, the Ctrl-handler — are RETIRED with the port
//! (DECISIONS R56).
//!
//! WINDOWS CLI OUTPUT (DECISIONS R91, .i26): DirWatch is a pure GUI-subsystem app on Windows
//! (`windows_subsystem = "windows"`), which is the ONLY design that guarantees NO console window
//! ever appears at launch — no "DOS box", and critically no console FLASH (the user's hard requirement:
//! a flashing console reads as malware in his field). A GUI-subsystem app also never holds the
//! terminal when launched from a shell. The cost of that bit is that a GUI-subsystem binary CANNOT
//! print reliably to cmd.exe (the shell doesn't wait for it; "the console subsystem cannot influence
//! that decision" — Microsoft Terminal spec #7335). The .i25 attempt (`AttachConsole`) proved this
//! on the user's hardware: output raced and vanished in cmd, and the bare GUI launch held the shell.
//!
//! So on WINDOWS the CLI text (`--version`/`--help`/`--license`/errors) is shown in a small iced
//! WINDOW (`cli_window`), not the console — shell-independent, works identically from cmd.exe,
//! PowerShell, and a double-click, with zero console involvement. On macOS/Linux there is no
//! subsystem problem, so those keep printing to the terminal normally (the user's choice 2, .i26). The
//! `windows_subsystem` attribute is a no-op on macOS/Linux.
//!
//! Every command-output block is framed with one blank line before and one after (the .NET
//! clean-output intent, entry 40a / D2); the window path shows the same framed text.

// Build the Windows binary as a GUI-subsystem app so launching it (double-click OR from a shell)
// opens NO console window and never holds the terminal. No-op on macOS/Linux (no subsystem concept).
// CLI text is routed to a window on Windows (see the module docs, R91) since a GUI-subsystem binary
// cannot print reliably to cmd.exe.
#![cfg_attr(windows, windows_subsystem = "windows")]

use dirwatch_core::{build_info, cli::CliOptions, license};
use std::path::PathBuf;

mod applog;
mod bench;
mod blink;
// The CLI-output window is Windows-only (R91): only Windows routes CLI text to a window; macOS/Linux
// print to the terminal. Gating the module keeps its iced code out of the non-Windows build entirely
// (and avoids dead-code warnings there under clippy -D warnings).
#[cfg(windows)]
mod cli_window;
mod gui;
mod placement;
mod runtime;
mod search;
mod visible;

fn main() {
    // Crash instrumentation (DECISIONS R66): a windows-subsystem / GUI process that panics can
    // vanish with no visible message. Install a panic hook that appends the panic payload +
    // location to `DirWatch_crash.log` beside the exe (where the debug log is written), so a
    // crash during interactive testing is captured and travels back in the artifact zip. The
    // default hook still runs after, so console/backtrace behavior is unchanged where visible.
    // `args_os` + lossy, not `args()` (review #6 finding 9, DECISIONS R147): `args()` PANICS on an
    // argument that is not valid Unicode (a Latin-1 directory name on Linux, an unpaired surrogate
    // on Windows) — and this runs before the crash logger below is installed, so on Windows the exe
    // simply "did nothing". The directory is re-resolved from a `String` anyway (R118).
    let raw: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let opts = CliOptions::parse(&raw);

    // Resolve the log directory ONCE, before anything can log (review #4 finding 10, DECISIONS
    // R127): `--log-dir` if given, else the per-user log directory for this OS, else beside the
    // exe. The panic hook below and the GUI's err/trace logs all write there.
    let _ = applog::init(opts.log_dir.as_deref().map(std::path::Path::new));
    install_crash_logger();

    // DEBUG-GATED search-scan bench (DECISIONS R161): with BOTH `DIRWATCH_DEBUG` and
    // `DIRWATCH_SEARCH_BENCH` set, measure the per-keystroke search scan cost across buffer sizes up
    // to the real load/scrollback clamps, write the numbers to `DirWatch.log`, and EXIT without
    // opening the GUI — a fast, deterministic, window-free run for the harness. Never fires for a
    // normal user (both gates required); no product behaviour changes.
    if bench::requested() {
        bench::run_and_log();
        return;
    }

    // Information/utility flags short-circuit, in the .NET precedence order. Each builds its framed
    // text (one blank line before + after — defect D2 / entry 40a) and hands it to `emit`, which
    // prints to the terminal on macOS/Linux and shows a window on Windows (R91, see module docs).
    //
    // The routing decision is the pure `decide_launch` (tested), so a refactor can't silently send a
    // CLI flag down the GUI path — which on Windows would mean its output goes nowhere. R90/R91.
    match decide_launch(&opts) {
        LaunchPath::Version => emit("DirWatch — Version", build_info::banner()),
        LaunchPath::Help => emit("DirWatch — Help", CliOptions::help_text().trim_end()),
        LaunchPath::License => emit("DirWatch — License", license::TEXT),
        LaunchPath::Error => {
            let mut msg = String::new();
            msg.push_str(build_info::banner());
            msg.push('\n');
            for e in &opts.errors {
                msg.push_str(&format!("error: {e}\n"));
            }
            msg.push('\n');
            msg.push_str(CliOptions::help_text().trim_end());
            emit_err("DirWatch — Error", &msg);
            std::process::exit(2);
        }
        // No utility flag: launch the iced GUI (all three OSes).
        LaunchPath::Gui => launch(&opts),
    }
}

/// Show a CLI-output block. macOS/Linux: print to stdout, framed (D2). Windows: show it in an iced
/// window instead of the console, because a GUI-subsystem binary cannot print reliably to cmd.exe
/// (R91). `title` names the window on Windows and is unused on macOS/Linux.
#[cfg(windows)]
fn emit(title: &str, body: &str) {
    // A failed window launch is not worth crashing over; best-effort like the rest of the CLI path.
    let _ = cli_window::show(title.to_string(), frame(body));
}

/// macOS/Linux: print the framed block to stdout (the terminal handles it; no subsystem problem).
#[cfg(not(windows))]
fn emit(_title: &str, body: &str) {
    print!("{}", frame(body));
}

/// Error variant of [`emit`]. Windows: same window (a window has no stderr distinction). macOS/Linux:
/// framed to stderr, matching the prior `block_err` behavior.
#[cfg(windows)]
fn emit_err(title: &str, body: &str) {
    let _ = cli_window::show(title.to_string(), frame(body));
}

#[cfg(not(windows))]
fn emit_err(_title: &str, body: &str) {
    eprint!("{}", frame(body));
}

/// Frame a command-output block with one blank line before and one after (defect D2 / .NET
/// clean-output intent, entry 40a). Used by both the terminal and the window paths so the text is
/// identical on every OS.
fn frame(body: &str) -> String {
    format!("\n{body}\n")
}

/// Which top-level path a parsed command line takes. The CLI variants (all but `Gui`) print to the
/// terminal and are the ones the Windows console attach (R90) exists to serve; `Gui` needs no
/// console. Pulled out of `main` as a PURE decision so it can be unit-tested — a guard against a
/// refactor silently routing a CLI flag to the GUI (and losing its output on Windows).
#[derive(Debug, PartialEq, Eq)]
enum LaunchPath {
    Version,
    Help,
    License,
    Error,
    Gui,
}

/// Decide the launch path from parsed options, in the .NET precedence order (version, help,
/// license, then errors, else GUI). Pure: no I/O, no globals.
fn decide_launch(opts: &CliOptions) -> LaunchPath {
    if opts.show_version {
        LaunchPath::Version
    } else if opts.show_help {
        LaunchPath::Help
    } else if opts.show_license {
        LaunchPath::License
    } else if !opts.errors.is_empty() {
        LaunchPath::Error
    } else {
        LaunchPath::Gui
    }
}

/// Launch the iced GUI. The directory defaults to the current directory when none was passed,
/// matching the .NET behavior and the help text; patterns/depth/poll/model-knobs come straight
/// from the parsed CLI, so `dirwatch <dir> -d 1 --patterns "*.log"` launches straight into
/// watching that config (step-1 scope: it starts watching immediately; a Start/Stop toggle in the
/// UI is a later increment).
fn launch(opts: &CliOptions) {
    let directory = opts
        .directory
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let cfg = runtime::WatchConfig {
        directory,
        patterns: opts.patterns.clone(),
        // Floor AND ceiling (review #4 finding 19): the Settings dialog already clamped to
        // [100, 60 000] ms; the CLI only had the floor, so `--poll-ms 2000000000` was a 23-day poll.
        poll_interval_ms: opts.poll_interval_ms.unwrap_or(250).clamp(
            dirwatch_core::POLL_INTERVAL_FLOOR_MS as i32,
            dirwatch_core::POLL_INTERVAL_CEILING_MS as i32,
        ) as u32,
        depth: opts
            .depth
            .unwrap_or(0)
            .clamp(0, dirwatch_core::watch::MAX_DEPTH),
    };
    let model_opts = gui::ModelOpts {
        no_open: opts.no_open,
        active_seconds: opts.active_seconds,
        max_windows: opts.max_windows,
    };
    if let Err(e) = gui::run(cfg, model_opts) {
        eprintln!("{}", build_info::banner());
        eprintln!("error: the DirWatch GUI failed to start: {e}");
        std::process::exit(1);
    }
}

/// Install a panic hook that appends crash details to `DirWatch_crash.log` in the log directory
/// (`applog`, R127 — was beside the exe, R66), then chains to the default hook. Best-effort: a
/// failure to write is swallowed (we're already panicking). The build ID is stamped in so a
/// captured crash ties to the build under test. Same 5 MB cap + one rotation as the main log.
fn install_crash_logger() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let loc = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown location>".to_string());
        let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "<non-string panic payload>".to_string()
        };
        applog::append_line(
            applog::CRASH_FILE,
            &format!(
                "=== PANIC in {} ===\n  at: {}\n  message: {}\n  backtrace:\n{:?}\n",
                build_info::BUILD_ID,
                loc,
                msg,
                std::backtrace::Backtrace::force_capture()
            ),
        );
        // Chain to the default hook (console message where visible).
        default(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::{decide_launch, LaunchPath};
    use dirwatch_core::cli::CliOptions;

    fn parse(args: &[&str]) -> CliOptions {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        CliOptions::parse(&owned)
    }

    // R90 (.i25): the console attach only serves the CLI paths, so the routing that decides CLI vs.
    // GUI is load-bearing on Windows — a flag wrongly routed to the GUI would lose its output (no
    // console). These lock the routing decision (which is independent of the OS/attach itself).

    #[test]
    fn version_routes_to_version() {
        assert_eq!(decide_launch(&parse(&["--version"])), LaunchPath::Version);
    }

    #[test]
    fn help_routes_to_help() {
        assert_eq!(decide_launch(&parse(&["--help"])), LaunchPath::Help);
        assert_eq!(decide_launch(&parse(&["-h"])), LaunchPath::Help);
    }

    #[test]
    fn license_routes_to_license() {
        assert_eq!(decide_launch(&parse(&["--license"])), LaunchPath::License);
    }

    #[test]
    fn bad_flag_routes_to_error() {
        assert_eq!(decide_launch(&parse(&["--bogus"])), LaunchPath::Error);
    }

    #[test]
    fn bare_and_dir_route_to_gui() {
        // No flags at all, and a plain directory arg, both launch the GUI — the ONLY path that does
        // not need the Windows console.
        assert_eq!(decide_launch(&parse(&[])), LaunchPath::Gui);
        assert_eq!(decide_launch(&parse(&["/tmp/logs"])), LaunchPath::Gui);
        assert_eq!(
            decide_launch(&parse(&["/tmp/logs", "-d", "1", "--no-open"])),
            LaunchPath::Gui
        );
    }

    #[test]
    fn version_wins_over_gui_args() {
        // Precedence: a utility flag alongside GUI-style args still takes the CLI path (so its
        // output is produced, not swallowed by a GUI launch).
        assert_eq!(
            decide_launch(&parse(&["/tmp/logs", "--version"])),
            LaunchPath::Version
        );
    }

    #[test]
    fn version_precedes_help_precedes_license() {
        // The .NET precedence order, pinned (--selftest removed at .i31, DECISIONS R106).
        assert_eq!(
            decide_launch(&parse(&["--license", "--help", "--version"])),
            LaunchPath::Version
        );
        assert_eq!(
            decide_launch(&parse(&["--license", "--help"])),
            LaunchPath::Help
        );
        assert_eq!(decide_launch(&parse(&["--license"])), LaunchPath::License);
    }
}
