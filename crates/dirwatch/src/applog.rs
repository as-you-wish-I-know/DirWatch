//! Application logging: WHERE `DirWatch.log` / `DirWatch_crash.log` live and HOW they are written
//! (review #4 finding 10, DECISIONS R127).
//!
//! Until `.i39` both logs went beside the executable. That is writable in a scratch `test\` tree
//! and at the user's PATH location, and nowhere a packaged build lands: an AppImage runs from a
//! read-only squashfs mount, a `.app` in `/Applications` is not writable by a standard user, and
//! `Program Files` needs elevation — so on every shipped shape the write silently failed and "what
//! happened at 2 am" had no answer. Logs now go to the platform's per-user log directory:
//!
//!   * Windows: `%LOCALAPPDATA%\DirWatch\` (Local, not Roaming: logs never roam)
//!   * macOS: `~/Library/Logs/DirWatch/` (Apple's per-user log location; Console.app lists it)
//!   * Linux: `$XDG_STATE_HOME/dirwatch/`, default `~/.local/state/dirwatch/` (XDG Base
//!     Directory 0.8: the STATE dir is defined for "logs, history")
//!
//! `--log-dir <path>` overrides (CLI only, the user's 3-A); the directory beside the executable is the
//! fallback when the chosen one cannot be created or written; failing that, silence (as before).
//! Resolved ONCE at start-up (`init`) and shared by the error log, the opt-in trace, and the panic
//! hook, so the crash log can never land somewhere other than the debug log.
//!
//! NAMES (the user's 2-A): `DirWatch.log` — real errors always, the operational trace only under
//! `DIRWATCH_DEBUG` (R103, unchanged) — and `DirWatch_crash.log`. The old `DirWatch_debug.log`
//! name invited "is the debug log on by default?"; it never was.
//!
//! GROWTH (the user's 4-A): each file is capped at [`LOG_CAP_BYTES`] with ONE rotation — when an
//! append would cross the cap the file is renamed to `<name>.1` (overwriting the previous `.1`)
//! and a fresh file starts. Worst case on disk is 2 × 5 MB per log; no knob, no timer, no
//! dependency; the check is one `metadata` per write, and writes only happen on errors or with
//! the trace on.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The error + opt-in-trace log (was `DirWatch_debug.log` until `.i39`).
pub const LOG_FILE: &str = "DirWatch.log";
/// The panic-hook log (DECISIONS R66; unchanged name, new location).
pub const CRASH_FILE: &str = "DirWatch_crash.log";
/// Per-file size cap; crossing it rotates the file to `<name>.1` (one generation kept).
pub const LOG_CAP_BYTES: u64 = 5 * 1024 * 1024;
/// The product's directory name under the per-user log root (Windows/macOS casing).
const APP_DIR: &str = "DirWatch";
/// …and its lower-case Linux/XDG spelling.
const APP_DIR_XDG: &str = "dirwatch";

static DIR: OnceLock<PathBuf> = OnceLock::new();

/// Which platform's convention to apply. A parameter (not `cfg!` inside `resolve`) so every arm is
/// unit-testable on any host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Windows,
    Mac,
    Other,
}

impl Os {
    pub fn current() -> Os {
        if cfg!(windows) {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::Mac
        } else {
            Os::Other
        }
    }
}

