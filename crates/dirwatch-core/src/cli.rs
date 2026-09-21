//! Command-line parsing. Ported from the .NET `CliOptions` (behavioral spec, build 2026-07-14.9).
//!
//! Every selection the GUI exposes (directory, patterns, poll interval) is also settable here
//! (CLI-or-GUI parity). A bare (non-flag) argument is the positional directory to watch; the
//! first one wins, a second is an error.

use crate::build_info;
use crate::glob::GlobMatcher;
use crate::watch::MAX_DEPTH;

/// Parsed command line.
#[derive(Debug, Default, Clone)]
pub struct CliOptions {
    pub directory: Option<String>,
    pub patterns: Vec<String>,
    pub poll_interval_ms: Option<i32>,
    pub depth: Option<i32>,
    pub no_open: bool,
    pub active_seconds: Option<i32>,
    pub max_windows: Option<i32>,
    pub show_help: bool,
    pub show_version: bool,
    pub show_license: bool,
    /// `--log-dir <path>`: where `DirWatch.log` / `DirWatch_crash.log` are written (review #4
    /// finding 10, DECISIONS R127). `None` = the per-OS default the GUI resolves.
    pub log_dir: Option<String>,
    pub errors: Vec<String>,
}

impl CliOptions {
    /// Parse argv (excluding the program name).
    pub fn parse(args: &[String]) -> CliOptions {
        let mut o = CliOptions::default();
        let mut i = 0;
        while i < args.len() {
            let a = &args[i];
            match a.to_lowercase().as_str() {
                "--help" | "-h" | "/?" => o.show_help = true,
                "--version" => o.show_version = true,
                "--license" => o.show_license = true,
                "--pattern" | "-p" => {
                    if i + 1 < args.len() {
                        i += 1;
                        o.patterns.push(args[i].clone());
                    } else {
                        o.errors.push("--pattern requires a glob".to_string());
                    }
                }
                "--patterns" => {
                    if i + 1 < args.len() {
                        i += 1;
                        for part in args[i].split([';', ',']) {
                            let t = part.trim();
                            if !t.is_empty() {
                                o.patterns.push(t.to_string());
                            }
                        }
                    } else {
                        o.errors
                            .push("--patterns requires a ;-separated list".to_string());
                    }
                }
                "--poll-ms" => {
                    if i + 1 < args.len() {
                        i += 1;
                        match args[i].parse::<i32>() {
                            Ok(ms) => o.poll_interval_ms = Some(ms),
                            Err(_) => o.errors.push("--poll-ms requires an integer".to_string()),
                        }
                    } else {
                        o.errors.push("--poll-ms requires an integer".to_string());
                    }
                }
                "--depth" | "-d" => {
                    if i + 1 < args.len() {
                        i += 1;
                        match args[i].parse::<i32>() {
                            Ok(d) => o.depth = Some(d.clamp(0, MAX_DEPTH)),
                            Err(_) => o
                                .errors
                                .push("--depth requires a non-negative integer".to_string()),
                        }
                    } else {
                        o.errors
                            .push("--depth requires a non-negative integer".to_string());
                    }
                }
                "--no-open" => o.no_open = true,
                "--log-dir" => {
                    if i + 1 < args.len() {
                        i += 1;
                        o.log_dir = Some(args[i].clone());
                    } else {
                        o.errors
                            .push("--log-dir requires a directory path".to_string());
                    }
                }
                "--active-secs" => {
                    if i + 1 < args.len() {
                        i += 1;
                        match args[i].parse::<i32>() {
                            Ok(s) => o.active_seconds = Some(s.max(1)),
                            Err(_) => o
                                .errors
                                .push("--active-secs requires an integer".to_string()),
                        }
                    } else {
                        o.errors
                            .push("--active-secs requires an integer".to_string());
                    }
                }
                "--max-windows" | "-w" => {
                    if i + 1 < args.len() {
                        i += 1;
                        match args[i].parse::<i32>() {
                            // Floor 1, ceiling MAX_WINDOWS_CEILING (50) — DECISIONS R29.
                            Ok(mw) => {
                                o.max_windows =
                                    Some(mw.clamp(1, crate::session::MAX_WINDOWS_CEILING))
                            }
                            Err(_) => o
                                .errors
                                .push("--max-windows requires an integer".to_string()),
                        }
                    } else {
                        o.errors
                            .push("--max-windows requires an integer".to_string());
                    }
                }
                _ => {
                    // A bare (non-flag) argument is the positional directory. First one wins.
                    if !a.starts_with('-') && o.directory.is_none() {
                        o.directory = Some(a.clone());
                    } else {
                        o.errors.push(format!("unknown argument: {a}"));
                    }
                }
            }
            i += 1;
        }
        o
    }

