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

use crate::backend::{Backend, BackendError, SandboxOutcome, SandboxSpec};
use crate::cache::{CacheLayout, build_cache_key, default_scope, remove_tree};
use crate::request::{CargoCommand, RunRequest};
use crate::stage::{StageError, copy_project};
use crate::toolchain::Toolchain;

const RUN_LOG_RETENTION: Duration = Duration::from_secs(3 * 24 * 3600);
const BUILD_CACHE_RETENTION: Duration = Duration::from_secs(14 * 24 * 3600);

/// Directories of a run directory that hold sandbox data, deleted after the
/// run. Everything else in it is logs.
const SCRATCH_DIRS: [&str; 5] = ["work", "tmp", "cargo-home", "bin", "target"];

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

    let io_failure = |what: &'static str| {
        move |err: std::io::Error| {
            Stop::because(Status::InfraError, Phase::Prepare, format!("{what}: {err}"))
        }
    };
    CacheLayout::new(&req.cache_dir)
        .ensure()
        .map_err(io_failure("could not create the cache directory"))?;
    // Sandbox rules match resolved paths, so every path starts canonical.
    let layout = CacheLayout::new(
        req.cache_dir
            .canonicalize()
            .map_err(io_failure("could not resolve the cache directory"))?,
    );

    backend.preflight().map_err(infra(Phase::Prepare))?;
    let toolchain = backend
        .toolchain(req.toolchain.as_deref(), layout.root())
        .map_err(infra(Phase::Prepare))?;
    report.environment = Some(Environment {
        backend: backend.name().to_string(),
        toolchain: toolchain.version.clone(),
        toolchain_id: toolchain.id.clone(),
        limits: req.limits.clone(),
    });
    if req.command == CargoCommand::Clippy && !toolchain.has_clippy() {
        return Err(Stop::because(
            Status::InfraError,
            Phase::Prepare,
            "clippy is not installed for this toolchain; install it with `rustup component add clippy`",
        ));
    }

    layout.prune(RUN_LOG_RETENTION, BUILD_CACHE_RETENTION);
    let (_, run_dir) = layout
        .new_run_dir()
        .map_err(io_failure("could not create the run directory"))?;
    report.log_dir = Some(run_dir.display().to_string());
    let _cleanup = RunCleanup {
        run_dir: run_dir.clone(),
        keep_staging: req.keep_staging,
    };

    let work = run_dir.join("work");
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
    let dirs = RunDirs::create(&run_dir, &layout, &toolchain)
        .map_err(io_failure("could not prepare the sandbox directories"))?;

    let target = if req.build_cache {
        let scope = req
            .cache_scope
            .clone()
            .unwrap_or_else(|| default_scope(&req.project));
        let key = build_cache_key(&scope, &toolchain.id);
        let target = layout.target_dir(&key);
        let warm = target.is_dir();
        fs::create_dir_all(&target).map_err(io_failure("could not create the build cache"))?;
        layout
            .touch_target(&key)
            .map_err(io_failure("could not update the build cache"))?;
        report.cache = Some(CacheInfo { key, warm });
        target
    } else {
        let target = run_dir.join("target");
        fs::create_dir(&target).map_err(io_failure("could not create the target directory"))?;
        target
    };

    let phases = Phases {
        req,
        backend,
        toolchain: &toolchain,
        run_dir,
        work: work.clone(),
        dirs,
    };
    // Diagnostics name files relative to the staged copy.
    let root = work.display().to_string();

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
        let mut args = vec!["fetch".to_string()];
        if req.cargo.locked {
            args.push("--locked".into());
        }
        let spec = phases.spec(
            Phase::Fetch,
            args,
            &layout.cargo_home(),
            None,
            Vec::new(),
            vec![layout.cargo_home()],
        );
        let outcome = phases.run(Phase::Fetch, &spec, report)?;
        check_limits(&outcome, Phase::Fetch)?;
        if outcome.exit_code != Some(0) {
            let stderr = read_log(&spec.stderr_path);
            let errors = parse_cargo_stderr(&stderr, &root, false);
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
    // The shared dependency cache is read-only from here on.
    let offline_spec = |phase| {
        phases.spec(
            phase,
            cargo_args(req, phase, fetched),
            &phases.dirs.cargo_home,
            Some(&target),
            vec![layout.registry(), layout.git()],
            vec![phases.dirs.cargo_home.clone(), target.clone()],
        )
    };
    let spec = offline_spec(Phase::Build);
    let outcome = phases.run(Phase::Build, &spec, report)?;
    let stdout = read_log(&spec.stdout_path);
    let stderr = read_log(&spec.stderr_path);
    diagnostics.extend(parse_cargo_messages(&stdout, &root));
    check_limits(&outcome, Phase::Build)?;
    if outcome.exit_code != Some(0) {
        if stderr.contains("error: no such command") {
            return Err(Stop::because(
                Status::InfraError,
                Phase::Build,
                format!(
                    "the toolchain does not provide `cargo {}`",
                    req.command.as_str()
                ),
            ));
        }
        diagnostics.extend(parse_cargo_stderr(&stderr, &root, true));
        ensure_error(diagnostics, "the build", &outcome);
        return Err(Stop::new(Status::BuildFailed, Phase::Build));
    }
    // Keep cargo's own warnings, such as `cargo:warning=` from build scripts.
    diagnostics.extend(parse_cargo_stderr(&stderr, &root, true));
    if req.command != CargoCommand::Test {
        return Ok(());
    }

    // Test: network off.
    let spec = offline_spec(Phase::Test);
    let outcome = phases.run(Phase::Test, &spec, report)?;
    let stdout = read_log(&spec.stdout_path);
    let tests = parse_libtest(&stdout, &root, req.max_test_failures);
    let failed = tests.failed;
    report.tests = Some(tests);
    check_limits(&outcome, Phase::Test)?;
    if outcome.exit_code != Some(0) {
        if failed == 0 {
            // A crashed test binary or a doctest that failed to build.
            diagnostics.extend(parse_cargo_stderr(
                &read_log(&spec.stderr_path),
                &root,
                false,
            ));
            ensure_error(diagnostics, "the tests", &outcome);
        }
        return Err(Stop::new(Status::TestFailed, Phase::Test));
    }
    Ok(())
}