/// PURE: the log directory for `os`, given the CLI override, an environment lookup, and the
/// executable's directory (the last-resort fallback). No filesystem access — creation and
/// writability are `init`'s and `append_line`'s business.
pub fn resolve(
    os: Os,
    override_dir: Option<&Path>,
    env: &dyn Fn(&str) -> Option<OsString>,
    exe_dir: &Path,
) -> PathBuf {
    if let Some(o) = override_dir {
        return o.to_path_buf();
    }
    let nonempty = |k: &str| env(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let candidate = match os {
        Os::Windows => nonempty("LOCALAPPDATA")
            .map(|p| p.join(APP_DIR))
            .or_else(|| {
                nonempty("USERPROFILE").map(|p| p.join("AppData").join("Local").join(APP_DIR))
            }),
        Os::Mac => nonempty("HOME").map(|p| p.join("Library").join("Logs").join(APP_DIR)),
        Os::Other => nonempty("XDG_STATE_HOME")
            // The XDG spec says a relative $XDG_STATE_HOME is invalid and must be ignored. POSIX
            // semantics ("starts with /"), NOT `Path::is_absolute` — on a Windows host that means
            // "has a drive letter", which is what failed the .i40 Windows gate on this very test
            // (the Linux arm is exercised on every host; R129).
            .filter(|p| p.to_string_lossy().starts_with('/'))
            .map(|p| p.join(APP_DIR_XDG))
            .or_else(|| nonempty("HOME").map(|p| p.join(".local").join("state").join(APP_DIR_XDG))),
    };
    candidate.unwrap_or_else(|| exe_dir.to_path_buf())
}

/// Directory of the running executable (the fallback location), best-effort.
pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Resolve the log directory ONCE for this process (call before anything logs). Creates it; if
/// that fails, the executable's directory is used instead. Returns the directory in force.
pub fn init(override_dir: Option<&Path>) -> PathBuf {
    let chosen = resolve(
        Os::current(),
        override_dir,
        &|k| std::env::var_os(k),
        &exe_dir(),
    );
    let dir = if std::fs::create_dir_all(&chosen).is_ok() {
        chosen
    } else {
        exe_dir()
    };
    // A second `init` (tests) keeps the first value; that is the "once" this exists for.
    DIR.get_or_init(|| dir).clone()
}

/// The directory in force (initialising with the defaults if `init` was never called).
pub fn dir() -> PathBuf {
    match DIR.get() {
        Some(d) => d.clone(),
        None => init(None),
    }
}

/// Append one line (a trailing newline is added) to `file` in the log directory, rotating at the
/// cap; on failure fall back to the executable's directory; on failure there, give up silently.
/// Best-effort by design — logging must never take the app down.
pub fn append_line(file: &str, line: &str) {
    let primary = dir();
    if append_to(&primary.join(file), line) {
        return;
    }
    let fallback = exe_dir();
    if fallback != primary {
        let _ = append_to(&fallback.join(file), line);
    }
}

/// Append one BENCH line to `DirWatch.log`, build-ID-stamped with a millisecond timestamp — the
/// same `TAG BUILD_ID [ms] msg` shape `gui::write_log_line` uses, so the `runtests` collector and a
/// gate `grep` catch a stale log by its build ID (DECISIONS R161, the search-scan bench). Kept here,
/// in the module that owns the file, rather than reaching into `gui`'s private writer.
pub fn append_bench_line(msg: &str) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    append_line(
        LOG_FILE,
        &format!(
            "BENCH {} [{ms}ms] {msg}",
            dirwatch_core::build_info::BUILD_ID
        ),
    );
}

/// Append `line` to `path`, rotating first if the append would cross [`LOG_CAP_BYTES`]. Returns
/// whether the write succeeded. Exposed for tests via `append_to_capped`.
fn append_to(path: &Path, line: &str) -> bool {
    append_to_capped(path, line, LOG_CAP_BYTES)
}

/// [`append_to`] with an explicit cap (tests use a tiny one).
pub fn append_to_capped(path: &Path, line: &str, cap: u64) -> bool {
    use std::io::Write;
    rotate_if_needed(path, line.len() as u64 + 1, cap);
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        Ok(mut f) => writeln!(f, "{line}").is_ok(),
        Err(_) => false,
    }
}

/// One-generation rotation: if `path` holds `cap` or more bytes once `adding` more are written,
/// rename it to `<file name>.1` (replacing any previous `.1`). A missing file is simply not rotated.
fn rotate_if_needed(path: &Path, adding: u64, cap: u64) {
    let len = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        Err(_) => return,
    };
    if len.saturating_add(adding) <= cap {
        return;
    }
    let rotated = rotated_name(path);
    let _ = std::fs::remove_file(&rotated); // Windows `rename` will not overwrite
    let _ = std::fs::rename(path, &rotated);
}

