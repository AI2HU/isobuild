//! Orchestration tests against a scripted backend: which sandboxes are
//! started, with what access, and how their outcomes become a report.

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use isobuild::cache::CacheLayout;
use isobuild::{
    Backend, BackendError, CargoCommand, RunRequest, SandboxOutcome, SandboxSpec, Toolchain, run,
};
use isobuild_report::{Phase, Severity, Status};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../diagnostics/tests/fixtures");

fn fixture(name: &str) -> String {
    fs::read_to_string(Path::new(FIXTURES).join(name)).unwrap()
}

/// What one sandbox "prints" and how it ends.
#[derive(Default, Clone)]
struct Step {
    exit_code: i32,
    stdout: String,
    stderr: String,
    timed_out: bool,
    oom_killed: bool,
    process_limit_hit: bool,
}

impl Step {
    fn ok() -> Self {
        Step::default()
    }

    fn exit(code: i32) -> Self {
        Step {
            exit_code: code,
            ..Default::default()
        }
    }

    fn stdout(mut self, text: impl Into<String>) -> Self {
        self.stdout = text.into();
        self
    }

    fn stderr(mut self, text: impl Into<String>) -> Self {
        self.stderr = text.into();
        self
    }
}

struct FakeBackend {
    unavailable: bool,
    /// A toolchain directory, with `bin/cargo-clippy` unless `no_clippy`.
    sysroot: tempfile::TempDir,
    steps: Mutex<VecDeque<Step>>,
    specs: Mutex<Vec<SandboxSpec>>,
}

impl Default for FakeBackend {
    fn default() -> Self {
        let sysroot = tempfile::tempdir().unwrap();
        fs::create_dir(sysroot.path().join("bin")).unwrap();
        fs::write(sysroot.path().join("bin/cargo-clippy"), "").unwrap();
        FakeBackend {
            unavailable: false,
            sysroot,
            steps: Mutex::default(),
            specs: Mutex::default(),
        }
    }
}

impl FakeBackend {
    fn with(steps: impl IntoIterator<Item = Step>) -> Self {
        let backend = FakeBackend::default();
        backend.push_steps(steps);
        backend
    }

    fn without_clippy(self) -> Self {
        fs::remove_file(self.sysroot.path().join("bin/cargo-clippy")).unwrap();
        self
    }

    fn specs(&self) -> Vec<SandboxSpec> {
        self.specs.lock().unwrap().clone()
    }

    fn push_steps(&self, steps: impl IntoIterator<Item = Step>) {
        self.steps.lock().unwrap().extend(steps);
    }
}

impl Backend for FakeBackend {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn preflight(&self) -> Result<(), BackendError> {
        if self.unavailable {
            Err(BackendError::Unavailable(
                "`sandbox-exec` cannot create sandboxes".into(),
            ))
        } else {
            Ok(())
        }
    }

    fn toolchain(&self, _requested: Option<&str>, cwd: &Path) -> Result<Toolchain, BackendError> {
        assert!(
            cwd.join("ISOBUILD_CACHE").is_file(),
            "resolved outside the project"
        );
        let sysroot = self.sysroot.path().canonicalize().unwrap();
        Ok(Toolchain {
            version: "rustc 1.91.1 (fake)".into(),
            id: "fake-toolchain".into(),
            read_only: vec![sysroot.clone()],
            sysroot,
            env: vec![("SDKROOT".into(), "/fake/sdk".into())],
            tools: vec![("cc".into(), "/usr/bin/true".into())],
        })
    }

    fn run(&self, spec: &SandboxSpec) -> Result<SandboxOutcome, BackendError> {
        self.specs.lock().unwrap().push(spec.clone());
        // The staged project must exist while its sandboxes run.
        assert!(spec.workdir.join("Cargo.toml").is_file());
        let step = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected sandbox");
        // Fixtures were recorded in /work; move them to the real staged copy.
        let work = format!("{}/", spec.workdir.display());
        fs::write(&spec.stdout_path, step.stdout.replace("/work/", &work)).unwrap();
        fs::write(&spec.stderr_path, step.stderr.replace("/work/", &work)).unwrap();
        Ok(SandboxOutcome {
            exit_code: Some(step.exit_code),
            timed_out: step.timed_out,
            oom_killed: step.oom_killed,
            process_limit_hit: step.process_limit_hit,
            duration: Duration::from_millis(5),
            output_truncated: false,
        })
    }
}

