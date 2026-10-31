//! The human-readable output of libtest, as printed by `cargo test`.
//!
//! libtest's JSON output is still unstable, so this parses the text format,
//! which has been stable for years. It tolerates output that stops midway
//! (timeouts, crashes) and reports what it saw.

use std::collections::HashMap;

use isobuild_report::{Location, TestFailure, TestSummary};

use crate::{relative, truncate};

const OUTPUT_MAX: usize = 2000;

#[derive(Default)]
struct Binary {
    passed: u32,
    failed: u32,
    ignored: u32,
    /// Index into the failure list, by test name.
    failures: HashMap<String, usize>,
}

enum Outcome {
    Ok,
    Failed,
    Ignored,
    Other,
}

/// Parse the stdout of `cargo test`, keeping at most `max_failures` failure details.
pub fn parse_libtest(stdout: &str, root: &str, max_failures: usize) -> TestSummary {
    let mut summary = TestSummary::default();
    let mut failures: Vec<TestFailure> = Vec::new();
    let mut binary: Option<Binary> = None;
    let mut block: Option<(String, Vec<&str>)> = None;
    let mut saw_binary = false;
    let mut all_finished = true;

    for line in stdout.lines() {
        if is_running_line(line) {
            close_block(block.take(), binary.as_ref(), &mut failures, root);
            if let Some(unfinished) = binary.take() {
                add_line_counts(&mut summary, &unfinished);
                all_finished = false;
            }
            binary = Some(Binary::default());
            saw_binary = true;
        } else if let Some(name) = block_header(line) {
            close_block(block.take(), binary.as_ref(), &mut failures, root);
            block = Some((name.to_string(), Vec::new()));
        } else if let Some(result) = line.strip_prefix("test result: ") {
            close_block(block.take(), binary.as_ref(), &mut failures, root);
            add_result_counts(&mut summary, result);
            binary = None;
        } else if line == "failures:" {
            close_block(block.take(), binary.as_ref(), &mut failures, root);
        } else if let Some((_, lines)) = block.as_mut() {
            lines.push(line);
        } else if let Some((name, outcome)) = test_line(line) {
            let current = binary.get_or_insert_with(Binary::default);
            match outcome {
                Outcome::Ok => current.passed += 1,
                Outcome::Ignored => current.ignored += 1,
                Outcome::Failed => {
                    current.failed += 1;
                    current.failures.insert(name.to_string(), failures.len());
                    failures.push(TestFailure {
                        name: name.to_string(),
                        message: None,
                        location: None,
                        output: String::new(),
                    });
                }
                Outcome::Other => {}
            }
        } else if let Some(name) = slow_test(line) {
            summary.slow.push(name.to_string());
        }
    }
    close_block(block, binary.as_ref(), &mut failures, root);
    if let Some(unfinished) = binary {
        add_line_counts(&mut summary, &unfinished);
        all_finished = false;
    }

    summary.complete = saw_binary && all_finished;
    summary.omitted_failures = failures.len().saturating_sub(max_failures);
    failures.truncate(max_failures);
    summary.failures = failures;
    summary
}

fn is_running_line(line: &str) -> bool {
    line.starts_with("running ") && (line.ends_with(" tests") || line.ends_with(" test"))
}

/// `---- tests::name stdout ----`
fn block_header(line: &str) -> Option<&str> {
    line.strip_prefix("---- ")?.strip_suffix(" stdout ----")
}

/// `test tests::name ... ok`, `test x - should panic ... FAILED`, `test y ... ignored, reason`
fn test_line(line: &str) -> Option<(&str, Outcome)> {
    let (name, outcome) = line.strip_prefix("test ")?.split_once(" ... ")?;
    let name = name.strip_suffix(" - should panic").unwrap_or(name);
    let outcome = match outcome.trim_end() {
        "ok" => Outcome::Ok,
        "FAILED" => Outcome::Failed,
        other if other.starts_with("ignored") => Outcome::Ignored,
        _ => Outcome::Other,
    };
    Some((name, outcome))
}

/// `test tests::name has been running for over 60 seconds`
fn slow_test(line: &str) -> Option<&str> {
    let (name, _) = line
        .strip_prefix("test ")?
        .split_once(" has been running for over ")?;
    Some(name)
}

fn add_line_counts(summary: &mut TestSummary, binary: &Binary) {
    summary.passed += binary.passed;
    summary.failed += binary.failed;
    summary.ignored += binary.ignored;
}

/// `FAILED. 1 passed; 3 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s`
fn add_result_counts(summary: &mut TestSummary, result: &str) {
    let counts = result.split_once(". ").map_or(result, |(_, rest)| rest);
    for part in counts.split(';') {
        let Some((number, label)) = part.trim().split_once(' ') else {
            continue;
        };
        let Ok(number) = number.parse::<u32>() else {
            continue;
        };
        match label {
            "passed" => summary.passed += number,
            "failed" => summary.failed += number,
            "ignored" => summary.ignored += number,
            "filtered out" => summary.filtered_out += number,
            _ => {}
        }
    }
}