/// `DirWatch.log` -> `DirWatch.log.1` (the whole file name plus `.1`, so the base name stays
/// recognisable in a directory listing).
pub fn rotated_name(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| OsString::from(LOG_FILE));
    name.push(".1");
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| m.get(k).map(OsString::from)
    }

    #[test]
    fn override_wins_on_every_os() {
        let env = env_of(&[
            ("LOCALAPPDATA", "C:\\Users\\x\\AppData\\Local"),
            ("HOME", "/h"),
        ]);
        for os in [Os::Windows, Os::Mac, Os::Other] {
            assert_eq!(
                resolve(os, Some(Path::new("/custom/logs")), &env, Path::new("/exe")),
                PathBuf::from("/custom/logs")
            );
        }
    }

    #[test]
    fn windows_uses_localappdata_then_userprofile_then_exe_dir() {
        let exe = Path::new("C:\\Tools");
        let env = env_of(&[("LOCALAPPDATA", "C:\\Users\\x\\AppData\\Local")]);
        assert_eq!(
            resolve(Os::Windows, None, &env, exe),
            Path::new("C:\\Users\\x\\AppData\\Local").join("DirWatch")
        );
        let env = env_of(&[("USERPROFILE", "C:\\Users\\x")]);
        assert_eq!(
            resolve(Os::Windows, None, &env, exe),
            Path::new("C:\\Users\\x")
                .join("AppData")
                .join("Local")
                .join("DirWatch")
        );
        let env = env_of(&[]);
        assert_eq!(resolve(Os::Windows, None, &env, exe), exe.to_path_buf());
    }

    #[test]
    fn macos_uses_library_logs() {
        let env = env_of(&[("HOME", "/Users/mike")]);
        assert_eq!(
            resolve(
                Os::Mac,
                None,
                &env,
                Path::new("/Applications/DirWatch.app/Contents/MacOS")
            ),
            PathBuf::from("/Users/mike/Library/Logs/DirWatch")
        );
    }

    #[test]
    fn linux_uses_xdg_state_home_then_dot_local_state_and_ignores_a_relative_xdg() {
        let exe = Path::new("/tmp/.mount_dw/usr/bin");
        let env = env_of(&[
            ("XDG_STATE_HOME", "/var/state/mike"),
            ("HOME", "/home/mike"),
        ]);
        assert_eq!(
            resolve(Os::Other, None, &env, exe),
            PathBuf::from("/var/state/mike/dirwatch")
        );
        let env = env_of(&[("HOME", "/home/mike")]);
        assert_eq!(
            resolve(Os::Other, None, &env, exe),
            PathBuf::from("/home/mike/.local/state/dirwatch")
        );
        // A relative XDG_STATE_HOME is invalid per the spec: ignored, not joined.
        let env = env_of(&[("XDG_STATE_HOME", "state"), ("HOME", "/home/mike")]);
        assert_eq!(
            resolve(Os::Other, None, &env, exe),
            PathBuf::from("/home/mike/.local/state/dirwatch")
        );
        let env = env_of(&[]);
        assert_eq!(resolve(Os::Other, None, &env, exe), exe.to_path_buf());
    }

    #[test]
    fn empty_env_values_are_treated_as_unset() {
        let env = env_of(&[("LOCALAPPDATA", ""), ("USERPROFILE", "C:\\Users\\x")]);
        assert_eq!(
            resolve(Os::Windows, None, &env, Path::new("C:\\T")),
            Path::new("C:\\Users\\x")
                .join("AppData")
                .join("Local")
                .join("DirWatch")
        );
    }

    #[test]
    fn append_rotates_once_at_the_cap_and_keeps_writing() {
        let dir = std::env::temp_dir().join(format!("dwlog_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(LOG_FILE);
        let cap = 64;
        // 10 lines of ~20 bytes: rotation must happen, the live file must stay under the cap, and
        // exactly one `.1` generation exists (the older one is replaced, never a `.2`).
        for i in 0..10 {
            assert!(append_to_capped(
                &p,
                &format!("line {i:02} xxxxxxxxxxxx"),
                cap
            ));
        }
        let live = std::fs::metadata(&p).unwrap().len();
        assert!(
            live <= cap,
            "live file {live} bytes must stay within the cap"
        );
        let rotated = rotated_name(&p);
        assert_eq!(rotated.file_name().unwrap(), "DirWatch.log.1");
        assert!(rotated.exists(), "one rotated generation");
        assert!(
            !dir.join("DirWatch.log.2").exists(),
            "never a second generation"
        );
        let all =
            std::fs::read_to_string(&p).unwrap() + &std::fs::read_to_string(&rotated).unwrap();
        assert!(all.contains("line 09"), "the newest line is on disk");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_to_an_unwritable_directory_reports_failure_not_panic() {
        let p = Path::new("/nonexistent-dirwatch-dir/sub/DirWatch.log");
        assert!(!append_to_capped(p, "x", LOG_CAP_BYTES));
    }
}