struct Setup {
    _dir: tempfile::TempDir,
    req: RunRequest,
}

fn setup(command: CargoCommand) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::write(project.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
    fs::write(project.join("src/lib.rs"), "").unwrap();
    let req = RunRequest::new(command, &project, dir.path().join("cache"));
    Setup { _dir: dir, req }
}

fn env<'a>(spec: &'a SandboxSpec, key: &str) -> Option<&'a str> {
    spec.env
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn cache(req: &RunRequest) -> CacheLayout {
    CacheLayout::new(req.cache_dir.canonicalize().unwrap())
}

#[test]
fn passing_test_run_uses_three_isolated_phases() {
    let mut s = setup(CargoCommand::Test);
    s.req.env.push(("RUST_LOG".into(), "debug".into()));
    let backend = FakeBackend::with([
        Step::ok(),
        Step::ok(),
        Step::ok().stdout("running 2 tests\ntest a ... ok\ntest b ... ok\n\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n"),
    ]);
    let report = run(&s.req, &backend);

    assert_eq!(report.status, Status::Ok, "{report:#?}");
    assert_eq!(report.summary, "2 passed, 0 ignored, 0 warnings");
    assert_eq!(report.tests.as_ref().unwrap().passed, 2);
    assert_eq!(
        report.phases.iter().map(|p| p.phase).collect::<Vec<_>>(),
        [Phase::Fetch, Phase::Build, Phase::Test]
    );
    let environment = report.environment.as_ref().unwrap();
    assert_eq!(environment.backend, "fake");
    assert_eq!(environment.toolchain_id, "fake-toolchain");
    assert!(!report.cache.as_ref().unwrap().warm);

    let layout = cache(&s.req);
    let sysroot = backend.sysroot.path().canonicalize().unwrap();
    let specs = backend.specs();
    let (fetch, build, test) = (&specs[0], &specs[1], &specs[2]);

    assert!(fetch.network, "fetch downloads dependencies");
    assert_eq!(
        fetch.command[0],
        sysroot.join("bin/cargo").display().to_string()
    );
    assert_eq!(fetch.command[1..], ["fetch"]);
    assert!(fetch.writable.contains(&layout.cargo_home()));
    assert_eq!(
        env(fetch, "CARGO_HOME"),
        Some(layout.cargo_home().to_str().unwrap())
    );
    assert!(
        env(fetch, "RUST_LOG").is_none(),
        "user env never reaches fetch"
    );

    for spec in [fetch, build, test] {
        assert!(
            spec.read_only.contains(&sysroot),
            "the toolchain is readable"
        );
        assert_eq!(env(spec, "SDKROOT"), Some("/fake/sdk"));
        assert_eq!(
            env(spec, "RUSTC"),
            Some(sysroot.join("bin/rustc").to_str().unwrap())
        );
        assert_eq!(env(spec, "HOME"), env(spec, "TMPDIR"));
        assert!(spec.writable.contains(&spec.workdir));
        for path in spec.read_only.iter().chain(&spec.writable) {
            assert!(path.is_absolute(), "{}", path.display());
        }
        assert!(
            !spec.writable.iter().any(|w| layout.root().starts_with(w)),
            "the cache root itself is never writable"
        );
    }
    for spec in [build, test] {
        assert!(!spec.network, "{} must be offline", spec.name);
        assert!(spec.read_only.contains(&layout.registry()));
        assert!(spec.read_only.contains(&layout.git()));
        assert!(
            !spec.writable.contains(&layout.cargo_home()),
            "shared cargo home is not writable"
        );
        let private_home = Path::new(env(spec, "CARGO_HOME").unwrap());
        assert!(spec.writable.iter().any(|w| w == private_home));
        assert_eq!(env(spec, "RUST_LOG"), Some("debug"));
        assert_eq!(env(spec, "CARGO_NET_OFFLINE"), Some("true"));
        let path = env(spec, "PATH").unwrap();
        assert!(path.starts_with(&format!("{}/bin:", report.log_dir.as_deref().unwrap())));
    }
    assert_eq!(
        build.command[1..4],
        ["test", "--no-run", "--message-format=json"]
    );
    assert_eq!(test.command[1..3], ["test", "--frozen"]);
    assert!(test.command.contains(&"--no-fail-fast".to_string()));

    // Logs are kept, the sandbox's directories are not.
    let log_dir = Path::new(report.log_dir.as_deref().unwrap());
    assert!(log_dir.join("test.stdout.log").is_file());
    for dir in ["work", "tmp", "cargo-home", "bin"] {
        assert!(!log_dir.join(dir).exists(), "{dir} left behind");
    }
}

