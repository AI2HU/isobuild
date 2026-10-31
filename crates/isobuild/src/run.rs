//! The run itself: prepare, fetch, build, test, and turn it all into a [`Report`].

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use isobuild_diagnostics::{finalize, parse_cargo_messages, parse_cargo_stderr, parse_libtest};
use isobuild_policy::check_project;
use isobuild_report::{
    CacheInfo, Diagnostic, DiagnosticSource, Environment, Level, Phase, PhaseOutcome, Report,
    Status,
};

use crate::backend::{Backend, BackendError, ContainerOutcome, ContainerSpec, Mount};
use crate::cache::{CacheLayout, build_cache_key, default_scope};
use crate::request::{CargoCommand, RunRequest};
use crate::stage::{StageError, copy_project};

/// Where the project lives inside the sandbox. Diagnostics are relative to it.
pub const WORKDIR: &str = "/work";
const CARGO_HOME: &str = "/cargo-home";
const TARGET_DIR: &str = "/target";

const RUN_LOG_RETENTION: Duration = Duration::from_secs(3 * 24 * 3600);
const BUILD_CACHE_RETENTION: Duration = Duration::from_secs(14 * 24 * 3600);

/// Why a run stopped early.
struct Stop {
    status: Status,
    phase: Phase,
    /// Overrides the generated summary when set.
    summary: Option<String>,
}

impl Stop {
    fn new(status: Status, phase: Phase) -> Self {
        Stop {
            status,
            phase,
            summary: None,
        }
    }

    fn because(status: Status, phase: Phase, summary: impl Into<String>) -> Self {
        Stop {
            status,
            phase,
            summary: Some(summary.into()),
        }
    }
}

fn infra(phase: Phase) -> impl Fn(BackendError) -> Stop {
    move |err| Stop::because(Status::InfraError, phase, err.to_string())
}

/// Run `req` on `backend`. Never panics on bad input and never returns an
/// error: every failure is described by the report.
pub fn run(req: &RunRequest, backend: &dyn Backend) -> Report {
    let mut report = Report::new(req.command.as_str(), Status::Ok, String::new());
    let mut diagnostics = Vec::new();

    let stop = execute(req, backend, &mut report, &mut diagnostics).err();
    let (diagnostics, counts) = finalize(diagnostics, req.max_diagnostics);
    report.diagnostics = diagnostics;
    report.diagnostic_counts = counts;

    match stop {
        None => report.summary = success_summary(&report),
        Some(stop) => {
            report.status = stop.status;
            report.failed_phase = Some(stop.phase);
            report.summary = stop
                .summary
                .unwrap_or_else(|| failure_summary(&report, req, stop.phase));
        }
    }
    report
}

