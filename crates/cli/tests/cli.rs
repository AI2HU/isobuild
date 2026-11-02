//! The binary's contract: JSON on stdout and documented exit codes. These
//! cases stop before any sandbox starts.

use std::fs;
use std::process::{Command, Output};

fn cargo_sandbox(args: &[&str], cache: &std::path::Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cargo-sandbox"))
        .args(args)
        .env("ISOBUILD_CACHE", cache)
        // Make sure nothing runs even if a case goes further than expected.
        .env("ISOBUILD_SANDBOX_PROGRAM", "/nonexistent/sandbox")
        .output()
        .unwrap()
}

fn json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn project_with_config(config: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"demo\"\n",
    )
    .unwrap();
    fs::create_dir(dir.path().join(".cargo")).unwrap();
    fs::write(dir.path().join(".cargo/config.toml"), config).unwrap();
    dir
}

#[test]
fn policy_denial_exits_2_with_a_json_report() {
    let project = project_with_config("[build]\nrustc = \"./evil\"\n");
    let cache = tempfile::tempdir().unwrap();
    let out = cargo_sandbox(
        &["test", "--project", project.path().to_str().unwrap()],
        cache.path(),
    );
    assert_eq!(out.status.code(), Some(2));
    let report = json(&out);
    assert_eq!(report["schema_version"], 2);
    assert_eq!(report["status"], "policy_denied");
    assert_eq!(report["failed_phase"], "prepare");
    assert_eq!(report["policy"][0]["rule"], "cargo-config.custom-rustc");
}

#[test]
fn works_as_a_cargo_subcommand() {
    // `cargo sandbox check` invokes `cargo-sandbox sandbox check`.
    let project = project_with_config("[build]\nrustc = \"./evil\"\n");
    let cache = tempfile::tempdir().unwrap();
    let out = cargo_sandbox(
        &[
            "sandbox",
            "check",
            "--project",
            project.path().to_str().unwrap(),
        ],
        cache.path(),
    );
    assert_eq!(json(&out)["command"], "check");
}

#[test]
fn missing_sandbox_tool_exits_4() {
    let project = project_with_config("[build]\njobs = 2\n");
    let cache = tempfile::tempdir().unwrap();
    let out = cargo_sandbox(
        &["check", "--project", project.path().to_str().unwrap()],
        cache.path(),
    );
    assert_eq!(out.status.code(), Some(4));
    let report = json(&out);
    assert_eq!(report["status"], "infra_error");
    assert!(
        report["summary"]
            .as_str()
            .unwrap()
            .contains("/nonexistent/sandbox")
    );
}

#[test]
fn missing_manifest_exits_2() {
    let empty = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let out = cargo_sandbox(
        &["build", "--project", empty.path().to_str().unwrap()],
        cache.path(),
    );
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(json(&out)["status"], "invalid_request");
}

#[test]
fn human_format_prints_the_status_line() {
    let project = project_with_config("[build]\nrustc = \"./evil\"\n");
    let cache = tempfile::tempdir().unwrap();
    let out = cargo_sandbox(
        &[
            "check",
            "--format",
            "human",
            "--project",
            project.path().to_str().unwrap(),
        ],
        cache.path(),
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("policy denied [cargo-config.custom-rustc]"));
    assert!(text.contains("policy_denied: "));
}

#[test]
fn clean_refuses_directories_it_does_not_own() {
    let not_a_cache = tempfile::tempdir().unwrap();
    fs::write(not_a_cache.path().join("precious.txt"), "keep me").unwrap();
    let out = cargo_sandbox(
        &["clean", "--cache-dir", not_a_cache.path().to_str().unwrap()],
        not_a_cache.path(),
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(not_a_cache.path().join("precious.txt").exists());
}

#[test]
fn clean_removes_a_cache() {
    let parent = tempfile::tempdir().unwrap();
    let cache = parent.path().join("cache");
    cargo_sandbox_layout(&cache);
    let out = cargo_sandbox(&["clean"], &cache);
    assert_eq!(out.status.code(), Some(0));
    assert!(!cache.exists());
}

fn cargo_sandbox_layout(root: &std::path::Path) {
    isobuild::cache::CacheLayout::new(root).ensure().unwrap();
}
