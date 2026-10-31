//! Parsers against real cargo 1.91 output, captured from the sample projects
//! described in each test. Absolute paths were rewritten to `/work`, the
//! project root inside the sandbox.

use isobuild_diagnostics::{finalize, parse_cargo_messages, parse_cargo_stderr, parse_libtest};
use isobuild_report::{Applicability, DiagnosticSource, Level};

fn fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// `let s: u32 = "hello";`
#[test]
fn type_error() {
    let diags = parse_cargo_messages(&fixture("compile_err.stdout"), "/work");
    assert_eq!(diags.len(), 1, "failure-note must be dropped: {diags:#?}");
    let d = &diags[0];
    assert_eq!(d.level, Level::Error);
    assert_eq!(d.source, DiagnosticSource::Compiler);
    assert_eq!(d.code.as_deref(), Some("E0308"));
    assert_eq!(d.message, "mismatched types");
    assert_eq!(d.label.as_deref(), Some("expected `u32`, found `&str`"));
    let loc = d.location.as_ref().unwrap();
    assert_eq!(
        (loc.file.as_str(), loc.line, loc.column),
        ("src/lib.rs", 5, 18)
    );
    assert!(
        d.rendered
            .as_deref()
            .unwrap()
            .starts_with("error[E0308]: mismatched types")
    );
}

/// Two warnings and `x = 6;` on an immutable binding.
#[test]
fn borrow_error_with_suggestions() {
    let diags = parse_cargo_messages(&fixture("borrow_err.stdout"), "/work");
    let (diags, counts) = finalize(diags, 50);
    assert_eq!((counts.errors, counts.warnings, counts.omitted), (1, 2, 0));

    let error = &diags[0];
    assert_eq!(error.code.as_deref(), Some("E0384"));
    let loc = error.location.as_ref().unwrap();
    assert_eq!(
        (loc.line, loc.column),
        (4, 5),
        "primary span, not the first span"
    );
    let fix = &error.suggestions[0];
    assert_eq!(fix.message, "consider making this binding mutable");
    assert_eq!(fix.replacement, "mut ");
    assert_eq!(fix.applicability, Applicability::MachineApplicable);
    assert_eq!((fix.location.line, fix.location.column), (3, 9));

    let unused = diags
        .iter()
        .find(|d| d.code.as_deref() == Some("unused_variables"))
        .unwrap();
    assert_eq!(unused.level, Level::Warning);
    assert_eq!(unused.suggestions[0].replacement, "_unused");
    assert_eq!(
        unused.suggestions[0].applicability,
        Applicability::MaybeIncorrect
    );
    assert!(
        unused
            .notes
            .iter()
            .any(|n| n.starts_with("note: `#[warn(unused_variables)]`"))
    );
}

/// build.rs prints `cargo:warning=about to fail` and exits with 1.
#[test]
fn build_script_failure() {
    assert!(parse_cargo_messages(&fixture("buildrs_fail.stdout"), "/work").is_empty());

    let stderr = fixture("buildrs_fail.stderr");
    let errors = parse_cargo_stderr(&stderr, "/work", false);
    assert_eq!(errors.len(), 1);
    let e = &errors[0];
    assert_eq!(e.source, DiagnosticSource::Cargo);
    assert_eq!(
        e.message,
        "failed to run custom build command for `buildrs_fail v0.1.0 (/work)`"
    );
    let rendered = e.rendered.as_deref().unwrap();
    assert!(
        rendered.contains("could not find libfoo"),
        "build script stderr kept: {rendered}"
    );
    assert!(
        !rendered.contains("/work/target"),
        "paths made relative: {rendered}"
    );

    let with_warnings = parse_cargo_stderr(&stderr, "/work", true);
    assert_eq!(with_warnings.len(), 2);
    assert_eq!(with_warnings[0].level, Level::Warning);
    assert_eq!(
        with_warnings[0].message,
        "buildrs_fail@0.1.0: about to fail"
    );
}

/// Unit tests, an integration test and a doctest, with several failure kinds.
#[test]
fn test_run_with_failures() {
    let summary = parse_libtest(&fixture("tests_run.stdout"), "/work", 50);
    assert!(summary.complete);
    assert_eq!((summary.passed, summary.failed, summary.ignored), (2, 5, 1));

    let names: Vec<_> = summary.failures.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "tests::should_panic_but_doesnt",
            "tests::fails_eq",
            "tests::panics_msg",
            "integ_fail",
            "src/lib.rs - add (line 3)",
        ]
    );

    let by_name = |name: &str| summary.failures.iter().find(|f| f.name == name).unwrap();

    let eq = by_name("tests::fails_eq");
    assert_eq!(
        eq.message.as_deref(),
        Some("assertion `left == right` failed\n  left: 3\n right: 4")
    );
    let loc = eq.location.as_ref().unwrap();
    assert_eq!(
        (loc.file.as_str(), loc.line, loc.column),
        ("src/lib.rs", 12, 29)
    );

    let msg = by_name("tests::panics_msg");
    assert_eq!(msg.message.as_deref(), Some("boom: 42"));
    assert!(msg.output.contains("some captured output"));

    let should_panic = by_name("tests::should_panic_but_doesnt");
    assert_eq!(
        should_panic.message.as_deref(),
        Some("test did not panic as expected")
    );
    assert_eq!(should_panic.location.as_ref().unwrap().line, 15);

    let integ = by_name("integ_fail");
    assert_eq!(
        integ.message.as_deref(),
        Some("index out of bounds: the len is 0 but the index is 3")
    );
    assert_eq!(integ.location.as_ref().unwrap().file, "tests/integ.rs");

    let doctest = by_name("src/lib.rs - add (line 3)");
    assert!(
        doctest
            .message
            .as_deref()
            .unwrap()
            .starts_with("assertion `left == right` failed")
    );
    assert!(
        doctest.location.is_none(),
        "rustdoc temp file locations are dropped"
    );
}

/// The stderr of a failing `cargo test` only repeats what the stdout says.
#[test]
fn test_run_stderr_summaries_are_dropped() {
    assert!(parse_cargo_stderr(&fixture("tests_run.stderr"), "/work", false).is_empty());
}
