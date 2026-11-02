//! Supervision of a sandboxed process, shared by every backend: output
//! capture, the timeout, and the memory and process limits.
//!
//! Neither Seatbelt nor unprivileged bubblewrap can cap memory or process
//! counts, so a watchdog samples the sandbox's process tree every 100 ms and
//! kills all of it when a limit is exceeded. A process that allocates very
//! fast can go over the memory limit for up to one interval before it dies.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use crate::backend::{BackendError, SandboxOutcome, SandboxSpec};

const SAMPLE_INTERVAL: Duration = Duration::from_millis(100);
const POLL_INTERVAL: Duration = Duration::from_millis(20);
/// How long to wait for output pipes to close once the sandbox has exited.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Root processes of the sandboxes currently running, so an interrupted CLI
/// can kill them.
static ACTIVE: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Kill every sandbox this process started that is still running.
pub fn kill_active() {
    let active: Vec<u32> = ACTIVE
        .lock()
        .map(|mut a| a.drain(..).collect())
        .unwrap_or_default();
    for root in active {
        kill_tree(root);
    }
}

/// Run `command` (the backend's wrapper around `spec.command`) under the
/// limits of `spec`. Sandbox setup errors, recognised by `setup_error_prefix`
/// at the start of stderr, become a [`BackendError`] instead of an exit code.
pub(crate) fn supervise(
    mut command: Command,
    spec: &SandboxSpec,
    setup_error_prefix: &str,
) -> Result<SandboxOutcome, BackendError> {
    let io_err = |err: io::Error| BackendError::Failed(format!("could not write logs: {err}"));
    let stdout_file = File::create(&spec.stdout_path).map_err(io_err)?;
    let stderr_file = File::create(&spec.stderr_path).map_err(io_err)?;

    command
        .current_dir(&spec.workdir)
        .env_clear()
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own process group, so Ctrl-C reaches the CLI only and the
        // whole group can be killed at once.
        .process_group(0);
    let mut child = command.spawn().map_err(|err| {
        BackendError::Unavailable(format!(
            "could not start `{}`: {err}",
            command.get_program().to_string_lossy()
        ))
    })?;
    let root = child.id();
    let _active = Active::register(root);

    let cap = spec.output_cap;
    let stdout_done = capture_in_background(child.stdout.take().expect("piped"), stdout_file, cap);
    let stderr_done = capture_in_background(child.stderr.take().expect("piped"), stderr_file, cap);

    let started = Instant::now();
    let mut next_sample = started;
    let memory_kb = spec.memory_mb.saturating_mul(1024);
    let mut outcome = SandboxOutcome::default();
    let wait_err =
        |err: io::Error| BackendError::Failed(format!("lost track of the sandbox: {err}"));
    let status = loop {
        if let Some(status) = child.try_wait().map_err(wait_err)? {
            break status;
        }
        let now = Instant::now();
        if now.duration_since(started) >= spec.timeout {
            outcome.timed_out = true;
        } else if now >= next_sample {
            next_sample = now + SAMPLE_INTERVAL;
            if let Some(usage) = sample(root) {
                outcome.oom_killed = usage.rss_kb > memory_kb;
                outcome.process_limit_hit = usage.pids.len() > spec.pids as usize;
            }
        }
        if outcome.timed_out || outcome.oom_killed || outcome.process_limit_hit {
            kill_tree(root);
            let _ = child.kill();
            break child.wait().map_err(wait_err)?;
        }
        thread::sleep(POLL_INTERVAL);
    };
    outcome.duration = started.elapsed();
    // Whatever the command left running, e.g. a server started by a test.
    kill_tree(root);

    // A pipe that never closes counts as truncated output.
    outcome.output_truncated = stdout_done.recv_timeout(DRAIN_TIMEOUT).unwrap_or(true)
        | stderr_done.recv_timeout(DRAIN_TIMEOUT).unwrap_or(true);
    outcome.exit_code = status.code().or_else(|| status.signal().map(|s| 128 + s));

    if outcome.exit_code != Some(0) {
        let stderr = std::fs::read_to_string(&spec.stderr_path).unwrap_or_default();
        if let Some(first) = stderr.lines().next()
            && first.starts_with(setup_error_prefix)
        {
            return Err(BackendError::Failed(format!(
                "the sandbox could not start: {first}"
            )));
        }
    }
    Ok(outcome)
}

/// Unregisters a sandbox from [`ACTIVE`] when its run ends.
struct Active(u32);

impl Active {
    fn register(root: u32) -> Self {
        if let Ok(mut active) = ACTIVE.lock() {
            active.push(root);
        }
        Active(root)
    }
}