fn execute(
    req: &RunRequest,
    backend: &dyn Backend,
    report: &mut Report,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<(), Stop> {
    // Prepare: nothing from the project runs here.
    req.validate()
        .map_err(|msg| Stop::because(Status::InvalidRequest, Phase::Prepare, msg))?;
    let policy = check_project(&req.project).map_err(|err| {
        Stop::because(
            Status::InfraError,
            Phase::Prepare,
            format!("could not read the project: {err}"),
        )
    })?;
    report.policy = policy.findings.clone();
    if policy.is_denied() {
        return Err(Stop::because(
            Status::PolicyDenied,
            Phase::Prepare,
            "the project uses settings the sandbox refuses; see `policy`",
        ));
    }

    backend.preflight().map_err(infra(Phase::Prepare))?;
    let image_id = backend
        .ensure_image(&req.image)
        .map_err(infra(Phase::Prepare))?;
    report.environment = Some(Environment {
        image: req.image.clone(),
        image_id: image_id.clone(),
        limits: req.limits.clone(),
    });

    let layout = CacheLayout::new(&req.cache_dir);
    let io_failure = |what: &'static str| {
        move |err: std::io::Error| {
            Stop::because(Status::InfraError, Phase::Prepare, format!("{what}: {err}"))
        }
    };
    layout
        .ensure()
        .map_err(io_failure("could not create the cache directory"))?;
    for key in layout
        .prune(RUN_LOG_RETENTION, BUILD_CACHE_RETENTION)
        .targets
    {
        backend.remove_volume(&target_volume(&key));
    }
    let (run_id, run_dir) = layout
        .new_run_dir()
        .map_err(io_failure("could not create the run directory"))?;
    report.log_dir = Some(run_dir.display().to_string());

    let work = run_dir.join("work");
    let mut cleanup = RunCleanup {
        backend,
        run_dir: run_dir.clone(),
        keep_staging: req.keep_staging,
        volume: None,
    };
    copy_project(&req.project, &work, &policy.excluded, req.max_project_bytes).map_err(|err| {
        match err {
            StageError::TooLarge { .. } => {
                Stop::because(Status::InvalidRequest, Phase::Prepare, err.to_string())
            }
            StageError::Io(err) => Stop::because(
                Status::InfraError,
                Phase::Prepare,
                format!("could not copy the project: {err}"),
            ),
        }
    })?;

    // Build output goes to a volume: it is written by project code, and on
    // macOS it is far faster (and safer) than a host directory.
    let user = owner_of(layout.root());
    let target = if req.build_cache {
        let scope = req
            .cache_scope
            .clone()
            .unwrap_or_else(|| default_scope(&req.project));
        let key = build_cache_key(&scope, &image_id);
        let volume = target_volume(&key);
        let warm = backend
            .ensure_volume(&volume, user.as_deref(), &image_id)
            .map_err(infra(Phase::Prepare))?;
        fs::create_dir_all(layout.target(&key))
            .map_err(io_failure("could not create the build cache"))?;
        layout
            .touch_target(&key)
            .map_err(io_failure("could not update the build cache"))?;
        report.cache = Some(CacheInfo { key, warm });
        volume
    } else {
        let volume = format!("isobuild-run-{run_id}");
        cleanup.volume = Some(volume.clone());
        backend
            .ensure_volume(&volume, user.as_deref(), &image_id)
            .map_err(infra(Phase::Prepare))?;
        volume
    };

    let phases = Phases {
        req,
        backend,
        run_id,
        run_dir,
        image_id,
        user,
        work,
    };

    // Fetch: network on, cargo only.
    let fetched = !req.offline;
    if fetched {
        let _lock = layout.lock_fetch().map_err(|err| {
            Stop::because(
                Status::InfraError,
                Phase::Fetch,
                format!("could not lock the dependency cache: {err}"),
            )
        })?;
        let mut command = vec!["cargo".to_string(), "fetch".to_string()];
        if req.cargo.locked {
            command.push("--locked".into());
        }
        let mounts = vec![
            phases.mount_work(),
            Mount::host(layout.cargo_home(), CARGO_HOME, false),
        ];
        let spec = phases.spec(Phase::Fetch, command, mounts, true, Vec::new());
        let outcome = phases.run(Phase::Fetch, &spec, report)?;
        check_limits(&outcome, Phase::Fetch)?;
        if outcome.exit_code != Some(0) {
            let stderr = read_log(&spec.stderr_path);
            let errors = parse_cargo_stderr(&stderr, WORKDIR, false);
            let network = looks_like_network_error(&stderr);
            diagnostics.extend(errors);
            if network {
                return Err(Stop::because(
                    Status::InfraError,
                    Phase::Fetch,
                    "could not download dependencies because of a network error; if a dependency points at a private or unknown host, fix it, otherwise retry",
                ));
            }
            ensure_error(diagnostics, "cargo fetch", &outcome);
            return Err(Stop::new(Status::FetchFailed, Phase::Fetch));
        }
    }

    // Build: network off, project code runs (build scripts, proc macros).
    let build_mounts = vec![
        phases.mount_work(),
        Mount::host(layout.registry(), format!("{CARGO_HOME}/registry"), true),
        Mount::host(layout.git(), format!("{CARGO_HOME}/git"), true),
        Mount::volume(target, TARGET_DIR),
    ];
    let spec = phases.spec(
        Phase::Build,
        cargo_command(req, Phase::Build, fetched),
        build_mounts.clone(),
        false,
        sandboxed_env(req),
    );
    let outcome = phases.run(Phase::Build, &spec, report)?;
    let stdout = read_log(&spec.stdout_path);
    let stderr = read_log(&spec.stderr_path);
    diagnostics.extend(parse_cargo_messages(&stdout, WORKDIR));
    check_limits(&outcome, Phase::Build)?;
    if outcome.exit_code != Some(0) {
        if stderr.contains("error: no such command") {
            return Err(Stop::because(
                Status::InfraError,
                Phase::Build,
                format!(
                    "the image `{}` does not provide `cargo {}`",
                    req.image,
                    req.command.as_str()
                ),
            ));
        }
        diagnostics.extend(parse_cargo_stderr(&stderr, WORKDIR, true));
        ensure_error(diagnostics, "the build", &outcome);
        return Err(Stop::new(Status::BuildFailed, Phase::Build));
    }
    // Keep cargo's own warnings, such as `cargo:warning=` from build scripts.
    diagnostics.extend(parse_cargo_stderr(&stderr, WORKDIR, true));
    if req.command != CargoCommand::Test {
        return Ok(());
    }

    // Test: network off.
    let spec = phases.spec(
        Phase::Test,
        cargo_command(req, Phase::Test, fetched),
        build_mounts,
        false,
        sandboxed_env(req),
    );
    let outcome = phases.run(Phase::Test, &spec, report)?;
    let stdout = read_log(&spec.stdout_path);
    let tests = parse_libtest(&stdout, WORKDIR, req.max_test_failures);
    let failed = tests.failed;
    report.tests = Some(tests);
    check_limits(&outcome, Phase::Test)?;
    if outcome.exit_code != Some(0) {
        if failed == 0 {
            // A crashed test binary or a doctest that failed to build.
            diagnostics.extend(parse_cargo_stderr(
                &read_log(&spec.stderr_path),
                WORKDIR,
                false,
            ));
            ensure_error(diagnostics, "the tests", &outcome);
        }
        return Err(Stop::new(Status::TestFailed, Phase::Test));
    }
    Ok(())
}

