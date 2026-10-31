//! What a caller asks the sandbox to do.

use std::path::PathBuf;

use isobuild_report::Limits;

/// Image used when the caller does not choose one. Pin it by digest in
/// production (`rust@sha256:…`) so the toolchain cannot change under you.
pub const DEFAULT_IMAGE: &str = "rust:1.91.1-slim-bookworm";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CargoCommand {
    Check,
    Clippy,
    Build,
    Test,
}

impl CargoCommand {
    pub fn as_str(self) -> &'static str {
        match self {
            CargoCommand::Check => "check",
            CargoCommand::Clippy => "clippy",
            CargoCommand::Build => "build",
            CargoCommand::Test => "test",
        }
    }
}

/// The cargo flags a caller may set. Arbitrary flags are not accepted, so a
/// request cannot move the target directory, change the config, or reach
/// outside the staged project.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CargoOptions {
    pub release: bool,
    pub features: Vec<String>,
    pub all_features: bool,
    pub no_default_features: bool,
    pub packages: Vec<String>,
    pub workspace: bool,
    pub all_targets: bool,
    /// Fail instead of updating `Cargo.lock`.
    pub locked: bool,
}

impl CargoOptions {
    /// Flags for build and test commands. Values are joined with `=` so they
    /// can never be read as another flag.
    pub(crate) fn to_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if self.release {
            args.push("--release".into());
        }
        if !self.features.is_empty() {
            args.push(format!("--features={}", self.features.join(",")));
        }
        if self.all_features {
            args.push("--all-features".into());
        }
        if self.no_default_features {
            args.push("--no-default-features".into());
        }
        for package in &self.packages {
            args.push(format!("--package={package}"));
        }
        if self.workspace {
            args.push("--workspace".into());
        }
        if self.all_targets {
            args.push("--all-targets".into());
        }
        args
    }
}

#[derive(Debug, Clone)]
pub struct RunRequest {
    pub command: CargoCommand,
    /// Directory containing the project's root `Cargo.toml`. It is copied, never mounted.
    pub project: PathBuf,
    pub cargo: CargoOptions,
    /// Arguments for the test binaries (`cargo test -- <args>`). Test command only.
    pub test_args: Vec<String>,
    /// Extra environment for the build and test phases. Never inherited from the host.
    pub env: Vec<(String, String)>,
    pub image: String,
    pub limits: Limits,
    /// Skip the fetch phase and build with `--offline` from what is already cached.
    pub offline: bool,
    pub cache_dir: PathBuf,
    /// Runs with the same scope share compiled artifacts. Defaults to the
    /// project's canonical path, so each project gets its own build cache.
    pub cache_scope: Option<String>,
    /// When false, every run compiles from scratch in a throwaway target directory.
    pub build_cache: bool,
    /// Keep the staged copy of the project after the run, for debugging.
    pub keep_staging: bool,
    pub max_diagnostics: usize,
    pub max_test_failures: usize,
    /// Refuse projects larger than this, excluding `target/` directories.
    pub max_project_bytes: u64,
    /// Cap on the stdout and stderr captured from each phase, in bytes.
    pub max_output_bytes: u64,
}

impl RunRequest {
    pub fn new(
        command: CargoCommand,
        project: impl Into<PathBuf>,
        cache_dir: impl Into<PathBuf>,
    ) -> Self {
        RunRequest {
            command,
            project: project.into(),
            cargo: CargoOptions::default(),
            test_args: Vec::new(),
            env: Vec::new(),
            image: DEFAULT_IMAGE.to_string(),
            limits: Limits::default(),
            offline: false,
            cache_dir: cache_dir.into(),
            cache_scope: None,
            build_cache: true,
            keep_staging: false,
            max_diagnostics: 50,
            max_test_failures: 50,
            max_project_bytes: 512 * 1024 * 1024,
            max_output_bytes: 32 * 1024 * 1024,
        }
    }

    /// Reject requests that would break the sandbox's own invariants.
    pub(crate) fn validate(&self) -> Result<(), String> {
        if !self.project.join("Cargo.toml").is_file() {
            return Err(format!("no Cargo.toml in {}", self.project.display()));
        }
        if !self.test_args.is_empty() && self.command != CargoCommand::Test {
            return Err("test arguments are only accepted by the test command".into());
        }
        for (name, _) in &self.env {
            if !is_env_name(name) {
                return Err(format!("invalid environment variable name `{name}`"));
            }
            if RESERVED_ENV.contains(&name.as_str()) || name.starts_with("RUSTUP_") {
                return Err(format!(
                    "environment variable `{name}` is set by the sandbox and cannot be overridden"
                ));
            }
        }
        let limits = &self.limits;
        if limits.memory_mb < 256 {
            return Err("memory limit must be at least 256 MB".into());
        }
        if limits.cpus <= 0.0 || limits.cpus.is_nan() {
            return Err("cpu limit must be positive".into());
        }
        if limits.pids < 64 {
            return Err("process limit must be at least 64".into());
        }
        if limits.fetch_timeout_secs == 0
            || limits.build_timeout_secs == 0
            || limits.test_timeout_secs == 0
        {
            return Err("timeouts must be at least one second".into());
        }
        Ok(())
    }
}

const RESERVED_ENV: [&str; 7] = [
    "CARGO_HOME",
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_TARGET_DIR",
    "CARGO_NET_OFFLINE",
    "HOME",
    "PATH",
    "TMPDIR",
];

fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> (tempfile::TempDir, RunRequest) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        let req = RunRequest::new(CargoCommand::Test, dir.path(), dir.path().join("cache"));
        (dir, req)
    }

    #[test]
    fn options_become_flags_that_cannot_be_split() {
        let options = CargoOptions {
            release: true,
            features: vec!["a".into(), "b c".into()],
            packages: vec!["-evil".into()],
            all_targets: true,
            ..Default::default()
        };
        assert_eq!(
            options.to_args(),
            [
                "--release",
                "--features=a,b c",
                "--package=-evil",
                "--all-targets"
            ]
        );
    }

    #[test]
    fn valid_request_passes() {
        let (_dir, mut req) = request();
        req.env.push(("RUST_LOG".into(), "debug".into()));
        req.test_args.push("--exact".into());
        assert_eq!(req.validate(), Ok(()));
    }

    #[test]
    fn missing_manifest_is_rejected() {
        let (dir, mut req) = request();
        req.project = dir.path().join("nope");
        assert!(req.validate().unwrap_err().contains("no Cargo.toml"));
    }

    #[test]
    fn reserved_and_malformed_env_is_rejected() {
        for name in [
            "CARGO_TARGET_DIR",
            "RUSTUP_TOOLCHAIN",
            "PATH",
            "1BAD",
            "A=B",
            "",
        ] {
            let (_dir, mut req) = request();
            req.env.push((name.into(), "x".into()));
            assert!(req.validate().is_err(), "{name} should be rejected");
        }
    }

    #[test]
    fn test_args_need_the_test_command() {
        let (_dir, mut req) = request();
        req.command = CargoCommand::Build;
        req.test_args.push("--exact".into());
        assert!(req.validate().is_err());
    }

    #[test]
    fn silly_limits_are_rejected() {
        let (_dir, mut req) = request();
        req.limits.memory_mb = 16;
        assert!(req.validate().is_err());
        let (_dir, mut req) = request();
        req.limits.test_timeout_secs = 0;
        assert!(req.validate().is_err());
    }
}
