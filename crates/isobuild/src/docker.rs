//! Docker backend: one hardened `docker run` per phase.
//!
//! Every container gets a read-only root filesystem, no capabilities,
//! `no-new-privileges`, memory/CPU/process limits, an init process to reap
//! children, and no network unless the spec asks for it. Containers are not
//! started with `--rm` so their OOM state can be inspected; a guard removes
//! them afterwards.
//!
//! Every docker command has a deadline. A stuck daemon (it happens, e.g. when
//! Docker Desktop's file sharing hangs) produces an `infra_error`, never a
//! CLI that waits forever.

use std::fs::File;
use std::io::{self, Read, Write};
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::backend::{Backend, BackendError, ContainerOutcome, ContainerSpec, MountSource};

/// Deadline for inspect, create, kill and rm commands.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
/// Deadline for pulling an image.
const PULL_TIMEOUT: Duration = Duration::from_secs(900);
/// How long to wait for output pipes to close once `docker run` has exited.
/// The docker CLI can leave a helper process holding them open.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Containers currently running, so an interrupted CLI can remove them.
static ACTIVE: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// Force-remove every container this process started that is still running.
pub fn remove_active_containers() {
    let active: Vec<_> = ACTIVE
        .lock()
        .map(|mut a| a.drain(..).collect())
        .unwrap_or_default();
    for (program, name) in active {
        let _ = bounded_output(&program, &["rm", "--force", &name], COMMAND_TIMEOUT);
    }
}

/// Run `program args`, collecting its output, and give up after `timeout`.
fn bounded_output(program: &str, args: &[&str], timeout: Duration) -> Result<Output, BackendError> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| BackendError::Unavailable(format!("could not run `{program}`: {err}")))?;
    let stdout = read_in_background(child.stdout.take().expect("stdout is piped"));
    let stderr = read_in_background(child.stderr.take().expect("stderr is piped"));

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < timeout => thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(BackendError::Failed(format!(
                    "`{program} {}` did not finish within {}s; the Docker daemon may be stuck",
                    args.first().copied().unwrap_or_default(),
                    timeout.as_secs()
                )));
            }
            Err(err) => {
                return Err(BackendError::Failed(format!(
                    "lost track of `{program}`: {err}"
                )));
            }
        }
    };
    Ok(Output {
        status,
        stdout: stdout.recv_timeout(DRAIN_TIMEOUT).unwrap_or_default(),
        stderr: stderr.recv_timeout(DRAIN_TIMEOUT).unwrap_or_default(),
    })
}

fn read_in_background(mut reader: impl Read + Send + 'static) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = reader.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    rx
}

#[derive(Debug, Clone)]
pub struct DockerBackend {
    program: String,
}

impl Default for DockerBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl DockerBackend {
    /// Uses `$ISOBUILD_DOCKER` as the docker binary when set (e.g. `podman`).
    pub fn new() -> Self {
        let program = std::env::var("ISOBUILD_DOCKER").unwrap_or_else(|_| "docker".into());
        DockerBackend { program }
    }

    /// Use `program` as the docker binary.
    pub fn with_program(program: impl Into<String>) -> Self {
        DockerBackend {
            program: program.into(),
        }
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.stdin(Stdio::null());
        cmd
    }

    fn output(&self, args: &[&str]) -> Result<Output, BackendError> {
        bounded_output(&self.program, args, COMMAND_TIMEOUT)
    }
}

impl Backend for DockerBackend {
    fn preflight(&self) -> Result<(), BackendError> {
        let out = self.output(&["version", "--format", "{{.Server.Version}}"])?;
        if out.status.success() {
            Ok(())
        } else {
            Err(BackendError::Unavailable(format!(
                "the Docker daemon is not reachable; start Docker and retry ({})",
                last_line(&String::from_utf8_lossy(&out.stderr))
            )))
        }
    }