/// Builds container specs that share the run's settings.
struct Phases<'a> {
    req: &'a RunRequest,
    backend: &'a dyn Backend,
    run_id: String,
    run_dir: PathBuf,
    image_id: String,
    user: Option<String>,
    work: PathBuf,
}

impl Phases<'_> {
    fn mount_work(&self) -> Mount {
        Mount::host(self.work.clone(), WORKDIR, false)
    }

    fn spec(
        &self,
        phase: Phase,
        command: Vec<String>,
        mounts: Vec<Mount>,
        network: bool,
        extra_env: Vec<(String, String)>,
    ) -> ContainerSpec {
        let name = phase_name(phase);
        let limits = &self.req.limits;
        let timeout = match phase {
            Phase::Prepare | Phase::Fetch => limits.fetch_timeout_secs,
            Phase::Build => limits.build_timeout_secs,
            Phase::Test => limits.test_timeout_secs,
        };
        let mut env = base_env();
        env.extend(extra_env);
        let mut tmpfs = vec![("/tmp".to_string(), 1024)];
        if !network {
            // Build and test only see registry/ and git/ of the shared cargo home;
            // the rest (locks, cache database) lives in memory.
            tmpfs.push((CARGO_HOME.to_string(), 64));
        }
        ContainerSpec {
            name: format!("isobuild-{}-{name}", self.run_id),
            image: self.image_id.clone(),
            command,
            env,
            mounts,
            tmpfs,
            network,
            workdir: WORKDIR.into(),
            user: self.user.clone(),
            memory_mb: limits.memory_mb,
            cpus: limits.cpus,
            pids: limits.pids,
            timeout: Duration::from_secs(timeout),
            stdout_path: self.run_dir.join(format!("{name}.stdout.log")),
            stderr_path: self.run_dir.join(format!("{name}.stderr.log")),
            output_cap: self.req.max_output_bytes,
        }
    }

    /// Run one phase and record its outcome in the report.
    fn run(
        &self,
        phase: Phase,
        spec: &ContainerSpec,
        report: &mut Report,
    ) -> Result<ContainerOutcome, Stop> {
        let outcome = self.backend.run(spec).map_err(infra(phase))?;
        report.phases.push(PhaseOutcome {
            phase,
            exit_code: outcome.exit_code,
            duration_ms: outcome.duration.as_millis() as u64,
            timed_out: outcome.timed_out,
            oom_killed: outcome.oom_killed,
            output_truncated: outcome.output_truncated,
        });
        Ok(outcome)
    }
}

