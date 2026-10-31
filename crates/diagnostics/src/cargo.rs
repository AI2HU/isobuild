//! Cargo's JSON message stream and its plain-text stderr.

use isobuild_report::{Applicability, Diagnostic, DiagnosticSource, Level, Location, Suggestion};
use serde::Deserialize;

use crate::{relative, truncate};

const RENDERED_MAX: usize = 4000;

#[derive(Deserialize)]
struct CargoMessage {
    reason: String,
    message: Option<RustcDiagnostic>,
}

#[derive(Deserialize)]
struct RustcDiagnostic {
    message: String,
    code: Option<RustcCode>,
    level: String,
    #[serde(default)]
    spans: Vec<RustcSpan>,
    #[serde(default)]
    children: Vec<RustcDiagnostic>,
    rendered: Option<String>,
}

#[derive(Deserialize)]
struct RustcCode {
    code: String,
}

#[derive(Deserialize)]
struct RustcSpan {
    file_name: String,
    line_start: u32,
    line_end: u32,
    column_start: u32,
    column_end: u32,
    is_primary: bool,
    label: Option<String>,
    suggested_replacement: Option<String>,
    suggestion_applicability: Option<String>,
    expansion: Option<Box<RustcExpansion>>,
}

#[derive(Deserialize)]
struct RustcExpansion {
    span: RustcSpan,
}

/// Extract compiler diagnostics from the stdout of `cargo … --message-format=json`.
///
/// Lines that are not cargo messages (for example output printed by a program
/// run through `cargo run`) are skipped.
pub fn parse_cargo_messages(stdout: &str, root: &str) -> Vec<Diagnostic> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with('{'))
        .filter_map(|line| serde_json::from_str::<CargoMessage>(line).ok())
        .filter(|msg| msg.reason == "compiler-message")
        .filter_map(|msg| msg.message)
        .filter_map(|diag| convert(diag, root))
        .collect()
}

fn convert(diag: RustcDiagnostic, root: &str) -> Option<Diagnostic> {
    let level = match diag.level.as_str() {
        "error" | "error: internal compiler error" => Level::Error,
        "warning" => Level::Warning,
        "note" => Level::Note,
        "help" => Level::Help,
        // "failure-note" and friends only point at `rustc --explain`.
        _ => return None,
    };
    if diag.spans.is_empty() && is_rustc_summary(&diag.message) {
        return None;
    }

    let primary = diag
        .spans
        .iter()
        .find(|s| s.is_primary)
        .or(diag.spans.first());
    let location = primary.map(|span| span_location(outside_macro(span), root));
    let label = primary.and_then(|span| span.label.clone());

    let mut notes = Vec::new();
    let mut suggestions = Vec::new();
    for child in &diag.children {
        let child_suggestions: Vec<_> = child
            .spans
            .iter()
            .filter_map(|span| {
                Some(Suggestion {
                    message: child.message.clone(),
                    location: span_location(span, root),
                    replacement: span.suggested_replacement.clone()?,
                    applicability: applicability(span.suggestion_applicability.as_deref()),
                })
            })
            .collect();
        if child_suggestions.is_empty() {
            notes.push(format!("{}: {}", child.level, child.message));
        } else {
            suggestions.extend(child_suggestions);
        }
    }

    Some(Diagnostic {
        level,
        source: DiagnosticSource::Compiler,
        code: diag.code.map(|c| c.code),
        message: diag.message,
        location,
        label,
        notes,
        suggestions,
        rendered: diag.rendered.map(|r| truncate(r.trim_end(), RENDERED_MAX)),
    })
}

/// Lines such as "aborting due to 2 previous errors" or "3 warnings emitted".
fn is_rustc_summary(message: &str) -> bool {
    message.starts_with("aborting due to")
        || (message.starts_with(|c: char| c.is_ascii_digit()) && message.ends_with("emitted"))
}

/// Follow macro expansions back to the user's code. Spans inside a macro
/// definition have file names like `<::core::macros::panic macros>`.
fn outside_macro(mut span: &RustcSpan) -> &RustcSpan {
    while span.file_name.starts_with('<') {
        match &span.expansion {
            Some(expansion) => span = &expansion.span,
            None => break,
        }
    }
    span
}

fn span_location(span: &RustcSpan, root: &str) -> Location {
    Location {
        file: relative(&span.file_name, root),
        line: span.line_start,
        column: span.column_start,
        end_line: Some(span.line_end),
        end_column: Some(span.column_end),
    }
}

fn applicability(value: Option<&str>) -> Applicability {
    match value {
        Some("MachineApplicable") => Applicability::MachineApplicable,
        Some("MaybeIncorrect") => Applicability::MaybeIncorrect,
        Some("HasPlaceholders") => Applicability::HasPlaceholders,
        _ => Applicability::Unspecified,
    }
}