    fn ensure_image(&self, image: &str) -> Result<String, BackendError> {
        let inspect = || self.output(&["image", "inspect", "--format", "{{.Id}}", image]);
        let out = inspect()?;
        if out.status.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
        }
        let pull = bounded_output(&self.program, &["pull", "--quiet", image], PULL_TIMEOUT)?;
        if !pull.status.success() {
            return Err(BackendError::Failed(format!(
                "could not pull image `{image}`: {}",
                last_line(&String::from_utf8_lossy(&pull.stderr))
            )));
        }
        let out = inspect()?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            Err(BackendError::Failed(format!(
                "image `{image}` is not available after pulling"
            )))
        }
    }

    fn ensure_volume(
        &self,
        name: &str,
        owner: Option<&str>,
        image: &str,
    ) -> Result<bool, BackendError> {
        check_volume_name(name)?;
        if self.output(&["volume", "inspect", name])?.status.success() {
            return Ok(true);
        }
        let created = self.output(&["volume", "create", "--label", "isobuild=1", name])?;
        if !created.status.success() {
            return Err(BackendError::Failed(format!(
                "could not create volume `{name}`: {}",
                last_line(&String::from_utf8_lossy(&created.stderr))
            )));
        }
        if let Some(owner) = owner {
            // New volumes belong to root. Hand this one to the sandbox user
            // with the image's own chown; no project code is involved.
            let mount = format!("type=volume,source={name},target=/volume");
            let container = format!("{name}-chown");
            let chown = self.output(&[
                "run",
                "--rm",
                "--name",
                &container,
                "--network",
                "none",
                "--user",
                "0",
                "--label",
                "isobuild=1",
                "--mount",
                &mount,
                image,
                "chown",
                owner,
                "/volume",
            ]);
            if !chown.as_ref().is_ok_and(|out| out.status.success()) {
                let _ = self.output(&["rm", "--force", &container]);
                self.remove_volume(name);
                let reason = match chown {
                    Ok(out) => last_line(&String::from_utf8_lossy(&out.stderr)).to_string(),
                    Err(err) => err.to_string(),
                };
                return Err(BackendError::Failed(format!(
                    "could not prepare volume `{name}`: {reason}"
                )));
            }
        }
        Ok(false)
    }

    fn remove_volume(&self, name: &str) {
        let _ = self.output(&["volume", "rm", "--force", name]);
    }

    fn run(&self, spec: &ContainerSpec) -> Result<ContainerOutcome, BackendError> {
        let io_err = |err: io::Error| BackendError::Failed(format!("could not write logs: {err}"));
        let stdout_file = File::create(&spec.stdout_path).map_err(io_err)?;
        let stderr_file = File::create(&spec.stderr_path).map_err(io_err)?;

        let args = run_args(spec)?;
        let mut child = self
            .command()
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| {
                BackendError::Unavailable(format!("could not run `{}`: {err}", self.program))
            })?;
        let _guard = ContainerGuard::register(&self.program, &spec.name);

        let cap = spec.output_cap;
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        let stdout_done = capture_in_background(stdout, stdout_file, cap);
        let stderr_done = capture_in_background(stderr, stderr_file, cap);

        let started = Instant::now();
        let mut timed_out = false;
        let wait_err =
            |err: io::Error| BackendError::Failed(format!("lost track of docker: {err}"));
        loop {
            if child.try_wait().map_err(wait_err)?.is_some() {
                break;
            }
            if started.elapsed() >= spec.timeout {
                timed_out = true;
                let _ = self.output(&["kill", &spec.name]);
                // `docker run` exits once the container is gone; kill the
                // client too in case the daemon never saw the container.
                let _ = child.kill();
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        child.wait().map_err(wait_err)?;
        let duration = started.elapsed();
        // A pipe that never closes counts as truncated output.
        let output_truncated = stdout_done.recv_timeout(DRAIN_TIMEOUT).unwrap_or(true)
            | stderr_done.recv_timeout(DRAIN_TIMEOUT).unwrap_or(true);

        let state = self.output(&[
            "inspect",
            "--format",
            "{{.State.OOMKilled}}|{{.State.ExitCode}}|{{.State.StartedAt}}",
            &spec.name,
        ])?;
        let state = String::from_utf8_lossy(&state.stdout).trim().to_string();
        let mut fields = state.split('|');
        let (Some(oom), Some(code), Some(started_at)) =
            (fields.next(), fields.next(), fields.next())
        else {
            return Err(BackendError::Failed(format!(
                "docker could not create the container: {}",
                tail_of(&spec.stderr_path)
            )));
        };
        if started_at.starts_with("0001-") {
            return Err(BackendError::Failed(format!(
                "docker could not start the container: {}",
                tail_of(&spec.stderr_path)
            )));
        }

        Ok(ContainerOutcome {
            exit_code: code.parse().ok(),
            timed_out,
            oom_killed: oom == "true",
            duration,
            output_truncated,
        })
    }
}