fn close_block(
    block: Option<(String, Vec<&str>)>,
    binary: Option<&Binary>,
    failures: &mut Vec<TestFailure>,
    root: &str,
) {
    let Some((name, lines)) = block else {
        return;
    };
    let index = match binary.and_then(|b| b.failures.get(&name)) {
        Some(&index) => index,
        None => {
            failures.push(TestFailure {
                name: name.clone(),
                message: None,
                location: None,
                output: String::new(),
            });
            failures.len() - 1
        }
    };
    let (message, location) = failure_reason(&lines, root);
    let failure = &mut failures[index];
    failure.message = message;
    failure.location = location;
    failure.output = truncate(lines.join("\n").trim(), OUTPUT_MAX);
}

/// Find the panic message and location in a failure's captured output.
fn failure_reason(lines: &[&str], root: &str) -> (Option<String>, Option<Location>) {
    for (i, line) in lines.iter().enumerate() {
        let Some(rest) = line.strip_prefix("thread '") else {
            continue;
        };
        let Some((_, after)) = rest.split_once(" panicked at ") else {
            continue;
        };
        // Rust 1.73+: "panicked at src/lib.rs:12:29:" then the message on the following lines.
        if let Some(loc) = after.strip_suffix(':') {
            let message: Vec<&str> = lines[i + 1..]
                .iter()
                .take_while(|l| !l.starts_with("note: ") && !l.starts_with("stack backtrace:"))
                .copied()
                .collect();
            let message = message.join("\n").trim().to_string();
            return (
                (!message.is_empty()).then_some(message),
                parse_location(loc, root),
            );
        }
        // Older: "panicked at 'message', src/lib.rs:12:29"
        if let Some(quoted) = after.strip_prefix('\'')
            && let Some((message, loc)) = quoted.rsplit_once("', ")
        {
            return (Some(message.to_string()), parse_location(loc, root));
        }
    }

    for line in lines {
        if let Some(rest) = line.strip_prefix("note: test did not panic as expected") {
            let location = rest
                .strip_prefix(" at ")
                .and_then(|loc| parse_location(loc, root));
            return (Some("test did not panic as expected".to_string()), location);
        }
    }

    // A doctest that does not compile reports rustc errors instead of a panic.
    let error = lines.iter().find(|l| l.starts_with("error"));
    (error.map(|l| l.to_string()), None)
}

/// `src/lib.rs:12:29` → Location. Locations in rustdoc's temporary files are dropped.
fn parse_location(text: &str, root: &str) -> Option<Location> {
    let mut parts = text.trim().rsplitn(3, ':');
    let column = parts.next()?.parse().ok()?;
    let line = parts.next()?.parse().ok()?;
    let file = parts.next()?;
    if file.contains("rustdoctest") {
        return None;
    }
    Some(Location {
        file: relative(file, root),
        line,
        column,
        end_line: None,
        end_column: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_panic_format_is_understood() {
        let out = "running 1 test\ntest it ... FAILED\n\nfailures:\n\n---- it stdout ----\nthread 'it' panicked at 'boom', src/lib.rs:4:5\nnote: run with `RUST_BACKTRACE=1`\n\nfailures:\n    it\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";
        let summary = parse_libtest(out, "/work", 10);
        let failure = &summary.failures[0];
        assert_eq!(failure.message.as_deref(), Some("boom"));
        assert_eq!(failure.location.as_ref().unwrap().line, 4);
    }

    #[test]
    fn truncated_output_is_marked_incomplete_and_counts_lines() {
        let out = "running 3 tests\ntest a ... ok\ntest b ... ok\ntest c has been running for over 60 seconds\n";
        let summary = parse_libtest(out, "/work", 10);
        assert!(!summary.complete);
        assert_eq!(summary.passed, 2);
        assert_eq!(summary.slow, ["c"]);
    }

    #[test]
    fn empty_output_is_incomplete() {
        let summary = parse_libtest("", "/work", 10);
        assert!(!summary.complete);
        assert_eq!(summary, TestSummary::default());
    }

    #[test]
    fn failures_are_capped() {
        let mut out = String::from("running 5 tests\n");
        for i in 0..5 {
            out.push_str(&format!("test t{i} ... FAILED\n"));
        }
        out.push_str("\ntest result: FAILED. 0 passed; 5 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n");
        let summary = parse_libtest(&out, "/work", 2);
        assert_eq!(summary.failed, 5);
        assert_eq!(summary.failures.len(), 2);
        assert_eq!(summary.omitted_failures, 3);
        assert!(summary.complete);
    }
}
