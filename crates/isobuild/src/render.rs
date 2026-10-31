//! Human-readable rendering of a [`Report`], for people at a terminal.
//! Agents should read the JSON instead.

use std::fmt::Write;

use isobuild_report::{Level, Report, Severity};

pub fn human(report: &Report) -> String {
    let mut out = String::new();
    for finding in &report.policy {
        let severity = match finding.severity {
            Severity::Deny => "denied",
            Severity::Warn => "warning",
        };
        let file = finding
            .file
            .as_deref()
            .map(|f| format!(" ({f})"))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "policy {severity} [{}]{file}: {}",
            finding.rule, finding.message
        );
    }

    for diagnostic in &report.diagnostics {
        match &diagnostic.rendered {
            Some(rendered) => {
                let _ = writeln!(out, "{rendered}\n");
            }
            None => {
                let level = match diagnostic.level {
                    Level::Error => "error",
                    Level::Warning => "warning",
                    Level::Note => "note",
                    Level::Help => "help",
                };
                let _ = writeln!(out, "{level}: {}\n", diagnostic.message);
            }
        }
    }
    if report.diagnostic_counts.omitted > 0 {
        let _ = writeln!(
            out,
            "… {} more diagnostics omitted\n",
            report.diagnostic_counts.omitted
        );
    }

    if let Some(tests) = &report.tests {
        for failure in &tests.failures {
            let location = failure
                .location
                .as_ref()
                .map(|l| format!(" at {}:{}:{}", l.file, l.line, l.column))
                .unwrap_or_default();
            let message = failure.message.as_deref().unwrap_or("failed");
            let _ = writeln!(
                out,
                "FAILED {}{location}\n  {}",
                failure.name,
                message.replace('\n', "\n  ")
            );
        }
        if tests.omitted_failures > 0 {
            let _ = writeln!(out, "… {} more failures omitted", tests.omitted_failures);
        }
    }

    let _ = writeln!(out, "{}: {}", report.status.as_str(), report.summary);
    if let Some(dir) = &report.log_dir {
        let _ = writeln!(out, "logs: {dir}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use isobuild_report::{Status, TestFailure, TestSummary};

    #[test]
    fn human_output_ends_with_status_and_lists_failures() {
        let mut report = Report::new("test", Status::TestFailed, "1 of 1 tests failed");
        report.tests = Some(TestSummary {
            failed: 1,
            failures: vec![TestFailure {
                name: "tests::it".into(),
                message: Some("left != right\n  left: 1".into()),
                location: None,
                output: String::new(),
            }],
            ..Default::default()
        });
        let text = human(&report);
        assert!(text.contains("FAILED tests::it\n  left != right\n    left: 1"));
        assert!(
            text.trim_end()
                .ends_with("test_failed: 1 of 1 tests failed")
        );
    }
}
