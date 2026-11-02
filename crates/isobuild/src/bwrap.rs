//! Linux backend: bubblewrap with every namespace unshared.
//!
//! The sandbox starts from an empty root. The system directories (`/usr`,
//! its compatibility links, a few files from `/etc`) are mounted read-only,
//! the spec's directories at their own paths, plus fresh `/proc`, `/dev` and
//! `/tmp`, on a root that is read-only once they are in place. Nothing else
//! from the host exists inside: the home directory is not unreadable but
//! absent. The network namespace is empty (loopback only)
//! except in the fetch phase, and the PID namespace means killing bubblewrap
//! kills everything the command started.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::backend::{Backend, BackendError, SandboxOutcome, SandboxSpec};
use crate::process::{bounded_output, last_line, supervise};

/// Top-level directories that are either links into `/usr` or real
/// directories, depending on the distribution.
const SYSTEM_DIRS: [&str; 6] = ["/bin", "/sbin", "/lib", "/lib32", "/lib64", "/libx32"];

/// Host configuration needed to run programs: the dynamic linker's cache and
/// Debian's alternatives (`cc` is a link through `/etc/alternatives`).
const ETC: [&str; 4] = [
    "/etc/ld.so.cache",
    "/etc/ld.so.conf",
    "/etc/ld.so.conf.d",
    "/etc/alternatives",
];

/// Host configuration for name resolution and TLS, fetch phase only.
const ETC_NETWORK: [&str; 9] = [
    "/etc/resolv.conf",
    "/etc/hosts",
    "/etc/nsswitch.conf",
    "/etc/host.conf",
    "/etc/gai.conf",
    "/etc/ssl",
    "/etc/pki",
    "/etc/ca-certificates",
    "/etc/crypto-policies",
];

#[derive(Debug, Clone)]
pub struct BubblewrapBackend {
    program: PathBuf,
}

impl Default for BubblewrapBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl BubblewrapBackend {
    /// Uses `bwrap` from `PATH`.
    pub fn new() -> Self {
        BubblewrapBackend {
            program: PathBuf::from("bwrap"),
        }
    }

    pub fn with_program(mut self, program: impl Into<PathBuf>) -> Self {
        self.program = program.into();
        self
    }
}

impl Backend for BubblewrapBackend {
    fn name(&self) -> &'static str {
        "bubblewrap"
    }

    fn preflight(&self) -> Result<(), BackendError> {
        let mut probe = Command::new(&self.program);
        probe.args(system_args(false)).args(["--", "/usr/bin/true"]);
        match bounded_output(probe, Duration::from_secs(10)) {
            Ok(out) if out.status.success() => Ok(()),
            Ok(out) => Err(BackendError::Unavailable(format!(
                "bubblewrap cannot create sandboxes: {}; unprivileged user namespaces may be disabled (on Ubuntu 24.04 and later, install the distribution's `bubblewrap` package, which ships the AppArmor profile that allows them)",
                last_line(&out.stderr)
            ))),
            Err(_) => Err(BackendError::Unavailable(format!(
                "`{}` was not found; install bubblewrap (e.g. `apt install bubblewrap` or `dnf install bubblewrap`)",
                self.program.display()
            ))),
        }
    }

    fn run(&self, spec: &SandboxSpec) -> Result<SandboxOutcome, BackendError> {
        let mut command = Command::new(&self.program);
        command.args(args(spec)?);
        supervise(command, spec, "bwrap:")
    }
}

/// Arguments for `bwrap`. Reads the host's `/` to mirror its layout.
pub fn args(spec: &SandboxSpec) -> Result<Vec<String>, BackendError> {
    let mut args = system_args(spec.network);
    for path in &spec.read_only {
        let path = utf8(path)?;
        args.extend(["--ro-bind".into(), path.clone(), path]);
    }
    for path in &spec.writable {
        let path = utf8(path)?;
        args.extend(["--bind".into(), path.clone(), path]);
    }
    // The root is a tmpfs holding the mount points' parents. Once everything
    // is mounted, make it read-only; the mounts on it keep their own modes.
    args.extend(["--remount-ro".into(), "/".into()]);
    args.extend(["--chdir".into(), utf8(&spec.workdir)?, "--".into()]);
    args.extend(spec.command.iter().cloned());
    Ok(args)
}