#[test]
fn second_run_reuses_the_build_cache() {
    let s = setup(CargoCommand::Check);
    let backend = FakeBackend::with([Step::ok(), Step::ok()]);
    let first = run(&s.req, &backend);
    backend.push_steps([Step::ok(), Step::ok()]);
    let second = run(&s.req, &backend);
    let key = &first.cache.as_ref().unwrap().key;
    assert!(!first.cache.as_ref().unwrap().warm);
    assert_eq!(key, &second.cache.as_ref().unwrap().key);
    assert!(second.cache.unwrap().warm);
    let target = cache(&s.req).target_dir(key);
    let build = &backend.specs()[3];
    assert_eq!(
        env(build, "CARGO_TARGET_DIR"),
        Some(target.to_str().unwrap())
    );
    assert!(build.writable.contains(&target));
}

#[test]
fn other_projects_get_their_own_build_cache() {
    let a = setup(CargoCommand::Check);
    let b = setup(CargoCommand::Check);
    let backend = FakeBackend::with([Step::ok(), Step::ok(), Step::ok(), Step::ok()]);
    let first = run(&a.req, &backend);
    let second = run(&b.req, &backend);
    assert_ne!(first.cache.unwrap().key, second.cache.as_ref().unwrap().key);
    assert!(!second.cache.unwrap().warm);
}

#[test]
fn no_cache_uses_a_throwaway_target_directory() {
    let mut s = setup(CargoCommand::Build);
    s.req.build_cache = false;
    let backend = FakeBackend::with([Step::ok(), Step::ok()]);
    let report = run(&s.req, &backend);
    assert_eq!(report.status, Status::Ok);
    assert!(report.cache.is_none());
    let target = PathBuf::from(env(&backend.specs()[1], "CARGO_TARGET_DIR").unwrap());
    assert_eq!(
        target,
        Path::new(report.log_dir.as_deref().unwrap()).join("target")
    );
    assert!(!target.exists(), "removed after the run");
}

#[test]
fn private_cargo_home_links_to_the_shared_sources() {
    struct Checking(FakeBackend);
    impl Backend for Checking {
        fn name(&self) -> &'static str {
            self.0.name()
        }
        fn preflight(&self) -> Result<(), BackendError> {
            self.0.preflight()
        }
        fn toolchain(
            &self,
            requested: Option<&str>,
            cwd: &Path,
        ) -> Result<Toolchain, BackendError> {
            self.0.toolchain(requested, cwd)
        }
        fn run(&self, spec: &SandboxSpec) -> Result<SandboxOutcome, BackendError> {
            if !spec.network {
                let home = Path::new(env(spec, "CARGO_HOME").unwrap());
                for dir in ["registry", "git"] {
                    let link = fs::read_link(home.join(dir)).unwrap();
                    assert!(spec.read_only.contains(&link), "{dir} is read-only");
                }
                let cc = spec.read_only.iter().find(|p| p.ends_with("bin")).unwrap();
                assert_eq!(
                    fs::read_link(cc.join("cc")).unwrap(),
                    Path::new("/usr/bin/true")
                );
            }
            self.0.run(spec)
        }
    }
    let s = setup(CargoCommand::Check);
    let report = run(
        &s.req,
        &Checking(FakeBackend::with([Step::ok(), Step::ok()])),
    );
    assert_eq!(report.status, Status::Ok);
}

