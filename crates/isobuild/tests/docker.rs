//! End-to-end runs in real Docker containers. They check the sandbox's
//! security properties, not just its plumbing.
//!
//! Skipped unless `ISOBUILD_DOCKER_TESTS=1`. They need a running Docker
//! daemon, the default image (pulled on first use), and network access for
//! the one test with a crates.io dependency.

use std::fs;
use std::path::{Path, PathBuf};

use isobuild::{CargoCommand, DockerBackend, Report, RunRequest, run};
use isobuild_report::{Phase, Status};

fn enabled() -> bool {
    let on = std::env::var("ISOBUILD_DOCKER_TESTS").is_ok_and(|v| v == "1");
    if !on {
        eprintln!("skipped: set ISOBUILD_DOCKER_TESTS=1 to run Docker tests");
    }
    on
}

/// One cache for every test, so dependencies download once.
fn cache_dir() -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join("docker-tests-cache")
}

fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (path, content) in files {
        let path = dir.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    dir
}

const MANIFEST: &str = "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";

fn sandbox(command: CargoCommand, project: &Path, tweak: impl FnOnce(&mut RunRequest)) -> Report {
    sandbox_with(&DockerBackend::new(), command, project, tweak)
}

fn sandbox_with(
    backend: &DockerBackend,
    command: CargoCommand,
    project: &Path,
    tweak: impl FnOnce(&mut RunRequest),
) -> Report {
    let mut req = RunRequest::new(command, project, cache_dir());
    req.limits.memory_mb = 2048;
    tweak(&mut req);
    let report = run(&req, backend);
    eprintln!("{}", serde_json::to_string_pretty(&report).unwrap());
    report
}

#[test]
fn dependencies_are_fetched_then_built_offline() {
    if !enabled() {
        return;
    }
    let dir = project(&[
        (
            "Cargo.toml",
            &format!("{MANIFEST}[dependencies]\nitoa = \"1\"\n"),
        ),
        (
            "src/lib.rs",
            "pub fn f(n: u64) -> String { itoa::Buffer::new().format(n).to_string() }\n#[test] fn it() { assert_eq!(f(7), \"7\"); }\n",
        ),
    ]);
    let report = sandbox(CargoCommand::Test, dir.path(), |_| {});
    assert_eq!(report.status, Status::Ok);
    assert_eq!(report.tests.unwrap().passed, 1);
}

#[test]
fn build_scripts_and_tests_have_no_network() {
    if !enabled() {
        return;
    }
    let probe = r#"
        use std::net::{TcpStream, ToSocketAddrs};
        use std::time::Duration;
        pub fn network_reachable() -> bool {
            let ip = TcpStream::connect_timeout(&"1.1.1.1:443".parse().unwrap(), Duration::from_secs(3)).is_ok();
            let dns = "crates.io:443".to_socket_addrs().is_ok();
            ip || dns
        }
    "#;
    let dir = project(&[
        ("Cargo.toml", MANIFEST),
        ("probe.rs", probe),
        (
            "build.rs",
            "include!(\"probe.rs\");\nfn main() { assert!(!network_reachable(), \"build script reached the network\"); }\n",
        ),
        (
            "src/lib.rs",
            "include!(\"../probe.rs\");\n#[test] fn offline() { assert!(!network_reachable(), \"test reached the network\"); }\n",
        ),
    ]);
    let report = sandbox(CargoCommand::Test, dir.path(), |_| {});
    assert_eq!(report.status, Status::Ok, "{}", report.summary);
}

