//! macOS backend: `sandbox-exec` with a Seatbelt profile generated per phase.
//!
//! The profile denies everything, then allows reading the system's libraries
//! and tools (Apple's `system.sb`, `/bin`, `/usr/bin`…) and the spec's
//! read-only directories, reading and writing the spec's writable
//! directories, and outbound network in the fetch phase only. Anything else,
//! the home directory included, cannot be read, written or connected to.
//!
//! Limits compared to bubblewrap: there is no process namespace, so a process
//! that detaches itself (double fork and `setsid`) between two watchdog
//! samples can outlive the run, still sandboxed; and loopback networking is
//! denied along with the rest, because allowing it would also reach services
//! listening on the host.

use std::collections::BTreeSet;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::backend::{Backend, BackendError, SandboxOutcome, SandboxSpec};
use crate::process::{bounded_output, last_line, supervise};

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Rules shared by every phase.
const BASE: &str = r#"(version 1)
(deny default)
(import "system.sb")
(allow process-fork)
(allow process-exec)
(allow signal (target same-sandbox))
(allow sysctl-read)
(allow file-read*
  (subpath "/bin")
  (subpath "/sbin")
  (subpath "/usr/bin")
  (subpath "/usr/sbin")
  (subpath "/usr/lib")
  (subpath "/usr/libexec")
  (subpath "/usr/share")
  (subpath "/private/var/select")
  ; Public TLS configuration and CA certificates; cargo loads them even offline.
  (subpath "/private/etc/ssl"))
(allow file-read* file-write-data
  (literal "/dev/null")
  (literal "/dev/zero")
  (literal "/dev/random")
  (literal "/dev/urandom"))
"#;

/// Rules that apply after the allowed directories: files the sandbox writes
/// must stay deletable by the host, so no immutable flags or ownership changes.
const DENY_AFTER: &str = "(deny file-write-flags file-write-owner file-write-setugid)\n";

/// Outbound network for the fetch phase, which runs cargo and nothing else.
/// DNS and TLS certificate checks go through system services.
const NETWORK: &str = r#"(allow network-outbound)
(allow system-socket)
(allow mach-lookup)
(allow file-read*
  (subpath "/Library/Keychains")
  (subpath "/private/var/db/mds")
  (subpath "/private/var/run")
  (literal "/private/etc/hosts")
  (literal "/private/etc/resolv.conf"))
"#;

#[derive(Debug, Clone)]
pub struct SeatbeltBackend {
    program: PathBuf,
}

impl Default for SeatbeltBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl SeatbeltBackend {
    pub fn new() -> Self {
        SeatbeltBackend {
            program: PathBuf::from(SANDBOX_EXEC),
        }
    }

    /// Use `program` instead of `/usr/bin/sandbox-exec`.
    pub fn with_program(mut self, program: impl Into<PathBuf>) -> Self {
        self.program = program.into();
        self
    }
}

impl Backend for SeatbeltBackend {
    fn name(&self) -> &'static str {
        "seatbelt"
    }

    fn preflight(&self) -> Result<(), BackendError> {
        let mut probe = Command::new(&self.program);
        probe.args(["-p", "(version 1)(allow default)", "/usr/bin/true"]);
        match bounded_output(probe, Duration::from_secs(10)) {
            Ok(out) if out.status.success() => Ok(()),
            Ok(out) => Err(BackendError::Unavailable(format!(
                "`{}` cannot create sandboxes: {}",
                self.program.display(),
                last_line(&out.stderr)
            ))),
            Err(err) => Err(BackendError::Unavailable(err)),
        }
    }

    fn run(&self, spec: &SandboxSpec) -> Result<SandboxOutcome, BackendError> {
        let mut command = Command::new(&self.program);
        command.arg("-p").arg(profile(spec)?).args(&spec.command);
        supervise(command, spec, "sandbox-exec:")
    }
}

