//! The isolation boundary: run one command so that it can read only the
//! directories it is given, write only the directories it is given, and
//! reach the network only when asked. Implemented with Seatbelt on macOS
//! ([`crate::seatbelt`]) and bubblewrap on Linux ([`crate::bwrap`]).

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::toolchain::{self, Toolchain};

/// One command to run in a fresh sandbox.
///
/// Every path is absolute and canonical: Seatbelt matches rules against
/// resolved paths, and bubblewrap mounts each directory at its own path.
#[derive(Debug, Clone, PartialEq)]
pub struct SandboxSpec {
    /// Unique name, used in error messages.
    pub name: String,
    /// Absolute path of the program, then its arguments.
    pub command: Vec<String>,
    /// The complete environment. Nothing is inherited from the host.
    pub env: Vec<(String, String)>,
    pub workdir: PathBuf,
    /// Directories the command may read, besides the system's own libraries
    /// and tools, which each backend adds.
    pub read_only: Vec<PathBuf>,
    /// Directories the command may read and write.
    pub writable: Vec<PathBuf>,
    /// Network access. Off for everything except the fetch phase.
    pub network: bool,
    /// Combined resident memory of the sandbox's processes.
    pub memory_mb: u64,
    /// Processes running at once in the sandbox.
    pub pids: u32,
    pub timeout: Duration,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
    /// Bytes kept from each of stdout and stderr; the rest is discarded.
    pub output_cap: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SandboxOutcome {
    /// Exit code, or 128 + the signal number when the process was killed.
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub oom_killed: bool,
    pub process_limit_hit: bool,
    pub duration: Duration,
    pub output_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendError {
    /// The sandbox tool or the toolchain is missing or unusable.
    Unavailable(String),
    /// The sandbox failed to run the command. Not caused by the project.
    Failed(String),
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackendError::Unavailable(msg) | BackendError::Failed(msg) => f.write_str(msg),
        }
    }
}

pub trait Backend {
    /// Short name recorded in the report, e.g. `seatbelt`.
    fn name(&self) -> &'static str;

    /// Check that the backend can create sandboxes at all.
    fn preflight(&self) -> Result<(), BackendError>;

    /// Find the Rust toolchain on the host. `requested` is a rustup toolchain
    /// name; `None` means the host's default. `cwd` is a directory with no
    /// toolchain override, so the project cannot influence the choice.
    fn toolchain(&self, requested: Option<&str>, cwd: &Path) -> Result<Toolchain, BackendError> {
        toolchain::resolve(requested, cwd).map_err(BackendError::Unavailable)
    }

    /// Run the command to completion or until a limit is hit, writing its
    /// output to the spec's paths.
    fn run(&self, spec: &SandboxSpec) -> Result<SandboxOutcome, BackendError>;
}

/// The backend for this platform: Seatbelt on macOS, bubblewrap on Linux.
///
/// `ISOBUILD_SANDBOX_PROGRAM` replaces the path of `sandbox-exec` or `bwrap`.
pub fn default_backend() -> Box<dyn Backend> {
    let program = std::env::var_os("ISOBUILD_SANDBOX_PROGRAM").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    {
        let mut backend = crate::seatbelt::SeatbeltBackend::new();
        if let Some(program) = program {
            backend = backend.with_program(program);
        }
        Box::new(backend)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let mut backend = crate::bwrap::BubblewrapBackend::new();
        if let Some(program) = program {
            backend = backend.with_program(program);
        }
        Box::new(backend)
    }
}
