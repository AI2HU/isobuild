//! End-to-end runs in the real sandbox of this platform (Seatbelt on macOS,
//! bubblewrap on Linux). They check the sandbox's security properties, not
//! just its plumbing.
//!
//! They need the host's Rust toolchain and, on Linux, `bwrap`. The one test
//! that downloads a crate needs network access and `ISOBUILD_NETWORK_TESTS=1`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use isobuild::{CargoCommand, Report, RunRequest, default_backend, run};
use isobuild_report::{Phase, Status};

/// One cache for every test, so dependencies download once.
fn cache_dir() -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join("sandbox-tests-cache")
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
    let mut req = RunRequest::new(command, project, cache_dir());
    req.limits.memory_mb = 2048;
    tweak(&mut req);
    let report = run(&req, default_backend().as_ref());
    eprintln!("{}", serde_json::to_string_pretty(&report).unwrap());
    report
}

#[test]
fn dependencies_are_fetched_then_built_offline() {
    if std::env::var("ISOBUILD_NETWORK_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: set ISOBUILD_NETWORK_TESTS=1 to download from crates.io");
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
    assert_eq!(report.status, Status::Ok, "{}", report.summary);
    assert_eq!(report.tests.unwrap().passed, 1);
}

#[test]
fn build_scripts_and_tests_have_no_network() {
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
    // A file outside the project and the cache, like ~/.ssh/id_rsa.
    let secret_dir = tempfile::tempdir().unwrap();
    let secret = secret_dir.path().join("credentials");
    fs::write(&secret, "hunter2").unwrap();
    let home = std::env::var("HOME").unwrap();

    let test = format!(
        r#"
        #[test] fn isolated() {{
            assert_ne!(std::env::var("HOME").unwrap(), {home:?}, "host HOME leaked");
            for var in ["USER", "SSH_AUTH_SOCK", "CARGO_TARGET_TMPDIR"] {{
                assert!(std::env::var(var).is_err(), "host env {{var}} leaked");
            }}
            assert_eq!(std::env::var("GIVEN").as_deref(), Ok("yes"));
            assert!(std::fs::read_to_string({secret:?}).is_err(), "host file is readable");
            assert!(std::fs::read_to_string("secret-link").is_err(), "symlink reached the host");
            assert!(std::fs::read_dir({home:?}).map_or(true, |d| d.count() <= 1), "home is listable");
            assert!(std::fs::hard_link({secret:?}, "hard-link").is_err(), "hard link to a host file");
            assert!(std::fs::write("/usr/pwned", "x").is_err(), "system directories are writable");
            std::fs::write(std::env::temp_dir().join("scratch"), "x").expect("TMPDIR is writable");
            std::fs::write("scratch", "x").expect("the project copy is writable");
        }}
    "#
    );
    let dir = project(&[("Cargo.toml", MANIFEST), ("src/lib.rs", &test)]);
    std::os::unix::fs::symlink(&secret, dir.path().join("secret-link")).unwrap();

    let report = sandbox(CargoCommand::Test, dir.path(), |req| {
        req.env.push(("GIVEN".into(), "yes".into()))
    });
    assert_eq!(report.status, Status::Ok, "{}", report.summary);
    assert!(
        !dir.path().join("scratch").exists(),
        "the original project is untouched"
    );
}

#[test]
fn build_scripts_cannot_write_outside_their_directories() {
    let build = r#"
        fn main() {
            let home = std::env::var("CARGO_MANIFEST_DIR").unwrap();
            let registry = std::path::Path::new(&std::env::var("CARGO_HOME").unwrap()).join("registry/planted");
            assert!(std::fs::write(&registry, "x").is_err(), "the shared dependency cache is writable");
            let above = std::path::Path::new(&home).parent().unwrap().join("planted");
            assert!(std::fs::write(&above, "x").is_err(), "the run directory is writable");
            assert!(std::fs::write("/planted", "x").is_err(), "the root is writable");
        }
    "#;
    let dir = project(&[
        ("Cargo.toml", MANIFEST),
        ("build.rs", build),
        ("src/lib.rs", ""),
    ]);
    let report = sandbox(CargoCommand::Build, dir.path(), |_| {});
    assert_eq!(report.status, Status::Ok, "{}", report.summary);
}

#[test]
fn compile_errors_come_back_structured() {
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
    let dir = project(&[
        ("Cargo.toml", MANIFEST),
        (
            "src/lib.rs",
            "#[test] fn spin() { loop { std::hint::spin_loop(); } }\n",
        ),
    ]);
    let report = sandbox(CargoCommand::Test, dir.path(), |req| {
        req.cache_scope = Some("runaway".into());
        req.limits.test_timeout_secs = 5;
    });
    assert_eq!(report.status, Status::Timeout);
    assert_eq!(report.failed_phase, Some(Phase::Test));

    // The test binary lives in this run's target directory.
    let key = &report.cache.unwrap().key;
    let ps = Command::new("ps")
        .args(["-A", "-o", "command="])
        .output()
        .unwrap();
    let running = String::from_utf8_lossy(&ps.stdout);
    assert!(
        !running.lines().any(|l| l.contains(key.as_str())),
        "processes of the run are still running"
    );
}

#[test]
fn memory_limit_is_enforced() {
    let test = "#[test] fn hog() { let mut v: Vec<Vec<u8>> = Vec::new(); loop { v.push(vec![1u8; 16 << 20]); std::thread::sleep(std::time::Duration::from_millis(10)); } }\n";
    let dir = project(&[("Cargo.toml", MANIFEST), ("src/lib.rs", test)]);
    let report = sandbox(CargoCommand::Test, dir.path(), |req| {
        req.limits.memory_mb = 512
    });
    assert_eq!(report.status, Status::Oom, "{}", report.summary);
    assert_eq!(report.failed_phase, Some(Phase::Test));
}

#[test]
fn fork_bombs_hit_the_process_limit() {
    let test = r#"#[test] fn bomb() {
        let mut children = Vec::new();
        for _ in 0..500 {
            match std::process::Command::new("/bin/sleep").arg("30").spawn() {
                Ok(child) => children.push(child),
                Err(_) => break,
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(30));
    }"#;
    let dir = project(&[("Cargo.toml", MANIFEST), ("src/lib.rs", test)]);
    let report = sandbox(CargoCommand::Test, dir.path(), |req| req.limits.pids = 128);
    assert_eq!(report.status, Status::ProcessLimit, "{}", report.summary);
}

#[test]
fn cargo_config_above_the_cache_is_not_read() {
    // Cargo reads `.cargo/config.toml` from every parent directory, like the
    // user's own `~/.cargo/config.toml` above `~/.cache/isobuild`.
    let parent = tempfile::tempdir().unwrap();
    fs::create_dir(parent.path().join(".cargo")).unwrap();
    fs::write(
        parent.path().join(".cargo/config.toml"),
        "[env]\nLEAKED_FROM_HOST = \"yes\"\n",
    )
    .unwrap();
    let dir = project(&[
        ("Cargo.toml", MANIFEST),
        (
            "src/lib.rs",
            "#[test] fn clean() { assert!(option_env!(\"LEAKED_FROM_HOST\").is_none()); }\n",
        ),
    ]);
    let mut req = RunRequest::new(CargoCommand::Test, dir.path(), parent.path().join("cache"));
    req.limits.memory_mb = 2048;
    let report = run(&req, default_backend().as_ref());
    assert_eq!(report.status, Status::Ok, "{}", report.summary);
}
