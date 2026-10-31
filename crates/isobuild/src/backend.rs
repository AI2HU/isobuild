//! The isolation boundary, abstracted so other backends (gVisor, microVMs,
//! hosted sandboxes) can replace Docker without touching the run logic.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

/// Where a mount's data lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountSource {
    /// A directory on the host.
    Host(PathBuf),
    /// A volume managed by the backend. Used for build output: on macOS,
    /// heavy writes to host directories through Docker Desktop's file
    /// sharing are slow and can hang the linker.
    Volume(String),
}

/// A directory made visible inside the container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub source: MountSource,
    pub container: String,
    pub read_only: bool,
}

impl Mount {
    pub fn host(path: impl Into<PathBuf>, container: impl Into<String>, read_only: bool) -> Self {
        Mount {
            source: MountSource::Host(path.into()),
            container: container.into(),
            read_only,
        }
    }

    pub fn volume(name: impl Into<String>, container: impl Into<String>) -> Self {
        Mount {
            source: MountSource::Volume(name.into()),
            container: container.into(),
            read_only: false,
        }
    }

    /// The host directory, for host mounts.
    pub fn host_path(&self) -> Option<&std::path::Path> {
        match &self.source {
            MountSource::Host(path) => Some(path),
            MountSource::Volume(_) => None,
        }
    }
}

/// One process to run in a fresh, isolated environment.
#[derive(Debug, Clone, PartialEq)]
pub struct ContainerSpec {
    /// Unique name, used to kill and clean up the container.
    pub name: String,
    pub image: String,
    pub command: Vec<String>,
    /// The complete environment added to the image's. Nothing is inherited from the host.
    pub env: Vec<(String, String)>,
    pub mounts: Vec<Mount>,
    /// In-memory writable directories: (path, size in MB).
    pub tmpfs: Vec<(String, u64)>,
    /// Network access. Off for everything except the fetch phase.
    pub network: bool,
    pub workdir: String,
    /// `uid:gid` to run as, so files written to mounts belong to the host user.
    pub user: Option<String>,
    pub memory_mb: u64,
    pub cpus: f64,
    pub pids: u32,
    pub timeout: Duration,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
    /// Bytes kept from each of stdout and stderr; the rest is discarded.
    pub output_cap: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerOutcome {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub oom_killed: bool,
    pub duration: Duration,
    pub output_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendError {
    /// The backend is not installed or not running.
    Unavailable(String),
    /// The backend failed to run the container. Not caused by the project.
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
    /// Check that the backend can run containers at all.
    fn preflight(&self) -> Result<(), BackendError>;

    /// Make `image` available locally and return its content-addressed id.
    fn ensure_image(&self, image: &str) -> Result<String, BackendError>;

    /// Create the volume `name` if needed, writable by `owner` (`uid:gid`).
    /// Returns true if it already existed.
    fn ensure_volume(
        &self,
        name: &str,
        owner: Option<&str>,
        image: &str,
    ) -> Result<bool, BackendError>;

    /// Delete the volume `name`. Best effort.
    fn remove_volume(&self, name: &str);

    /// Run the container to completion or until its timeout, writing its
    /// output to the spec's paths.
    fn run(&self, spec: &ContainerSpec) -> Result<ContainerOutcome, BackendError>;
}