/// The sandbox's private directories inside a run directory.
struct RunDirs {
    /// `HOME` and `TMPDIR`.
    tmp: PathBuf,
    /// `CARGO_HOME` for build and test: writable for cargo's locks and
    /// caches, with `registry` and `git` linked to the shared, read-only ones.
    cargo_home: PathBuf,
    /// Extra programs for `PATH`, as links to the toolchain's tools.
    bin: Option<PathBuf>,
}

impl RunDirs {
    fn create(
        run_dir: &Path,
        layout: &CacheLayout,
        toolchain: &Toolchain,
    ) -> std::io::Result<Self> {
        use std::os::unix::fs::symlink;
        let tmp = run_dir.join("tmp");
        fs::create_dir(&tmp)?;
        let cargo_home = run_dir.join("cargo-home");
        fs::create_dir(&cargo_home)?;
        symlink(layout.registry(), cargo_home.join("registry"))?;
        symlink(layout.git(), cargo_home.join("git"))?;
        let bin = if toolchain.tools.is_empty() {
            None
        } else {
            let bin = run_dir.join("bin");
            fs::create_dir(&bin)?;
            for (name, path) in &toolchain.tools {
                symlink(path, bin.join(name))?;
            }
            Some(bin)
        };
        Ok(RunDirs {
            tmp,
            cargo_home,
            bin,
        })
    }
}

/// Builds sandbox specs that share the run's settings.
struct Phases<'a> {
    req: &'a RunRequest,
    backend: &'a dyn Backend,
    toolchain: &'a Toolchain,
    run_dir: PathBuf,
    work: PathBuf,
    dirs: RunDirs,
}