/// Namespaces and the system part of the filesystem.
fn system_args(network: bool) -> Vec<String> {
    let mut args: Vec<String> = [
        "--unshare-all",
        // Killed with the CLI; and no controlling terminal to inject input into.
        "--die-with-parent",
        "--new-session",
        "--hostname",
        "sandbox",
    ]
    .map(String::from)
    .to_vec();
    if network {
        args.push("--share-net".into());
    }
    args.extend(["--ro-bind", "/usr", "/usr"].map(String::from));
    for dir in SYSTEM_DIRS {
        match std::fs::symlink_metadata(dir) {
            Ok(meta) if meta.is_symlink() => {
                if let Ok(target) = std::fs::read_link(dir) {
                    args.extend(["--symlink".into(), target.display().to_string(), dir.into()]);
                }
            }
            Ok(meta) if meta.is_dir() => args.extend(["--ro-bind", dir, dir].map(String::from)),
            _ => {}
        }
    }
    let etc = ETC
        .iter()
        .chain(if network { &ETC_NETWORK[..] } else { &[] });
    for path in etc {
        args.extend(["--ro-bind-try", path, path].map(String::from));
    }
    // `/tmp` comes before the spec's mounts, which may live under it.
    args.extend(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"].map(String::from));
    args
}

fn utf8(path: &Path) -> Result<String, BackendError> {
    path.to_str()
        .filter(|_| path.is_absolute())
        .map(String::from)
        .ok_or_else(|| {
            BackendError::Failed(format!(
                "cannot sandbox the path {}: it must be absolute and valid UTF-8",
                path.display()
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(network: bool) -> SandboxSpec {
        SandboxSpec {
            name: "test".into(),
            command: vec!["/toolchain/bin/cargo".into(), "build".into()],
            env: Vec::new(),
            workdir: "/cache/runs/1/work".into(),
            read_only: vec!["/toolchain".into()],
            writable: vec!["/cache/runs/1/work".into()],
            network,
            memory_mb: 1024,
            pids: 64,
            timeout: Duration::from_secs(1),
            stdout_path: "/dev/null".into(),
            stderr_path: "/dev/null".into(),
            output_cap: 0,
        }
    }

    fn has(args: &[String], seq: &[&str]) -> bool {
        args.windows(seq.len()).any(|w| w == seq)
    }

    #[test]
    fn everything_is_unshared_and_offline_by_default() {
        let args = args(&spec(false)).unwrap();
        assert!(has(&args, &["--unshare-all"]));
        assert!(has(&args, &["--die-with-parent"]));
        assert!(!args.iter().any(|a| a == "--share-net"));
        assert!(!args.iter().any(|a| a == "/etc/resolv.conf"));
        assert!(has(&args, &["--ro-bind", "/usr", "/usr"]));
    }

    #[test]
    fn the_spec_is_mounted_at_its_own_paths() {
        let args = args(&spec(true)).unwrap();
        assert!(has(&args, &["--share-net"]));
        assert!(has(
            &args,
            &["--ro-bind-try", "/etc/resolv.conf", "/etc/resolv.conf"]
        ));
        assert!(has(&args, &["--ro-bind", "/toolchain", "/toolchain"]));
        assert!(has(
            &args,
            &["--bind", "/cache/runs/1/work", "/cache/runs/1/work"]
        ));
        assert!(has(
            &args,
            &[
                "--chdir",
                "/cache/runs/1/work",
                "--",
                "/toolchain/bin/cargo",
                "build"
            ]
        ));
        let tmpfs = args.iter().position(|a| a == "--tmpfs").unwrap();
        let bind = args.iter().position(|a| a == "--bind").unwrap();
        assert!(tmpfs < bind, "a fresh /tmp must not hide mounts under it");
        let remount = args.iter().position(|a| a == "--remount-ro").unwrap();
        assert!(
            bind < remount,
            "the root is made read-only after the mounts"
        );
        assert_eq!(args[remount + 1], "/");
    }

    #[test]
    fn relative_paths_are_refused() {
        let mut spec = spec(false);
        spec.writable.push("relative".into());
        assert!(args(&spec).is_err());
    }
}