/// The Seatbelt profile for `spec`. Pure, so the rules can be tested.
pub fn profile(spec: &SandboxSpec) -> Result<String, BackendError> {
    let mut profile = String::from(BASE);

    // Path lookups stat every parent directory: allow their metadata, not
    // their contents.
    let mut parents = BTreeSet::new();
    for path in spec
        .read_only
        .iter()
        .chain(&spec.writable)
        .chain([&spec.workdir])
    {
        parents.extend(path.ancestors().skip(1).map(Path::to_path_buf));
    }
    rule(&mut profile, "file-read-metadata", "literal", &parents)?;
    rule(&mut profile, "file-read*", "subpath", &spec.read_only)?;
    rule(
        &mut profile,
        "file-read* file-write*",
        "subpath",
        &spec.writable,
    )?;
    profile.push_str(DENY_AFTER);
    if spec.network {
        profile.push_str(NETWORK);
    }
    Ok(profile)
}

fn rule<'a>(
    profile: &mut String,
    operations: &str,
    filter: &str,
    paths: impl IntoIterator<Item = &'a PathBuf>,
) -> Result<(), BackendError> {
    let mut filters = String::new();
    for path in paths {
        let _ = write!(filters, "\n  ({filter} {})", quote(path)?);
    }
    if !filters.is_empty() {
        let _ = writeln!(profile, "(allow {operations}{filters})");
    }
    Ok(())
}

/// A profile string literal. Paths must be absolute: Seatbelt matches
/// resolved paths, and a relative one would match nothing.
fn quote(path: &Path) -> Result<String, BackendError> {
    let text = path
        .to_str()
        .filter(|t| path.is_absolute() && !t.chars().any(char::is_control))
        .ok_or_else(|| {
            BackendError::Failed(format!(
                "cannot sandbox the path {}: it must be absolute, valid UTF-8 and free of control characters",
                path.display()
            ))
        })?;
    Ok(format!(
        "\"{}\"",
        text.replace('\\', "\\\\").replace('"', "\\\"")
    ))
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
            read_only: vec!["/toolchain".into(), "/cache/cargo-home/registry".into()],
            writable: vec![
                "/cache/runs/1/work".into(),
                "/cache/targets/k/target".into(),
            ],
            network,
            memory_mb: 1024,
            pids: 64,
            timeout: Duration::from_secs(1),
            stdout_path: "/dev/null".into(),
            stderr_path: "/dev/null".into(),
            output_cap: 0,
        }
    }

    #[test]
    fn profile_denies_by_default_and_allows_only_the_spec() {
        let profile = profile(&spec(false)).unwrap();
        assert!(profile.starts_with("(version 1)\n(deny default)"));
        assert!(profile.contains("(allow file-read*\n  (subpath \"/toolchain\")"));
        assert!(profile.contains(
            "(allow file-read* file-write*\n  (subpath \"/cache/runs/1/work\")\n  (subpath \"/cache/targets/k/target\"))"
        ));
        // Parents are visible as metadata only.
        assert!(profile.contains("(literal \"/cache/runs\")"));
        assert!(!profile.contains("(subpath \"/cache\")"));
        assert!(profile.contains("(deny file-write-flags"));
        assert!(!profile.contains("network"));
    }

    #[test]
    fn network_is_only_allowed_when_asked() {
        assert!(
            profile(&spec(true))
                .unwrap()
                .contains("(allow network-outbound)")
        );
    }

    #[test]
    fn paths_are_quoted_or_refused() {
        let mut spec = spec(false);
        spec.writable.push(r#"/odd/"quote"\dir"#.into());
        assert!(
            profile(&spec)
                .unwrap()
                .contains(r#"(subpath "/odd/\"quote\"\\dir")"#)
        );

        spec.writable.push("/new\nline".into());
        assert!(profile(&spec).is_err());
        spec.writable.pop();
        spec.read_only.push("relative/path".into());
        assert!(profile(&spec).is_err());
    }
}