    /// The full help text (matches the .NET `HelpText`).
    pub fn help_text() -> String {
        let defaults = GlobMatcher::DEFAULT_PATTERNS_STR;
        // Option rows in the user's order (R38): directory, help, depth, pattern(s), max-windows, then
        // the rest. Descriptions are LEFT-ALIGNED to a fixed column (`DESC_COL`) regardless of how
        // long the option token is, so they form a clean vertical line.
        const DESC_COL: usize = 24;
        let rows: &[(&str, &str)] = &[
            (
                "[directory]",
                "Directory to watch (positional; default: current directory)",
            ),
            ("-h, --help", "Show this help"),
            (
                "-d, --depth <n>",
                "Watch subdirectories down to n levels (0 = top level, max 10)",
            ),
            (
                "-w, --max-windows <n>",
                "Max tail windows open at once (default 10, max 50)",
            ),
            (
                "-p, --pattern <glob>",
                "Add a filename pattern (repeatable), e.g. step*.log",
            ),
            (
                "    --patterns <list>",
                "Semicolon/comma-separated patterns, e.g. \"*.log;*.txt\"",
            ),
            (
                "    --poll-ms <n>",
                "Polling fallback interval in ms (default 250)",
            ),
            (
                "    --active-secs <n>",
                "Mark a file 'active' for n seconds after a write (default 5)",
            ),
            (
                "    --no-open",
                "Do not auto-open windows; open only on click",
            ),
            (
                "    --log-dir <path>",
                "Write DirWatch.log / DirWatch_crash.log here (see Logs below)",
            ),
            ("    --version", "Print build ID and exit"),
            ("    --license", "Print the license (MIT) and exit"),
        ];
        let mut opts = String::new();
        for (token, desc) in rows {
            // Two-space left margin + the token, padded so the description starts at DESC_COL.
            let pad = DESC_COL.saturating_sub(token.len());
            opts.push_str(&format!("  {token}{}{desc}\n", " ".repeat(pad.max(1))));
        }
        // Leading blank line before the banner (R38); blank line after the Defaults line (R38).
        format!(
            "\n{banner}\n\n\
Watches a directory and opens a tail window for each new/appended .log/.txt file.\n\n\
Usage: DirWatch [directory] [options]\n\n\
{opts}\n\
Defaults: patterns {defaults}\n\n\
Environment:\n\
\x20 DIRWATCH_DEBUG=1       Write a verbose diagnostic trace to DirWatch.log\n\
\x20                       (default: only real errors are logged)\n\n\
Logs:\n\
\x20 DirWatch.log (errors; the trace with DIRWATCH_DEBUG) and DirWatch_crash.log are\n\
\x20 written to --log-dir if given, else the per-user log directory:\n\
\x20   Windows  %LOCALAPPDATA%\\DirWatch\\\n\
\x20   macOS    ~/Library/Logs/DirWatch/\n\
\x20   Linux    $XDG_STATE_HOME/dirwatch/  (default ~/.local/state/dirwatch/)\n\
\x20 falling back to the directory beside the executable. Each log is capped at\n\
\x20 5 MB and rotated once to a .1 file. A double-clicked launch has no command\n\
\x20 line: put --log-dir in the shortcut's target to use it there.\n\n\
With no options, DirWatch watches the current directory. Watch multiple\n\
directories by launching one instance per directory.\n",
            banner = build_info::banner(),
            opts = opts,
            defaults = defaults,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> CliOptions {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        CliOptions::parse(&owned)
    }

    // Ported from CliOptionsTests.cs (parity oracle).

    #[test]
    fn parses_depth() {
        let o = parse(&["--depth", "2"]);
        assert_eq!(o.depth, Some(2));
        assert!(o.errors.is_empty());
    }

    #[test]
    fn depth_negative_clamped_to_zero() {
        let o = parse(&["--depth", "-3"]);
        assert_eq!(o.depth, Some(0));
        assert!(o.errors.is_empty());
    }

    #[test]
    fn depth_over_max_clamped_to_ten() {
        let o = parse(&["--depth", "99"]);
        assert_eq!(o.depth, Some(10));
        assert!(o.errors.is_empty());
    }

    #[test]
    fn parses_no_open() {
        let o = parse(&["--no-open"]);
        assert!(o.no_open);
    }

    #[test]
    fn parses_active_secs_and_max_windows() {
        let o = parse(&["--active-secs", "5", "--max-windows", "3"]);
        assert_eq!(o.active_seconds, Some(5));
        assert_eq!(o.max_windows, Some(3));
        assert!(o.errors.is_empty());
    }

    #[test]
    fn unknown_switch_is_an_error() {
        let o = parse(&["--bogus"]);
        assert!(!o.errors.is_empty());
    }

    #[test]
    fn missing_value_is_an_error() {
        let o = parse(&["--depth"]);
        assert!(!o.errors.is_empty());
    }

    #[test]
    fn directory_is_a_positional_argument() {
        let o = parse(&["/tmp/logs"]);
        assert_eq!(o.directory.as_deref(), Some("/tmp/logs"));
        assert!(o.errors.is_empty());
    }

    #[test]
    fn dash_d_is_depth() {
        let o = parse(&["-d", "2"]);
        assert_eq!(o.depth, Some(2));
        assert!(o.errors.is_empty());
    }

    #[test]
    fn max_windows_clamped_to_ceiling() {
        let o = parse(&["--max-windows", "9999"]);
        assert_eq!(o.max_windows, Some(crate::session::MAX_WINDOWS_CEILING));
        assert!(o.errors.is_empty());
    }

    #[test]
    fn max_windows_floored_at_one() {
        let o = parse(&["--max-windows", "0"]);
        assert_eq!(o.max_windows, Some(1));
    }

    #[test]
    fn dash_w_is_max_windows() {
        let o = parse(&["-w", "3"]);
        assert_eq!(o.max_windows, Some(3));
        assert!(o.errors.is_empty());
    }

    #[test]
    fn parses_license() {
        let o = parse(&["--license"]);
        assert!(o.show_license);
    }

    #[test]
    fn second_positional_is_an_error() {
        let o = parse(&["/one", "/two"]);
        assert_eq!(o.directory.as_deref(), Some("/one"));
        assert!(!o.errors.is_empty());
    }

    #[test]
    fn combines_positional_dir_and_switches() {
        let o = parse(&[
            "/tmp",
            "-d",
            "1",
            "--no-open",
            "-w",
            "4",
            "--active-secs",
            "7",
            "--poll-ms",
            "250",
        ]);
        assert_eq!(o.directory.as_deref(), Some("/tmp"));
        assert_eq!(o.depth, Some(1));
        assert!(o.no_open);
        assert_eq!(o.max_windows, Some(4));
        assert_eq!(o.active_seconds, Some(7));
        assert_eq!(o.poll_interval_ms, Some(250));
        assert!(o.errors.is_empty());
    }

    #[test]
    fn help_active_secs_spells_out_seconds() {
        // R43: the --active-secs line must read "n seconds", not the terse solo "n s".
        let help = CliOptions::help_text();
        assert!(
            help.contains("for n seconds after a write"),
            "help should spell out 'seconds'"
        );
        assert!(
            !help.contains("for n s after a write"),
            "help must not use the terse 'n s'"
        );
    }

    #[test]
    fn parses_log_dir_and_requires_a_value() {
        // review #4 finding 10 (DECISIONS R127): `--log-dir <path>` names the log directory.
        let o = parse(&["--log-dir", "C:\\logs\\dw"]);
        assert_eq!(o.log_dir.as_deref(), Some("C:\\logs\\dw"));
        assert!(o.errors.is_empty());
        let o = parse(&["--log-dir"]);
        assert!(o.log_dir.is_none());
        assert!(!o.errors.is_empty());
    }

    #[test]
    fn help_documents_log_dir_and_the_per_os_log_locations() {
        let help = CliOptions::help_text();
        assert!(help.contains("--log-dir <path>"));
        assert!(help.contains("Logs:"));
        assert!(help.contains("%LOCALAPPDATA%\\DirWatch\\"));
        assert!(help.contains("~/Library/Logs/DirWatch/"));
        assert!(help.contains("$XDG_STATE_HOME/dirwatch/"));
        assert!(help.contains("DirWatch.log"));
        assert!(
            !help.contains("DirWatch_debug.log"),
            "the old name must not linger in help"
        );
    }

    #[test]
    fn help_documents_the_debug_env_var() {
        // .i30 (review B6, DECISIONS R103): the verbose trace moved behind DIRWATCH_DEBUG; --help
        // must tell the user the lever exists (it's an env var, not a flag, so it lives in an
        // Environment section, not the option rows).
        let help = CliOptions::help_text();
        assert!(
            help.contains("DIRWATCH_DEBUG"),
            "help must document the DIRWATCH_DEBUG environment variable"
        );
        assert!(
            help.contains("Environment:"),
            "help must have an Environment section"
        );
    }
}