/// Arguments for `docker run`. Pure, so the hardening can be tested.
pub fn run_args(spec: &ContainerSpec) -> Result<Vec<String>, BackendError> {
    let mut args: Vec<String> = [
        "run",
        "--name",
        &spec.name,
        "--label",
        "isobuild=1",
        "--init",
        "--read-only",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--network",
        if spec.network { "bridge" } else { "none" },
        "--ipc",
        "private",
        "--hostname",
        "sandbox",
        "--workdir",
        &spec.workdir,
    ]
    .into_iter()
    .map(String::from)
    .collect();

    let memory = format!("{}m", spec.memory_mb);
    args.extend([
        "--memory".into(),
        memory.clone(),
        "--memory-swap".into(),
        memory,
    ]);
    args.extend(["--cpus".into(), format!("{}", spec.cpus)]);
    args.extend(["--pids-limit".into(), spec.pids.to_string()]);
    args.extend(["--ulimit".into(), "nofile=4096:4096".into()]);
    if let Some(user) = &spec.user {
        args.extend(["--user".into(), user.clone()]);
    }
    for (path, size_mb) in &spec.tmpfs {
        args.extend([
            "--tmpfs".into(),
            format!("{path}:rw,exec,nosuid,nodev,size={size_mb}m,mode=1777"),
        ]);
    }
    for mount in &spec.mounts {
        let mut value = match &mount.source {
            MountSource::Host(path) => {
                let host = path.to_str().ok_or_else(|| {
                    BackendError::Failed(format!(
                        "mount path is not valid UTF-8: {}",
                        path.display()
                    ))
                })?;
                if host.contains(',') || mount.container.contains(',') {
                    return Err(BackendError::Failed(format!(
                        "mount paths cannot contain commas: {host}"
                    )));
                }
                format!("type=bind,source={host},target={}", mount.container)
            }
            MountSource::Volume(name) => {
                check_volume_name(name)?;
                format!("type=volume,source={name},target={}", mount.container)
            }
        };
        if mount.read_only {
            value.push_str(",readonly");
        }
        args.extend(["--mount".into(), value]);
    }
    for (key, value) in &spec.env {
        args.extend(["--env".into(), format!("{key}={value}")]);
    }
    args.push(spec.image.clone());
    args.extend(spec.command.iter().cloned());
    Ok(args)
}

/// Delete every volume isobuild created. Used by `cargo sandbox clean`.
pub fn remove_all_volumes(backend: &DockerBackend) -> usize {
    let Ok(out) = backend.output(&["volume", "ls", "--quiet", "--filter", "label=isobuild=1"])
    else {
        return 0;
    };
    let names = String::from_utf8_lossy(&out.stdout).to_string();
    let names: Vec<&str> = names
        .lines()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .collect();
    for name in &names {
        backend.remove_volume(name);
    }
    names.len()
}

fn check_volume_name(name: &str) -> Result<(), BackendError> {
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if valid {
        Ok(())
    } else {
        Err(BackendError::Failed(format!(
            "invalid volume name `{name}`"
        )))
    }
}

/// Removes the container when the run ends, however it ends.
struct ContainerGuard {
    program: String,
    name: String,
}

impl ContainerGuard {
    fn register(program: &str, name: &str) -> Self {
        if let Ok(mut active) = ACTIVE.lock() {
            active.push((program.to_string(), name.to_string()));
        }
        ContainerGuard {
            program: program.to_string(),
            name: name.to_string(),
        }
    }
}

impl Drop for ContainerGuard {
    fn drop(&mut self) {
        let _ = bounded_output(
            &self.program,
            &["rm", "--force", &self.name],
            COMMAND_TIMEOUT,
        );
        if let Ok(mut active) = ACTIVE.lock() {
            active.retain(|(_, name)| name != &self.name);
        }
    }
}

fn capture_in_background(
    reader: impl Read + Send + 'static,
    file: File,
    cap: u64,
) -> mpsc::Receiver<bool> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(capture(reader, file, cap));
    });
    rx
}

/// Copy `reader` into `file`, keeping at most `cap` bytes but draining
/// everything so the child never blocks on a full pipe. Returns true if
/// output was dropped.
fn capture(mut reader: impl Read, mut file: File, cap: u64) -> bool {
    let mut buf = [0u8; 64 * 1024];
    let mut written = 0u64;
    let mut truncated = false;
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let room = cap.saturating_sub(written).min(n as u64) as usize;
                if room > 0 && file.write_all(&buf[..room]).is_ok() {
                    written += room as u64;
                }
                truncated |= room < n;
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    truncated
}

fn last_line(text: &str) -> &str {
    text.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("no details")
        .trim()
}

