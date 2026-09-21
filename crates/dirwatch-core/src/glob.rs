//! Glob matcher — matches file NAMES (not full paths) against patterns like `*.log`, `*.txt`,
//! or `step*.log`. Ported from the .NET `GlobMatcher` (behavioral spec, build 2026-07-14.9).
//!
//! Case-insensitive to match Windows filesystem semantics. Extensions and named-with-wildcards
//! are the same mechanism. Std-only (no regex/glob crate — DECISIONS R7): the semantics are
//! `*` → match any run, `?` → match one char, everything else literal, anchored at both ends.

/// The default patterns when none are supplied.
pub const DEFAULT_PATTERNS: &[&str] = &["*.log", "*.txt"];

impl GlobMatcher {
    /// The default patterns joined with a space, for help text (matches the .NET
    /// `string.Join(" ", DefaultPatterns)` → `"*.log *.txt"`).
    pub const DEFAULT_PATTERNS_STR: &'static str = "*.log *.txt";
}

/// Matches a file name against one of a list of glob patterns.
#[derive(Debug, Clone)]
pub struct GlobMatcher {
    patterns: Vec<String>,
    /// Each pattern compiled to a lowercased token program (case-insensitive matching).
    programs: Vec<Vec<Token>>,
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    /// `*` — matches any run of characters (including empty).
    Star,
    /// `?` — matches exactly one character.
    Any,
    /// A literal character (already lowercased for case-insensitive compare).
    Lit(char),
}

impl GlobMatcher {
    /// Build a matcher. `None` or an all-empty list falls back to the defaults.
    pub fn new(patterns: Option<&[String]>) -> Self {
        let mut list: Vec<String> = match patterns {
            Some(ps) => ps
                .iter()
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect(),
            None => Vec::new(),
        };
        if list.is_empty() {
            list = DEFAULT_PATTERNS.iter().map(|s| s.to_string()).collect();
        }
        let programs = list.iter().map(|p| compile(p)).collect();
        GlobMatcher {
            patterns: list,
            programs,
        }
    }

    /// The (normalized) patterns this matcher uses.
    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }

    /// True if the file NAME (basename of `path`) matches any pattern.
    pub fn is_match(&self, path: &str) -> bool {
        let name = file_name(path);
        // Lowercase once for case-insensitive matching against lowercased programs.
        let name_lc: Vec<char> = name.chars().flat_map(|c| c.to_lowercase()).collect();
        self.programs.iter().any(|prog| matches(prog, &name_lc))
    }
}

/// Basename: the portion after the last `/` or `\` (Windows and POSIX separators both, so the
/// matcher behaves the same regardless of which separator a path uses — mirrors .NET
/// `Path.GetFileName` on Windows).
fn file_name(path: &str) -> &str {
    match path.rfind(['/', '\\']) {
        Some(i) => &path[i + 1..],
        None => path,
    }
}

/// Compile a glob into a token program, lowercasing literals for case-insensitive matching.
fn compile(glob: &str) -> Vec<Token> {
    glob.chars()
        .map(|c| match c {
            '*' => Token::Star,
            '?' => Token::Any,
            other => {
                // Lowercase the literal (may expand to >1 char in unicode; take the first — file
                // names in practice are simple, and this matches .NET's IgnoreCase behavior for
                // the common case).
                let lc = other.to_lowercase().next().unwrap_or(other);
                Token::Lit(lc)
            }
        })
        .collect()
}

/// Anchored glob match with `*` (any run) and `?` (one char) against a lowercased char slice.
/// Iterative backtracking on `*` — O(n*m) worst case, fine for file names.
fn matches(prog: &[Token], name: &[char]) -> bool {
    let (mut ti, mut ni) = (0usize, 0usize);
    // Backtrack points: the last `*` seen and the name index to resume from.
    let mut star_ti: Option<usize> = None;
    let mut star_ni = 0usize;

    while ni < name.len() {
        match prog.get(ti) {
            Some(Token::Lit(c)) if *c == name[ni] => {
                ti += 1;
                ni += 1;
            }
            Some(Token::Any) => {
                ti += 1;
                ni += 1;
            }
            Some(Token::Star) => {
                star_ti = Some(ti);
                star_ni = ni;
                ti += 1; // try to match `*` as empty first
            }
            _ => {
                // Mismatch (literal differs or program exhausted): backtrack to last `*` if any.
                match star_ti {
                    Some(sti) => {
                        ti = sti + 1;
                        star_ni += 1; // `*` absorbs one more char
                        ni = star_ni;
                    }
                    None => return false,
                }
            }
        }
    }
    // Consume any trailing `*` tokens.
    while let Some(Token::Star) = prog.get(ti) {
        ti += 1;
    }
    ti == prog.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pats: &[&str]) -> GlobMatcher {
        let owned: Vec<String> = pats.iter().map(|s| s.to_string()).collect();
        GlobMatcher::new(Some(&owned))
    }

    // Ported from GlobMatcherTests.cs (parity oracle).

    #[test]
    fn defaults_match_log_and_txt_only() {
        let g = GlobMatcher::new(None);
        assert!(g.is_match("server.log"));
        assert!(g.is_match("notes.txt"));
        assert!(!g.is_match("data.md"));
        assert!(!g.is_match("logfile"));
    }

    #[test]
    fn named_wildcard_pattern() {
        let g = m(&["step*.log"]);
        assert!(g.is_match("step1.log"));
        assert!(g.is_match("step.log"));
        assert!(!g.is_match("astep.log"));
        assert!(!g.is_match("step1.txt"));
    }

    #[test]
    fn case_insensitive() {
        let g = m(&["*.LOG"]);
        assert!(g.is_match("App.Log"));
    }

    #[test]
    fn matches_against_file_name_not_full_path() {
        let g = m(&["*.log"]);
        assert!(g.is_match("some/dir/x.log"));
        assert!(g.is_match("some\\dir\\x.log"));
        assert!(!g.is_match("log.dir/x.txt"));
    }

    #[test]
    fn question_mark_matches_single_char() {
        let g = m(&["log?.txt"]);
        assert!(g.is_match("log1.txt"));
        assert!(!g.is_match("log12.txt"));
    }

    #[test]
    fn empty_falls_back_to_defaults() {
        let g = GlobMatcher::new(Some(&[]));
        assert_eq!(g.patterns(), DEFAULT_PATTERNS);
    }

    #[test]
    fn whitespace_only_patterns_fall_back_to_defaults() {
        let g = m(&["  ", ""]);
        assert_eq!(g.patterns(), DEFAULT_PATTERNS);
    }
}