impl Drop for Active {
    fn drop(&mut self) {
        if let Ok(mut active) = ACTIVE.lock() {
            active.retain(|&pid| pid != self.0);
        }
    }
}

/// Resource use of a sandbox at one instant.
struct Usage {
    rss_kb: u64,
    pids: Vec<u32>,
}

/// One row of the host's process table.
struct Proc {
    pid: u32,
    ppid: u32,
    pgid: u32,
    rss_kb: u64,
}

/// The sandbox's processes: descendants of `root`, plus members of its
/// process group whose parent already exited.
fn sample(root: u32) -> Option<Usage> {
    let table = process_table()?;
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for p in &table {
        children.entry(p.ppid).or_default().push(p.pid);
    }
    let mut members: HashSet<u32> = table
        .iter()
        .filter(|p| p.pid == root || p.pgid == root)
        .map(|p| p.pid)
        .collect();
    let mut queue: Vec<u32> = members.iter().copied().collect();
    while let Some(pid) = queue.pop() {
        for &child in children.get(&pid).into_iter().flatten() {
            if members.insert(child) {
                queue.push(child);
            }
        }
    }
    let rss_kb = table
        .iter()
        .filter(|p| members.contains(&p.pid))
        .map(|p| p.rss_kb)
        .sum();
    Some(Usage {
        rss_kb,
        pids: members.into_iter().collect(),
    })
}

#[cfg(target_os = "linux")]
fn process_table() -> Option<Vec<Proc>> {
    let mut table = Vec::new();
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse().ok()) else {
            continue;
        };
        // Processes can exit between listing and reading.
        let Ok(status) = std::fs::read_to_string(entry.path().join("status")) else {
            continue;
        };
        let field = |name: &str| {
            status
                .lines()
                .find_map(|l| l.strip_prefix(name))
                .and_then(|v| v.split_whitespace().next())
                .and_then(|v| v.parse::<u64>().ok())
        };
        table.push(Proc {
            pid,
            ppid: field("PPid:").unwrap_or(0) as u32,
            pgid: field("NSpgid:").unwrap_or(0) as u32,
            rss_kb: field("VmRSS:").unwrap_or(0),
        });
    }
    Some(table)
}

#[cfg(not(target_os = "linux"))]
fn process_table() -> Option<Vec<Proc>> {
    let mut ps = Command::new("/bin/ps");
    ps.args(["-A", "-o", "pid=,ppid=,pgid=,rss="]);
    let out = bounded_output(ps, Duration::from_secs(5)).ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let table = text
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace().map(str::parse::<u64>);
            let (Some(Ok(pid)), Some(Ok(ppid)), Some(Ok(pgid)), Some(Ok(rss_kb))) =
                (fields.next(), fields.next(), fields.next(), fields.next())
            else {
                return None;
            };
            Some(Proc {
                pid: pid as u32,
                ppid: ppid as u32,
                pgid: pgid as u32,
                rss_kb,
            })
        })
        .collect();
    Some(table)
}

/// SIGKILL the sandbox rooted at `root`: its process group and every
/// process [`sample`] finds.
fn kill_tree(root: u32) {
    let mut targets = vec![format!("-{root}")];
    if let Some(usage) = sample(root) {
        targets.extend(usage.pids.iter().map(u32::to_string));
    }
    let _ = Command::new("/bin/kill")
        .arg("-KILL")
        .arg("--")
        .args(&targets)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Run a host command, collecting its output, and give up after `timeout`.
pub(crate) fn bounded_output(mut command: Command, timeout: Duration) -> Result<Output, String> {
    let program = command.get_program().to_string_lossy().into_owned();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("could not run `{program}`: {err}"))?;
    let stdout = read_in_background(child.stdout.take().expect("piped"));
    let stderr = read_in_background(child.stderr.take().expect("piped"));

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < timeout => thread::sleep(POLL_INTERVAL),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "`{program}` did not finish within {}s",
                    timeout.as_secs()
                ));
            }
            Err(err) => return Err(format!("lost track of `{program}`: {err}")),
        }
    };
    Ok(Output {
        status,
        stdout: stdout.recv_timeout(DRAIN_TIMEOUT).unwrap_or_default(),
        stderr: stderr.recv_timeout(DRAIN_TIMEOUT).unwrap_or_default(),
    })
}

