//! The result format `isobuild` returns to coding agents.
//!
//! The format is versioned by [`SCHEMA_VERSION`]. Within a version, fields are
//! only ever added; renaming or removing a field bumps the version. Consumers
//! should ignore fields they do not know.

use serde::{Deserialize, Serialize};

/// Version of the JSON layout described by [`Report`].
pub const SCHEMA_VERSION: u32 = 2;

/// Outcome of a run, from the agent's point of view.
///
/// The variants separate "change your code" from "retry or report it":
/// `invalid_request`, `policy_denied`, `fetch_failed`, `build_failed` and
/// `test_failed` are caused by the submitted project; `infra_error` is not.
/// `timeout`, `oom` and `process_limit` are usually caused by the code but can
/// also mean the limits are too tight for the project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    InvalidRequest,
    PolicyDenied,
    FetchFailed,
    BuildFailed,
    TestFailed,
    Timeout,
    Oom,
    /// Too many processes at once, e.g. a fork bomb.
    ProcessLimit,
    InfraError,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::InvalidRequest => "invalid_request",
            Status::PolicyDenied => "policy_denied",
            Status::FetchFailed => "fetch_failed",
            Status::BuildFailed => "build_failed",
            Status::TestFailed => "test_failed",
            Status::Timeout => "timeout",
            Status::Oom => "oom",
            Status::ProcessLimit => "process_limit",
            Status::InfraError => "infra_error",
        }
    }
}

/// Steps of a run, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Host-side checks and staging. Runs no project code.
    Prepare,
    /// `cargo fetch` with network access. Runs no project code.
    Fetch,
    /// Compilation, including build scripts and proc macros. No network.
    Build,
    /// Running the test binaries. No network.
    Test,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub schema_version: u32,
    pub status: Status,
    /// Phase in which the run stopped, when it did not succeed.
    pub failed_phase: Option<Phase>,
    /// One line describing the outcome, e.g. "build failed: 2 errors".
    pub summary: String,
    /// The requested cargo command, e.g. "test".
    pub command: String,
    pub phases: Vec<PhaseOutcome>,
    /// Errors first, then warnings. Capped; see `diagnostic_counts.omitted`.
    pub diagnostics: Vec<Diagnostic>,
    pub diagnostic_counts: DiagnosticCounts,
    /// Present when tests ran, even partially.
    pub tests: Option<TestSummary>,
    pub policy: Vec<PolicyFinding>,
    pub environment: Option<Environment>,
    pub cache: Option<CacheInfo>,
    /// Host directory holding the full stdout/stderr of each phase.
    pub log_dir: Option<String>,
}