fn phase_name(phase: Phase) -> &'static str {
    match phase {
        Phase::Prepare => "prepare",
        Phase::Fetch => "fetch",
        Phase::Build => "build",
        Phase::Test => "test",
    }
}

fn base_env() -> Vec<(String, String)> {
    [
        ("CARGO_HOME", CARGO_HOME),
        ("CARGO_TARGET_DIR", TARGET_DIR),
        ("CARGO_TERM_COLOR", "never"),
        ("CARGO_TERM_PROGRESS_WHEN", "never"),
        ("HOME", "/tmp"),
        ("TMPDIR", "/tmp"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

fn sandboxed_env(req: &RunRequest) -> Vec<(String, String)> {
    let mut env = vec![("CARGO_NET_OFFLINE".to_string(), "true".to_string())];
    env.extend(req.env.iter().cloned());
    env
}

/// The cargo invocation for the build or test phase.
fn cargo_command(req: &RunRequest, phase: Phase, fetched: bool) -> Vec<String> {
    let mut args = vec!["cargo".to_string()];
    match (req.command, phase) {
        (CargoCommand::Test, Phase::Build) => args.extend(["test".into(), "--no-run".into()]),
        (command, _) => args.push(command.as_str().into()),
    }
    if phase == Phase::Build {
        args.push("--message-format=json".into());
    }
    if fetched {
        // Cargo.lock is now complete and every dependency is on disk.
        args.push("--frozen".into());
    } else {
        args.push("--offline".into());
        if req.cargo.locked {
            args.push("--locked".into());
        }
    }
    args.extend(req.cargo.to_args());
    if phase == Phase::Test {
        args.push("--no-fail-fast".into());
        if !req.test_args.is_empty() {
            args.push("--".into());
            args.extend(req.test_args.iter().cloned());
        }
    }
    args
}

fn check_limits(outcome: &ContainerOutcome, phase: Phase) -> Result<(), Stop> {
    if outcome.oom_killed {
        return Err(Stop::new(Status::Oom, phase));
    }
    if outcome.timed_out {
        return Err(Stop::new(Status::Timeout, phase));
    }
    Ok(())
}

/// Guarantee that a failed phase explains itself, even when nothing could be parsed.
fn ensure_error(diagnostics: &mut Vec<Diagnostic>, what: &str, outcome: &ContainerOutcome) {
    if diagnostics.iter().any(|d| d.level == Level::Error) {
        return;
    }
    let code = outcome
        .exit_code
        .map_or("no exit code".to_string(), |c| format!("exit code {c}"));
    diagnostics.push(Diagnostic {
        level: Level::Error,
        source: DiagnosticSource::Cargo,
        code: None,
        message: format!(
            "{what} failed with {code} and no parseable error; see the logs in `log_dir`"
        ),
        location: None,
        label: None,
        notes: Vec::new(),
        suggestions: Vec::new(),
        rendered: None,
    });
}

fn looks_like_network_error(stderr: &str) -> bool {
    const MARKERS: [&str; 9] = [
        "Could not resolve host",
        "Couldn't resolve host",
        "failed to resolve address",
        "Connection refused",
        "Connection timed out",
        "network failure seems to have happened",
        "spurious network error",
        "SSL connect error",
        "[6] Couldn't resolve",
    ];
    MARKERS.iter().any(|m| stderr.contains(m))
}

fn read_log(path: &Path) -> String {
    fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

/// `uid:gid` of the cache directory's owner, so containers write files the
/// host user can delete.
#[cfg(unix)]
fn owner_of(path: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata(path).ok()?;
    Some(format!("{}:{}", meta.uid(), meta.gid()))
}

#[cfg(not(unix))]
fn owner_of(_path: &Path) -> Option<String> {
    None
}

fn target_volume(key: &str) -> String {
    format!("isobuild-target-{key}")
}

/// Deletes the staged project and any throwaway target volume, keeping logs.
struct RunCleanup<'a> {
    backend: &'a dyn Backend,
    run_dir: PathBuf,
    keep_staging: bool,
    volume: Option<String>,
}

impl Drop for RunCleanup<'_> {
    fn drop(&mut self) {
        if !self.keep_staging {
            let _ = fs::remove_dir_all(self.run_dir.join("work"));
        }
        if let Some(volume) = &self.volume {
            self.backend.remove_volume(volume);
        }
    }
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

fn success_summary(report: &Report) -> String {
    let warnings = plural(report.diagnostic_counts.warnings, "warning");
    match &report.tests {
        Some(t) => format!("{} passed, {} ignored, {warnings}", t.passed, t.ignored),
        None => format!("no errors, {warnings}"),
    }
}

fn failure_summary(report: &Report, req: &RunRequest, phase: Phase) -> String {
    let counts = report.diagnostic_counts;
    let first_error = report
        .diagnostics
        .iter()
        .find(|d| d.level == Level::Error)
        .map(|d| d.message.as_str())
        .unwrap_or("unknown error");
    match report.status {
        Status::FetchFailed => format!("dependency resolution failed: {first_error}"),
        Status::BuildFailed => format!(
            "build failed: {}, {}",
            plural(counts.errors, "error"),
            plural(counts.warnings, "warning")
        ),
        Status::TestFailed => match &report.tests {
            Some(t) if t.failed > 0 => {
                format!("{} of {} tests failed", t.failed, t.passed + t.failed)
            }
            _ => format!("tests failed: {first_error}"),
        },
        Status::Timeout => {
            let limit = match phase {
                Phase::Prepare | Phase::Fetch => req.limits.fetch_timeout_secs,
                Phase::Build => req.limits.build_timeout_secs,
                Phase::Test => req.limits.test_timeout_secs,
            };
            let slow = report
                .tests
                .as_ref()
                .filter(|t| !t.slow.is_empty())
                .map(|t| format!("; still running: {}", t.slow.join(", ")))
                .unwrap_or_default();
            format!(
                "timed out after {limit}s in the {} phase{slow}",
                phase_name(phase)
            )
        }
        Status::Oom => format!(
            "ran out of memory (limit {} MB) in the {} phase",
            req.limits.memory_mb,
            phase_name(phase)
        ),
        status => status.as_str().replace('_', " "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::CargoOptions;

    fn request(command: CargoCommand) -> RunRequest {
        RunRequest::new(command, "/p", "/c")
    }

    #[test]
    fn test_command_builds_then_runs() {
        let mut req = request(CargoCommand::Test);
        req.test_args = vec!["--exact".into(), "it".into()];
        req.cargo = CargoOptions {
            release: true,
            ..Default::default()
        };
        assert_eq!(
            cargo_command(&req, Phase::Build, true),
            [
                "cargo",
                "test",
                "--no-run",
                "--message-format=json",
                "--frozen",
                "--release"
            ]
        );
        assert_eq!(
            cargo_command(&req, Phase::Test, true),
            [
                "cargo",
                "test",
                "--frozen",
                "--release",
                "--no-fail-fast",
                "--",
                "--exact",
                "it"
            ]
        );
    }

    #[test]
    fn offline_runs_use_offline_and_keep_locked() {
        let mut req = request(CargoCommand::Clippy);
        req.cargo.locked = true;
        assert_eq!(
            cargo_command(&req, Phase::Build, false),
            [
                "cargo",
                "clippy",
                "--message-format=json",
                "--offline",
                "--locked"
            ]
        );
    }

    #[test]
    fn network_errors_are_recognised() {
        assert!(looks_like_network_error(
            "error: failed to get `serde` as a dependency\nCaused by:\n  [6] Couldn't resolve host name (Could not resolve host: index.crates.io)"
        ));
        assert!(!looks_like_network_error(
            "error: no matching package named `serdee` found"
        ));
    }
}