#[test]
fn compile_errors_become_build_failed() {
    let s = setup(CargoCommand::Check);
    let backend = FakeBackend::with([
        Step::ok(),
        Step::exit(101)
            .stdout(fixture("borrow_err.stdout"))
            .stderr("error: could not compile `borrow_err` (lib) due to 1 previous error; 2 warnings emitted\n"),
    ]);
    let report = run(&s.req, &backend);
    assert_eq!(report.status, Status::BuildFailed);
    assert_eq!(report.failed_phase, Some(Phase::Build));
    assert_eq!(report.summary, "build failed: 1 error, 2 warnings");
    assert_eq!(report.diagnostics[0].code.as_deref(), Some("E0384"));
    assert!(report.tests.is_none());
    assert_eq!(
        backend.specs().len(),
        2,
        "no test phase after a failed build"
    );
}

#[test]
fn build_script_failure_is_explained() {
    let s = setup(CargoCommand::Test);
    let backend = FakeBackend::with([
        Step::ok(),
        Step::exit(101)
            .stdout(fixture("buildrs_fail.stdout"))
            .stderr(fixture("buildrs_fail.stderr")),
    ]);
    let report = run(&s.req, &backend);
    assert_eq!(report.status, Status::BuildFailed);
    let error = &report.diagnostics[0];
    assert!(
        error
            .message
            .starts_with("failed to run custom build command")
    );
    assert!(
        error
            .rendered
            .as_deref()
            .unwrap()
            .contains("could not find libfoo")
    );
    assert_eq!(
        report.diagnostic_counts.warnings, 1,
        "cargo:warning is kept"
    );
}

#[test]
fn failing_tests_become_test_failed() {
    let s = setup(CargoCommand::Test);
    let backend = FakeBackend::with([
        Step::ok(),
        Step::ok(),
        Step::exit(101)
            .stdout(fixture("tests_run.stdout"))
            .stderr(fixture("tests_run.stderr")),
    ]);
    let report = run(&s.req, &backend);
    assert_eq!(report.status, Status::TestFailed);
    assert_eq!(report.summary, "5 of 7 tests failed");
    assert_eq!(report.tests.as_ref().unwrap().failures.len(), 5);
    assert!(
        report.diagnostics.is_empty(),
        "cargo's test summaries are not diagnostics"
    );
}

#[test]
fn crashed_test_binary_without_failures_still_fails() {
    let s = setup(CargoCommand::Test);
    let backend = FakeBackend::with([
        Step::ok(),
        Step::ok(),
        Step::exit(101)
            .stdout("running 1 test\n")
            .stderr("error: test failed, to rerun pass `--lib`\n\nCaused by:\n  process didn't exit successfully: `/cache/target/debug/deps/demo-1` (signal: 11, SIGSEGV: invalid memory reference)\n"),
    ]);
    let report = run(&s.req, &backend);
    assert_eq!(report.status, Status::TestFailed);
    assert!(!report.tests.as_ref().unwrap().complete);
    assert_eq!(
        report.diagnostics.len(),
        1,
        "a fallback error points at the logs"
    );
    assert!(report.summary.starts_with("tests failed: "));
}

#[test]
fn timeouts_keep_partial_test_results() {
    let s = setup(CargoCommand::Test);
    let backend = FakeBackend::with([
        Step::ok(),
        Step::ok(),
        Step {
            exit_code: 137,
            timed_out: true,
            stdout: "running 2 tests\ntest fast ... ok\ntest spins has been running for over 60 seconds\n".into(),
            ..Default::default()
        },
    ]);
    let report = run(&s.req, &backend);
    assert_eq!(report.status, Status::Timeout);
    assert_eq!(report.failed_phase, Some(Phase::Test));
    assert_eq!(
        report.summary,
        "timed out after 600s in the test phase; still running: spins"
    );
    let tests = report.tests.unwrap();
    assert_eq!(tests.passed, 1);
    assert!(!tests.complete);
}