impl Phases<'_> {
    /// A spec running `cargo <args>`. The staged project and the private
    /// temporary directory are always writable; the toolchain is always readable.
    fn spec(
        &self,
        phase: Phase,
        args: Vec<String>,
        cargo_home: &Path,
        target: Option<&Path>,
        read_only: Vec<PathBuf>,
        writable: Vec<PathBuf>,
    ) -> SandboxSpec {
        let name = phase_name(phase);
        let limits = &self.req.limits;
        let timeout = match phase {
            Phase::Prepare | Phase::Fetch => limits.fetch_timeout_secs,
            Phase::Build => limits.build_timeout_secs,
            Phase::Test => limits.test_timeout_secs,
        };

        let mut path = Vec::new();
        path.extend(self.dirs.bin.iter().map(|b| b.display().to_string()));
        path.push(self.toolchain.bin_dir().display().to_string());
        path.extend(["/usr/bin".to_string(), "/bin".to_string()]);
        let tmp = self.dirs.tmp.display().to_string();
        let mut env = vec![
            ("PATH".to_string(), path.join(":")),
            ("HOME".to_string(), tmp.clone()),
            ("TMPDIR".to_string(), tmp),
            ("CARGO_HOME".to_string(), cargo_home.display().to_string()),
            ("CARGO_TERM_COLOR".to_string(), "never".to_string()),
            ("CARGO_TERM_PROGRESS_WHEN".to_string(), "never".to_string()),
            (
                "CARGO_BUILD_JOBS".to_string(),
                (limits.cpus.ceil() as u64).max(1).to_string(),
            ),
            (
                "RUSTC".to_string(),
                self.toolchain.bin_dir().join("rustc").display().to_string(),
            ),
            (
                "RUSTDOC".to_string(),
                self.toolchain
                    .bin_dir()
                    .join("rustdoc")
                    .display()
                    .to_string(),
            ),
        ];
        if let Some(target) = target {
            env.push(("CARGO_TARGET_DIR".to_string(), target.display().to_string()));
        }
        env.extend(self.toolchain.env.iter().cloned());
        if phase != Phase::Fetch {
            env.push(("CARGO_NET_OFFLINE".to_string(), "true".to_string()));
            env.extend(self.req.env.iter().cloned());
        }

        let mut command = vec![self.toolchain.cargo().display().to_string()];
        command.extend(args);
        let mut all_read_only = self.toolchain.read_only.clone();
        all_read_only.extend(self.dirs.bin.iter().cloned());
        all_read_only.extend(read_only);
        let mut all_writable = vec![self.work.clone(), self.dirs.tmp.clone()];
        all_writable.extend(writable);

        SandboxSpec {
            name: name.to_string(),
            command,
            env,
            workdir: self.work.clone(),
            read_only: all_read_only,
            writable: all_writable,
            network: phase == Phase::Fetch,
            memory_mb: limits.memory_mb,
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
        spec: &SandboxSpec,
        report: &mut Report,
    ) -> Result<SandboxOutcome, Stop> {
        let outcome = self.backend.run(spec).map_err(infra(phase))?;
        report.phases.push(PhaseOutcome {
            phase,
            exit_code: outcome.exit_code,
            duration_ms: outcome.duration.as_millis() as u64,
            timed_out: outcome.timed_out,
            oom_killed: outcome.oom_killed,
            process_limit_hit: outcome.process_limit_hit,
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

/// Cargo's arguments for the build or test phase.
fn cargo_args(req: &RunRequest, phase: Phase, fetched: bool) -> Vec<String> {
    let mut args = Vec::new();
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

fn check_limits(outcome: &SandboxOutcome, phase: Phase) -> Result<(), Stop> {
    if outcome.oom_killed {
        return Err(Stop::new(Status::Oom, phase));
    }
    if outcome.process_limit_hit {
        return Err(Stop::new(Status::ProcessLimit, phase));
    }
    if outcome.timed_out {
        return Err(Stop::new(Status::Timeout, phase));
    }
    Ok(())
}

/// Guarantee that a failed phase explains itself, even when nothing could be parsed.
fn ensure_error(diagnostics: &mut Vec<Diagnostic>, what: &str, outcome: &SandboxOutcome) {
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

/// Deletes the run's sandbox directories when it ends, keeping its logs.
struct RunCleanup {
    run_dir: PathBuf,
    keep_staging: bool,
}

impl Drop for RunCleanup {
    fn drop(&mut self) {
        for dir in SCRATCH_DIRS {
            if dir == "work" && self.keep_staging {
                continue;
            }
            let _ = remove_tree(&self.run_dir.join(dir));
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
        Status::ProcessLimit => format!(
            "ran more than {} processes at once in the {} phase",
            req.limits.pids,
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
            cargo_args(&req, Phase::Build, true),
            [
                "test",
                "--no-run",
                "--message-format=json",
                "--frozen",
                "--release"
            ]
        );
        assert_eq!(
            cargo_args(&req, Phase::Test, true),
            [
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
            cargo_args(&req, Phase::Build, false),
            ["clippy", "--message-format=json", "--offline", "--locked"]
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
