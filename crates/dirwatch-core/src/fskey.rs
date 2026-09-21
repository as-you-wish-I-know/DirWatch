//! Filesystem identity key for a path — the ONE place case-folding is decided (DECISIONS R97, review
//! A1).
//!
//! The .NET original ran on Windows only, so it keyed every dictionary by `OrdinalIgnoreCase` and
//! the Rust port copied that as `to_lowercase()` in three places (`watch`, `session`, the GUI's
//! path→window map). On Linux ext4 — a signed-off platform since R96 — `a.log` and `A.log` are two
//! DIFFERENT files, and folding them onto one key made the sweep report phantom activity on both
//! forever (reproduced by `case_variant_names_are_distinct_files_on_case_sensitive_fs`).
//!
//! Rule: fold case exactly where the filesystem does. Windows (NTFS/FAT) and macOS (default APFS/
//! HFS+) are case-insensitive → lowercase. Everything else (Linux) is case-sensitive → the path
//! verbatim. A case-SENSITIVE APFS volume on macOS is the one known mismatch; it is rare, documented
//! in README, and accepted (R97).
//!
//! Every keyed structure in the app MUST go through [`fs_key`] so the three cannot drift apart.

use std::path::Path;

/// True when this OS's default filesystems compare names case-insensitively.
pub const fn fs_is_case_insensitive() -> bool {
    cfg!(any(windows, target_os = "macos"))
}

/// The identity key for `path` (a `String`, lossy for non-UTF-8 names).
pub fn fs_key(path: &Path) -> String {
    fs_key_str(&path.to_string_lossy())
}

/// [`fs_key`] for a path already held as a string (the GUI keeps paths as `String`).
pub fn fs_key_str(path: &str) -> String {
    if fs_is_case_insensitive() {
        path.to_lowercase()
    } else {
        path.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_folds_case_only_where_the_filesystem_does() {
        let k = fs_key_str("Dir/MixedCase.LOG");
        if fs_is_case_insensitive() {
            assert_eq!(k, "dir/mixedcase.log");
        } else {
            assert_eq!(k, "Dir/MixedCase.LOG");
        }
    }

    #[test]
    fn path_and_str_forms_agree() {
        let p = Path::new("Some/Path/File.TXT");
        assert_eq!(fs_key(p), fs_key_str("Some/Path/File.TXT"));
    }
}