/// Extract cargo's own `error:` (and optionally `warning:`) messages from stderr.
///
/// These cover what the JSON stream does not: dependency resolution errors,
/// build script failures with their output, and `cargo:warning=` lines.
/// Summaries that repeat information found elsewhere ("could not compile",
/// "test failed, to rerun pass …") are dropped.
pub fn parse_cargo_stderr(stderr: &str, root: &str, include_warnings: bool) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let mut current: Option<(Level, Vec<&str>)> = None;

    for line in stderr.lines() {
        let header = if line.starts_with("error: ") {
            Some(Level::Error)
        } else if line.starts_with("warning: ") {
            Some(Level::Warning)
        } else {
            None
        };
        if let Some(level) = header {
            flush(current.take(), root, include_warnings, &mut out);
            current = Some((level, vec![line]));
        } else if is_status_line(line) {
            flush(current.take(), root, include_warnings, &mut out);
        } else if let Some((_, lines)) = current.as_mut() {
            lines.push(line);
        }
    }
    flush(current, root, include_warnings, &mut out);
    out
}

fn flush(
    block: Option<(Level, Vec<&str>)>,
    root: &str,
    include_warnings: bool,
    out: &mut Vec<Diagnostic>,
) {
    let Some((level, mut lines)) = block else {
        return;
    };
    if level == Level::Warning && !include_warnings {
        return;
    }
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    let message = lines[0].split_once(": ").map_or(lines[0], |(_, rest)| rest);
    if is_cargo_summary(message) {
        return;
    }
    let rendered = lines.join("\n").replace(&format!("{root}/"), "");
    out.push(Diagnostic {
        level,
        source: DiagnosticSource::Cargo,
        code: None,
        message: message.to_string(),
        location: None,
        label: None,
        notes: Vec::new(),
        suggestions: Vec::new(),
        rendered: Some(truncate(&rendered, RENDERED_MAX)),
    });
}

fn is_cargo_summary(message: &str) -> bool {
    message.starts_with("could not compile")
        || message.starts_with("test failed, to rerun pass")
        || message.starts_with("doctest failed, to rerun pass")
        || message.starts_with("build failed, waiting for other jobs")
        || message.ends_with("targets failed:")
        || (message.contains(") generated ") && message.contains("warning"))
}

/// Cargo status lines right-align a verb to column 12: "   Compiling foo".
fn is_status_line(line: &str) -> bool {
    let bytes = line.as_bytes();
    if bytes.len() < 14 || bytes[12] != b' ' {
        return false;
    }
    let verb = line[..12].trim_start();
    verb.starts_with(|c: char| c.is_ascii_uppercase())
        && verb.chars().all(|c| c.is_ascii_alphabetic() || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_lines_are_recognised() {
        assert!(is_status_line("   Compiling foo v0.1.0 (/work)"));
        assert!(is_status_line(
            "     Running unittests src/lib.rs (target/debug/deps/x)"
        ));
        assert!(is_status_line("   Doc-tests tests_mix"));
        assert!(is_status_line(
            "    Finished `test` profile [unoptimized] target(s) in 0.02s"
        ));
        assert!(!is_status_line("  Error compiling vendored C code"));
        assert!(!is_status_line("  --- stderr"));
        assert!(!is_status_line("Caused by:"));
    }

    #[test]
    fn macro_spans_resolve_to_user_code() {
        let line = r#"{"reason":"compiler-message","message":{"message":"boom","code":null,"level":"error","spans":[{"file_name":"<::core::macros::panic macros>","line_start":1,"line_end":1,"column_start":1,"column_end":2,"is_primary":true,"label":null,"suggested_replacement":null,"suggestion_applicability":null,"expansion":{"span":{"file_name":"/work/src/main.rs","line_start":7,"line_end":7,"column_start":5,"column_end":20,"is_primary":false,"label":null,"suggested_replacement":null,"suggestion_applicability":null,"expansion":null}}}],"children":[],"rendered":"error: boom\n"}}"#;
        let diags = parse_cargo_messages(line, "/work");
        let loc = diags[0].location.as_ref().unwrap();
        assert_eq!(
            (loc.file.as_str(), loc.line, loc.column),
            ("src/main.rs", 7, 5)
        );
    }

    #[test]
    fn non_json_and_other_reasons_are_ignored() {
        let input =
            "hello from the program\n{\"reason\":\"build-finished\",\"success\":true}\n{not json";
        assert!(parse_cargo_messages(input, "/work").is_empty());
    }

    #[test]
    fn rustc_summaries_are_dropped() {
        let line = r#"{"reason":"compiler-message","message":{"message":"aborting due to 2 previous errors","code":null,"level":"error","spans":[],"children":[],"rendered":"error: aborting due to 2 previous errors\n"}}"#;
        assert!(parse_cargo_messages(line, "/work").is_empty());
    }

    #[test]
    fn resolution_errors_are_reported() {
        let stderr = "    Updating crates.io index\nerror: no matching package named `serdee` found\nlocation searched: crates.io index\nrequired by package `demo v0.1.0 (/work)`\n";
        let diags = parse_cargo_stderr(stderr, "/work", false);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].message, "no matching package named `serdee` found");
        let rendered = diags[0].rendered.as_deref().unwrap();
        assert!(rendered.contains("required by package `demo v0.1.0 (/work)`"));
    }
}