/// The last non-empty line of `text`, for short error messages.
pub(crate) fn last_line(text: &[u8]) -> String {
    String::from_utf8_lossy(text)
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("no details")
        .trim()
        .to_string()
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn spec(dir: &std::path::Path, command: &[&str]) -> SandboxSpec {
        SandboxSpec {
            name: "test".into(),
            command: command.iter().map(|s| s.to_string()).collect(),
            env: vec![("PATH".into(), "/usr/bin:/bin".into())],
            workdir: dir.to_path_buf(),
            read_only: Vec::new(),
            writable: Vec::new(),
            network: false,
            memory_mb: 4096,
            pids: 64,
            timeout: Duration::from_secs(20),
            stdout_path: dir.join("out"),
            stderr_path: dir.join("err"),
            output_cap: 1 << 20,
        }
    }

    /// Runs `spec.command` directly: the supervision without a sandbox.
    fn run(spec: &SandboxSpec) -> SandboxOutcome {
        let mut command = Command::new(&spec.command[0]);
        command.args(&spec.command[1..]);
        supervise(command, spec, "setup:").unwrap()
    }

    #[test]
    fn exit_codes_and_output_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = run(&spec(
            dir.path(),
            &["/bin/sh", "-c", "echo out; echo err >&2; exit 3"],
        ));
        assert_eq!(outcome.exit_code, Some(3));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out")).unwrap(),
            "out\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("err")).unwrap(),
            "err\n"
        );
    }

    #[test]
    fn the_host_environment_is_not_inherited() {
        let dir = tempfile::tempdir().unwrap();
        run(&spec(dir.path(), &["/usr/bin/env"]));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out")).unwrap(),
            "PATH=/usr/bin:/bin\n"
        );
    }

    #[test]
    fn timeouts_kill_the_whole_tree() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("survived");
        let script = format!("(sleep 3; touch {}) & sleep 60", marker.display());
        let mut spec = spec(dir.path(), &["/bin/sh", "-c", &script]);
        spec.timeout = Duration::from_millis(500);
        let started = Instant::now();
        let outcome = run(&spec);
        assert!(outcome.timed_out);
        assert_eq!(outcome.exit_code, Some(137));
        assert!(started.elapsed() < Duration::from_secs(5));
        thread::sleep(Duration::from_secs(4));
        assert!(!marker.exists(), "a background child outlived the timeout");
    }

    #[test]
    fn processes_left_behind_are_killed() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("survived");
        let script = format!("(sleep 2; touch {}) &", marker.display());
        let outcome = run(&spec(dir.path(), &["/bin/sh", "-c", &script]));
        assert_eq!(outcome.exit_code, Some(0));
        thread::sleep(Duration::from_secs(3));
        assert!(!marker.exists(), "a background child outlived the run");
    }

    #[test]
    fn too_many_processes_are_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let mut spec = spec(
            dir.path(),
            &[
                "/bin/sh",
                "-c",
                "for i in $(seq 100); do sleep 30 & done; wait",
            ],
        );
        spec.pids = 20;
        let outcome = run(&spec);
        assert!(outcome.process_limit_hit);
        assert!(!outcome.timed_out);
    }

    #[test]
    fn memory_hogs_are_stopped() {
        let dir = tempfile::tempdir().unwrap();
        // Many small processes, so no single allocation is large.
        let mut spec = spec(
            dir.path(),
            &[
                "/bin/sh",
                "-c",
                "for i in $(seq 40); do /usr/bin/perl -e '$x = \"x\" x (16 << 20); sleep 30' & done; wait",
            ],
        );
        spec.memory_mb = 256;
        spec.pids = 100;
        let outcome = run(&spec);
        assert!(outcome.oom_killed, "{outcome:?}");
    }

    #[test]
    fn setup_errors_are_backend_errors() {
        let dir = tempfile::tempdir().unwrap();
        let spec = spec(
            dir.path(),
            &["/bin/sh", "-c", "echo 'setup: no namespaces' >&2; exit 1"],
        );
        let mut command = Command::new(&spec.command[0]);
        command.args(&spec.command[1..]);
        let err = supervise(command, &spec, "setup:").unwrap_err();
        assert!(err.to_string().contains("no namespaces"));
    }

    #[test]
    fn missing_programs_are_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let spec = spec(dir.path(), &["/nonexistent/sandbox"]);
        let command = Command::new(PathBuf::from("/nonexistent/sandbox"));
        let err = supervise(command, &spec, "setup:").unwrap_err();
        assert!(matches!(err, BackendError::Unavailable(_)));
    }

    #[test]
    fn stuck_host_commands_time_out() {
        let started = Instant::now();
        let mut sleep = Command::new("sleep");
        sleep.arg("30");
        let err = bounded_output(sleep, Duration::from_millis(200)).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(err.contains("did not finish within"));
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