#[test]
fn host_environment_and_files_stay_out() {
    if !enabled() {
        return;
    }
    let secret_dir = tempfile::tempdir().unwrap();
    let secret = secret_dir.path().join("credentials");
    fs::write(&secret, "hunter2").unwrap();

    let test = r#"
        #[test] fn isolated() {
            assert!(std::env::var("ISOBUILD_TEST_SECRET").is_err(), "host env leaked");
            assert_eq!(std::env::var("GIVEN").as_deref(), Ok("yes"));
            assert!(std::fs::read_to_string("secret-link").is_err(), "symlink reached the host");
            assert!(std::fs::write("/usr/pwned", "x").is_err(), "root filesystem is writable");
            std::fs::write("/tmp/scratch", "x").expect("/tmp is writable");
            std::fs::write("scratch", "x").expect("the project copy is writable");
        }
    "#;
    let dir = project(&[("Cargo.toml", MANIFEST), ("src/lib.rs", test)]);
    #[cfg(unix)]
    std::os::unix::fs::symlink(&secret, dir.path().join("secret-link")).unwrap();

    // The docker CLI process sees the secret; the container must not.
    let wrapper_dir = tempfile::tempdir().unwrap();
    let wrapper = wrapper_dir.path().join("docker");
    fs::write(
        &wrapper,
        "#!/bin/sh\nexport ISOBUILD_TEST_SECRET=hunter2\nexec docker \"$@\"\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let backend = DockerBackend::with_program(wrapper.to_str().unwrap());
    let report = sandbox_with(&backend, CargoCommand::Test, dir.path(), |req| {
        req.env.push(("GIVEN".into(), "yes".into()))
    });
    assert_eq!(report.status, Status::Ok, "{}", report.summary);
    assert!(
        !dir.path().join("scratch").exists(),
        "the original project is untouched"
    );
}

#[test]
fn compile_errors_come_back_structured() {
    if !enabled() {
        return;
    }
    let dir = project(&[
        ("Cargo.toml", MANIFEST),
        ("src/lib.rs", "pub fn f() -> u32 { \"no\" }\n"),
    ]);
    let report = sandbox(CargoCommand::Check, dir.path(), |_| {});
    assert_eq!(report.status, Status::BuildFailed);
    let error = &report.diagnostics[0];
    assert_eq!(error.code.as_deref(), Some("E0308"));
    let location = error.location.as_ref().unwrap();
    assert_eq!((location.file.as_str(), location.line), ("src/lib.rs", 1));
}

#[test]
fn test_failures_come_back_structured() {
    if !enabled() {
        return;
    }
    let dir = project(&[
        ("Cargo.toml", MANIFEST),
        (
            "src/lib.rs",
            "#[test] fn good() {}\n#[test] fn bad() { assert_eq!(1 + 1, 3); }\n",
        ),
    ]);
    let report = sandbox(CargoCommand::Test, dir.path(), |_| {});
    assert_eq!(report.status, Status::TestFailed);
    let tests = report.tests.unwrap();
    assert_eq!((tests.passed, tests.failed), (1, 1));
    assert_eq!(tests.failures[0].name, "bad");
    assert_eq!(tests.failures[0].location.as_ref().unwrap().line, 2);
}

#[test]
fn runaway_tests_are_stopped_and_cleaned_up() {
    if !enabled() {
        return;
    }
    let dir = project(&[
        ("Cargo.toml", MANIFEST),
        (
            "src/lib.rs",
            "#[test] fn spin() { loop { std::hint::spin_loop(); } }\n",
        ),
    ]);
    let report = sandbox(CargoCommand::Test, dir.path(), |req| {
        req.limits.test_timeout_secs = 5
    });
    assert_eq!(report.status, Status::Timeout);
    assert_eq!(report.failed_phase, Some(Phase::Test));

    let leftovers = std::process::Command::new("docker")
        .args([
            "ps",
            "--all",
            "--quiet",
            "--filter",
            "label=isobuild=1",
            "--filter",
            "status=running",
        ])
        .output()
        .unwrap();
    let running = String::from_utf8_lossy(&leftovers.stdout);
    // Other tests may be running in parallel; only this run's containers must be gone.
    let log_dir = report.log_dir.unwrap();
    let run_id = Path::new(&log_dir)
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let names = std::process::Command::new("docker")
        .args(["ps", "--all", "--format", "{{.Names}}"])
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&names.stdout).contains(&run_id),
        "container of run {run_id} left behind (running: {running})"
    );
}

#[test]
fn memory_limit_is_enforced() {
    if !enabled() {
        return;
    }
    let test = "#[test] fn hog() { let mut v: Vec<Vec<u8>> = Vec::new(); loop { v.push(vec![1u8; 64 << 20]); } }\n";
    let dir = project(&[("Cargo.toml", MANIFEST), ("src/lib.rs", test)]);
    let report = sandbox(CargoCommand::Test, dir.path(), |req| {
        req.limits.memory_mb = 512
    });
    assert_eq!(report.status, Status::Oom, "{}", report.summary);
    assert_eq!(report.failed_phase, Some(Phase::Test));
}