impl Report {
    pub fn new(command: impl Into<String>, status: Status, summary: impl Into<String>) -> Self {
        Report {
            schema_version: SCHEMA_VERSION,
            status,
            failed_phase: None,
            summary: summary.into(),
            command: command.into(),
            phases: Vec::new(),
            diagnostics: Vec::new(),
            diagnostic_counts: DiagnosticCounts::default(),
            tests: None,
            policy: Vec::new(),
            environment: None,
            cache: None,
            log_dir: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhaseOutcome {
    pub phase: Phase,
    /// Exit code of the phase's main process. `None` if it never exited normally.
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub timed_out: bool,
    pub oom_killed: bool,
    /// Killed for running more processes than the limit allows.
    pub process_limit_hit: bool,
    /// True when stdout or stderr exceeded the capture limit.
    pub output_truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DiagnosticCounts {
    pub errors: usize,
    pub warnings: usize,
    /// Diagnostics dropped from `diagnostics` because of the cap.
    pub omitted: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Error,
    Warning,
    Note,
    Help,
}

/// Who produced a diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSource {
    /// rustc or clippy, from cargo's JSON messages.
    Compiler,
    /// Cargo itself: resolution errors, build script failures, `cargo:warning`.
    Cargo,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub level: Level,
    pub source: DiagnosticSource,
    /// Lint or error code, e.g. "E0308" or "unused_variables".
    pub code: Option<String>,
    pub message: String,
    /// Primary location. Paths inside the project are relative to its root.
    pub location: Option<Location>,
    /// Label attached to the primary location, e.g. "expected `u32`, found `&str`".
    pub label: Option<String>,
    /// Notes and help messages without a code change, prefixed with their level.
    pub notes: Vec<String>,
    pub suggestions: Vec<Suggestion>,
    /// The compiler's own rendering, as a human would see it. Capped in length.
    pub rendered: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub file: String,
    pub line: u32,
    pub column: u32,
    pub end_line: Option<u32>,
    pub end_column: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Suggestion {
    pub message: String,
    pub location: Location,
    /// Text that replaces the span between `location` and its end.
    pub replacement: String,
    pub applicability: Applicability,
}

/// How safe it is to apply a suggestion without review, as reported by rustc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Applicability {
    MachineApplicable,
    MaybeIncorrect,
    HasPlaceholders,
    Unspecified,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TestSummary {
    pub passed: u32,
    pub failed: u32,
    pub ignored: u32,
    pub filtered_out: u32,
    /// Capped; see `omitted_failures`.
    pub failures: Vec<TestFailure>,
    pub omitted_failures: usize,
    /// Tests libtest reported as running for over 60 seconds. When a run times
    /// out, these are the likely culprits.
    pub slow: Vec<String>,
    /// False when the output ended before every test binary reported a result,
    /// for example after a timeout or a crash.
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestFailure {
    pub name: String,
    /// Panic message, or libtest's reason such as "test did not panic as expected".
    pub message: Option<String>,
    pub location: Option<Location>,
    /// Captured output of the test. Capped in length.
    pub output: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// The run is refused until the project changes.
    Deny,
    /// The run goes ahead; the finding explains a difference from a normal build.
    Warn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyFinding {
    pub severity: Severity,
    /// Stable identifier, e.g. "cargo-config.rustc-wrapper".
    pub rule: String,
    /// Path relative to the project root.
    pub file: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Environment {
    /// The sandbox that ran the code: `seatbelt` (macOS) or `bubblewrap` (Linux).
    pub backend: String,
    /// The Rust toolchain, as `rustc --version` prints it.
    pub toolchain: String,
    /// Hash identifying the toolchain exactly, including its install location.
    pub toolchain_id: String,
    pub limits: Limits,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Limits {
    /// Combined resident memory of every process in the sandbox.
    pub memory_mb: u64,
    /// Parallelism: cargo runs this many jobs at once, rounded up.
    pub cpus: f64,
    /// Processes running at once in the sandbox.
    pub pids: u32,
    pub fetch_timeout_secs: u64,
    pub build_timeout_secs: u64,
    pub test_timeout_secs: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            memory_mb: 4096,
            cpus: 2.0,
            pids: 1024,
            fetch_timeout_secs: 300,
            build_timeout_secs: 900,
            test_timeout_secs: 600,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheInfo {
    /// Key of the build cache this run used. Runs with the same key share compiled artifacts.
    pub key: String,
    /// True when the build cache already existed before this run.
    pub warm: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_serializes_as_snake_case_and_matches_as_str() {
        for status in [
            Status::Ok,
            Status::InvalidRequest,
            Status::PolicyDenied,
            Status::FetchFailed,
            Status::BuildFailed,
            Status::TestFailed,
            Status::Timeout,
            Status::Oom,
            Status::ProcessLimit,
            Status::InfraError,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            assert_eq!(json, format!("\"{}\"", status.as_str()));
        }
    }

    #[test]
    fn report_round_trips_through_json() {
        let mut report = Report::new("test", Status::TestFailed, "1 of 2 tests failed");
        report.failed_phase = Some(Phase::Test);
        report.tests = Some(TestSummary {
            passed: 1,
            failed: 1,
            failures: vec![TestFailure {
                name: "tests::it".into(),
                message: Some("boom".into()),
                location: Some(Location {
                    file: "src/lib.rs".into(),
                    line: 3,
                    column: 5,
                    end_line: None,
                    end_column: None,
                }),
                output: String::new(),
            }],
            complete: true,
            ..Default::default()
        });
        let json = serde_json::to_string(&report).unwrap();
        let back: Report = serde_json::from_str(&json).unwrap();
        assert_eq!(back, report);
        assert_eq!(back.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn report_json_uses_documented_field_names() {
        let report = Report::new("check", Status::Ok, "ok");
        let value: serde_json::Value = serde_json::to_value(&report).unwrap();
        for field in [
            "schema_version",
            "status",
            "failed_phase",
            "summary",
            "command",
            "phases",
            "diagnostics",
            "diagnostic_counts",
            "tests",
            "policy",
            "environment",
            "cache",
            "log_dir",
        ] {
            assert!(value.get(field).is_some(), "missing field {field}");
        }
    }
}
