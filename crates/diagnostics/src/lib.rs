//! Parsers that turn cargo and libtest output into the structured types of
//! [`isobuild_report`].
//!
//! Inputs are the raw stdout/stderr of `cargo <cmd> --message-format=json` and
//! of `cargo test`. Paths under `root` (the project root as cargo saw it) are
//! made relative so agents can map them straight onto their working copy.

mod cargo;
mod libtest;

use isobuild_report::{Diagnostic, DiagnosticCounts, Level};

pub use cargo::{parse_cargo_messages, parse_cargo_stderr};
pub use libtest::parse_libtest;

/// Drop duplicate diagnostics, put errors before warnings, and keep at most `max`.
///
/// Cargo reports the same diagnostic once per target that compiles a file
/// (for example the lib and its unit tests), hence the deduplication.
pub fn finalize(diagnostics: Vec<Diagnostic>, max: usize) -> (Vec<Diagnostic>, DiagnosticCounts) {
    let mut unique: Vec<Diagnostic> = Vec::with_capacity(diagnostics.len());
    for diagnostic in diagnostics {
        let duplicate = unique.iter().any(|seen| {
            seen.level == diagnostic.level
                && seen.code == diagnostic.code
                && seen.message == diagnostic.message
                && seen.location == diagnostic.location
        });
        if !duplicate {
            unique.push(diagnostic);
        }
    }

    // Stable sort keeps compiler order within a level; the first error is
    // usually the one to fix.
    unique.sort_by_key(|d| match d.level {
        Level::Error => 0,
        Level::Warning => 1,
        Level::Note | Level::Help => 2,
    });

    let counts = DiagnosticCounts {
        errors: unique.iter().filter(|d| d.level == Level::Error).count(),
        warnings: unique.iter().filter(|d| d.level == Level::Warning).count(),
        omitted: unique.len().saturating_sub(max),
    };
    unique.truncate(max);
    (unique, counts)
}

/// Shorten `text` to at most `max` bytes on a character boundary, marking the cut.
pub(crate) fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… [truncated]", &text[..end])
}

/// Make `path` relative to `root` when it lies inside it.
pub(crate) fn relative(path: &str, root: &str) -> String {
    let root = root.trim_end_matches('/');
    if root.is_empty() {
        return path.to_string();
    }
    match path.strip_prefix(root) {
        Some(rest) if rest.starts_with('/') => rest.trim_start_matches('/').to_string(),
        _ => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use isobuild_report::{DiagnosticSource, Location};

    fn diag(level: Level, message: &str, line: u32) -> Diagnostic {
        Diagnostic {
            level,
            source: DiagnosticSource::Compiler,
            code: None,
            message: message.into(),
            location: Some(Location {
                file: "src/lib.rs".into(),
                line,
                column: 1,
                end_line: None,
                end_column: None,
            }),
            label: None,
            notes: vec![],
            suggestions: vec![],
            rendered: None,
        }
    }

    #[test]
    fn finalize_dedupes_sorts_and_caps() {
        let input = vec![
            diag(Level::Warning, "unused", 1),
            diag(Level::Error, "first error", 2),
            diag(Level::Warning, "unused", 1),
            diag(Level::Error, "second error", 3),
            diag(Level::Warning, "other", 4),
        ];
        let (kept, counts) = finalize(input, 3);
        let messages: Vec<_> = kept.iter().map(|d| d.message.as_str()).collect();
        assert_eq!(messages, ["first error", "second error", "unused"]);
        assert_eq!(
            counts,
            DiagnosticCounts {
                errors: 2,
                warnings: 2,
                omitted: 1
            }
        );
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        let text = "héllo wörld";
        let cut = truncate(text, 2);
        assert!(cut.starts_with('h'));
        assert!(cut.ends_with("[truncated]"));
        assert_eq!(truncate("short", 10), "short");
    }

    #[test]
    fn relative_only_strips_whole_path_components() {
        assert_eq!(relative("/work/src/lib.rs", "/work"), "src/lib.rs");
        assert_eq!(relative("/work/src/lib.rs", "/work/"), "src/lib.rs");
        assert_eq!(
            relative("/workspace/src/lib.rs", "/work"),
            "/workspace/src/lib.rs"
        );
        assert_eq!(relative("src/lib.rs", "/work"), "src/lib.rs");
    }
}