fn tail_of(path: &std::path::Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    last_line(&text).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Mount;
    use std::path::PathBuf;

    fn spec(network: bool) -> ContainerSpec {
        ContainerSpec {
            name: "isobuild-test".into(),
            image: "sha256:abc".into(),
            command: vec!["cargo".into(), "build".into()],
            env: vec![("CARGO_TERM_COLOR".into(), "never".into())],
            mounts: vec![
                Mount::host("/cache/run/work", "/work", false),
                Mount::host("/cache/cargo-home/registry", "/cargo-home/registry", true),
                Mount::volume("isobuild-target-abc", "/target"),
            ],
            tmpfs: vec![("/tmp".into(), 1024)],
            network,
            workdir: "/work".into(),
            user: Some("501:20".into()),
            memory_mb: 2048,
            cpus: 1.5,
            pids: 512,
            timeout: Duration::from_secs(10),
            stdout_path: PathBuf::from("/dev/null"),
            stderr_path: PathBuf::from("/dev/null"),
            output_cap: 1024,
        }
    }

    fn has_pair(args: &[String], flag: &str, value: &str) -> bool {
        args.windows(2).any(|w| w[0] == flag && w[1] == value)
    }

    #[test]
    fn containers_are_hardened() {
        let args = run_args(&spec(false)).unwrap();
        for flag in ["--read-only", "--init"] {
            assert!(args.iter().any(|a| a == flag), "missing {flag}");
        }
        assert!(has_pair(&args, "--network", "none"));
        assert!(has_pair(&args, "--cap-drop", "ALL"));
        assert!(has_pair(&args, "--security-opt", "no-new-privileges"));
        assert!(has_pair(&args, "--memory", "2048m"));
        assert!(
            has_pair(&args, "--memory-swap", "2048m"),
            "no swap beyond the memory limit"
        );
        assert!(has_pair(&args, "--cpus", "1.5"));
        assert!(has_pair(&args, "--pids-limit", "512"));
        assert!(has_pair(&args, "--user", "501:20"));
        assert!(
            !args
                .iter()
                .any(|a| a == "--privileged" || a.starts_with("--env-file"))
        );
    }

    #[test]
    fn mounts_env_and_command_are_passed_through() {
        let args = run_args(&spec(true)).unwrap();
        assert!(has_pair(&args, "--network", "bridge"));
        assert!(has_pair(
            &args,
            "--mount",
            "type=bind,source=/cache/run/work,target=/work"
        ));
        assert!(has_pair(
            &args,
            "--mount",
            "type=bind,source=/cache/cargo-home/registry,target=/cargo-home/registry,readonly"
        ));
        assert!(has_pair(
            &args,
            "--mount",
            "type=volume,source=isobuild-target-abc,target=/target"
        ));
        assert!(has_pair(&args, "--env", "CARGO_TERM_COLOR=never"));
        assert!(has_pair(
            &args,
            "--tmpfs",
            "/tmp:rw,exec,nosuid,nodev,size=1024m,mode=1777"
        ));
        let image = args.iter().position(|a| a == "sha256:abc").unwrap();
        assert_eq!(
            &args[image + 1..],
            ["cargo", "build"],
            "command follows the image"
        );
    }

    #[test]
    fn mount_paths_with_commas_are_refused() {
        let mut spec = spec(false);
        spec.mounts[0] = Mount::host("/tmp/a,readonly=false", "/work", false);
        assert!(run_args(&spec).is_err());
        spec.mounts[0] = Mount::volume("Bad,name", "/work");
        assert!(run_args(&spec).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn stuck_commands_time_out() {
        let started = Instant::now();
        let err = bounded_output("sleep", &["30"], Duration::from_millis(200)).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(err.to_string().contains("did not finish within"));

        let out = bounded_output("echo", &["hello"], Duration::from_secs(5)).unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "hello\n");
    }

    #[test]
    fn missing_docker_is_unavailable() {
        let err = bounded_output("/nonexistent/docker", &["version"], COMMAND_TIMEOUT).unwrap_err();
        assert!(matches!(err, BackendError::Unavailable(_)));
    }

    #[test]
    fn capture_keeps_the_head_and_reports_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out");
        let data = vec![b'x'; 200_000];
        let truncated = capture(&data[..], File::create(&path).unwrap(), 1000);
        assert!(truncated);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 1000);

        let truncated = capture(&b"small"[..], File::create(&path).unwrap(), 1000);
        assert!(!truncated);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "small");
    }
}