#[test]
fn oom_is_reported_as_such() {
    let s = setup(CargoCommand::Build);
    let backend = FakeBackend::with([
        Step::ok(),
        Step {
            exit_code: 137,
            oom_killed: true,
            ..Default::default()
        },
    ]);
    let report = run(&s.req, &backend);
    assert_eq!(report.status, Status::Oom);
    assert_eq!(
        report.summary,
        "ran out of memory (limit 4096 MB) in the build phase"
    );
    assert!(report.phases[1].oom_killed);
}

#[test]
fn unknown_dependency_is_a_fetch_failure() {
    let s = setup(CargoCommand::Check);
    let backend = FakeBackend::with([Step::exit(101).stderr(
        "    Updating crates.io index\nerror: no matching package named `serdee` found\nlocation searched: crates.io index\n",
    )]);
    let report = run(&s.req, &backend);
    assert_eq!(report.status, Status::FetchFailed);
    assert_eq!(
        report.summary,
        "dependency resolution failed: no matching package named `serdee` found"
    );
}

#[test]
fn network_trouble_while_fetching_is_infra() {
    let s = setup(CargoCommand::Check);
    let backend = FakeBackend::with([Step::exit(101).stderr(
        "error: failed to query replaced source registry `crates-io`\n\nCaused by:\n  [6] Couldn't resolve host name (Could not resolve host: index.crates.io)\n",
    )]);
    let report = run(&s.req, &backend);
    assert_eq!(report.status, Status::InfraError);
    assert_eq!(report.failed_phase, Some(Phase::Fetch));
}

#[test]
fn locked_and_offline_change_the_commands() {
    let mut s = setup(CargoCommand::Check);
    s.req.cargo.locked = true;
    let backend = FakeBackend::with([Step::ok(), Step::ok()]);
    run(&s.req, &backend);
    assert_eq!(backend.specs()[0].command[1..], ["fetch", "--locked"]);

    s.req.offline = true;
    let backend = FakeBackend::with([Step::ok()]);
    let report = run(&s.req, &backend);
    assert_eq!(report.status, Status::Ok);
    let specs = backend.specs();
    assert_eq!(specs.len(), 1, "no fetch phase");
    assert!(specs[0].command.contains(&"--offline".to_string()));
    assert!(specs[0].command.contains(&"--locked".to_string()));
}

#[test]
fn missing_clippy_is_infra_not_a_code_error() {
    let s = setup(CargoCommand::Clippy);
    let backend = FakeBackend::default().without_clippy();
    let report = run(&s.req, &backend);
    assert_eq!(report.status, Status::InfraError);
    assert_eq!(report.failed_phase, Some(Phase::Prepare));
    assert!(report.summary.contains("rustup component add clippy"));
    assert!(backend.specs().is_empty());

    let backend = FakeBackend::with([
        Step::ok(),
        Step::exit(101).stderr("error: no such command: `clippy`\n"),
    ]);
    let report = run(&s.req, &backend);
    assert_eq!(report.status, Status::InfraError);
    assert!(report.summary.contains("does not provide `cargo clippy`"));
}

#[test]
fn too_many_processes_is_its_own_status() {
    let s = setup(CargoCommand::Test);
    let backend = FakeBackend::with([
        Step::ok(),
        Step::ok(),
        Step {
            exit_code: 137,
            process_limit_hit: true,
            ..Default::default()
        },
    ]);
    let report = run(&s.req, &backend);
    assert_eq!(report.status, Status::ProcessLimit);
    assert_eq!(
        report.summary,
        "ran more than 1024 processes at once in the test phase"
    );
    assert!(report.phases[2].process_limit_hit);
}

#[test]
fn diagnostics_are_relative_to_the_project() {
    let s = setup(CargoCommand::Check);
    let backend = FakeBackend::with([
        Step::ok(),
        Step::exit(101).stdout(fixture("compile_err.stdout")),
    ]);
    let report = run(&s.req, &backend);
    let location = report.diagnostics[0].location.as_ref().unwrap();
    assert_eq!(location.file, "src/lib.rs");
}

#[test]
fn denied_policy_runs_nothing() {
    let s = setup(CargoCommand::Test);
    let config = s.req.project.join(".cargo");
    fs::create_dir(&config).unwrap();
    fs::write(
        config.join("config.toml"),
        "[build]\nrustc-wrapper = \"./steal.sh\"\n",
    )
    .unwrap();
    let backend = FakeBackend::default();
    let report = run(&s.req, &backend);
    assert_eq!(report.status, Status::PolicyDenied);
    assert_eq!(report.failed_phase, Some(Phase::Prepare));
    assert_eq!(report.policy[0].rule, "cargo-config.rustc-wrapper");
    assert_eq!(report.policy[0].severity, Severity::Deny);
    assert!(backend.specs().is_empty());
}

#[test]
fn toolchain_file_is_left_out_of_the_sandbox() {
    let s = setup(CargoCommand::Check);
    fs::write(
        s.req.project.join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"nightly\"\n",
    )
    .unwrap();

    struct Checking(FakeBackend);
    impl Backend for Checking {
        fn name(&self) -> &'static str {
            self.0.name()
        }
        fn preflight(&self) -> Result<(), BackendError> {
            self.0.preflight()
        }
        fn toolchain(
            &self,
            requested: Option<&str>,
            cwd: &Path,
        ) -> Result<Toolchain, BackendError> {
            self.0.toolchain(requested, cwd)
        }
        fn run(&self, spec: &SandboxSpec) -> Result<SandboxOutcome, BackendError> {
            assert!(!spec.workdir.join("rust-toolchain.toml").exists());
            self.0.run(spec)
        }
    }

    let report = run(
        &s.req,
        &Checking(FakeBackend::with([Step::ok(), Step::ok()])),
    );
    assert_eq!(report.status, Status::Ok);
    assert_eq!(report.policy[0].rule, "toolchain.file-ignored");
    assert!(
        s.req.project.join("rust-toolchain.toml").exists(),
        "the original is untouched"
    );
}

#[test]
fn unusable_sandbox_is_infra() {
    let s = setup(CargoCommand::Check);
    let backend = FakeBackend {
        unavailable: true,
        ..Default::default()
    };
    let report = run(&s.req, &backend);
    assert_eq!(report.status, Status::InfraError);
    assert_eq!(report.failed_phase, Some(Phase::Prepare));
    assert!(report.summary.contains("cannot create sandboxes"));
}

#[test]
fn bad_requests_are_refused_before_anything_runs() {
    let mut s = setup(CargoCommand::Test);
    s.req
        .env
        .push(("CARGO_TARGET_DIR".into(), "/elsewhere".into()));
    let report = run(&s.req, &FakeBackend::default());
    assert_eq!(report.status, Status::InvalidRequest);

    let mut s = setup(CargoCommand::Check);
    s.req.max_project_bytes = 4;
    let report = run(&s.req, &FakeBackend::default());
    assert_eq!(report.status, Status::InvalidRequest);
    assert!(report.summary.contains("larger than"));
}

#[test]
fn diagnostics_are_capped() {
    let mut s = setup(CargoCommand::Check);
    s.req.max_diagnostics = 1;
    let backend = FakeBackend::with([
        Step::ok(),
        Step::exit(101).stdout(fixture("borrow_err.stdout")),
    ]);
    let report = run(&s.req, &backend);
    assert_eq!(report.diagnostics.len(), 1);
    assert_eq!(report.diagnostic_counts.omitted, 2);
    assert_eq!(report.diagnostics[0].level, isobuild_report::Level::Error);
}
