# isobuild: design and reference

Build and test untrusted code in a sandbox, and get back a JSON report a
coding agent can act on. Rust comes first, through the `cargo-sandbox` CLI:

```bash
cargo sandbox test --project ./generated-crate
```

The core (isolation, policy, caching, the report format) is not tied to
Cargo, so other ecosystems can be added later as adapters.

## Why

Agents check the code they write by running it. In Rust that starts before
the program does: `cargo build` runs build scripts (`build.rs`) and
procedural macros with the full rights of whoever started the build. A
sandbox around the test binary alone does not cover that.

isobuild runs the whole build in an OS sandbox with no network, no host
environment and no host files except a copy of the project. There is no
daemon, VM or image: the sandbox wraps the Rust toolchain already installed
on the host, and a warm run takes well under a second.

## How a run works

| Phase | Where | Network | Runs project code | What happens |
| --- | --- | --- | --- | --- |
| prepare | host | – | no | Policy check; copy the project without following symlinks; skip `target/`; find the toolchain |
| fetch | sandbox | **on** | no | `cargo fetch` into the shared dependency cache |
| build | sandbox | off | **yes** | `cargo check/clippy/build`, or `cargo test --no-run` |
| test | sandbox | off | **yes** | `cargo test --no-fail-fast` |

### The sandbox

| | macOS: Seatbelt (`sandbox-exec`) | Linux: bubblewrap (`bwrap`) |
| --- | --- | --- |
| Filesystem | Deny by default. Readable: system libraries and tools, the toolchain, Xcode's clang and SDK, the dependency cache. Writable: the staged project, the build cache, a private temp dir | Empty, read-only root. `/usr` and a few `/etc` files mounted read-only, the same directories as on macOS at their own paths, fresh `/proc`, `/dev`, `/tmp`. Nothing else exists |
| Network | None in build and test, loopback included | Empty network namespace in build and test (loopback only) |
| Processes | Own process group | PID, IPC, UTS, user and cgroup namespaces; dies with the CLI |
| Environment | Only what the sandbox sets plus your `--env` values | Same |

Both run as the host user and can never write outside their directories. On
macOS, flag, ownership and setuid changes are denied too, so the host can
always delete what a run left behind.

Limits:

- **Time:** a timeout per phase; the whole process tree is killed.
- **Memory and processes:** a watchdog samples the sandbox's processes every
  100 ms and kills all of them when their combined resident memory or count
  goes over the limit (`oom`, `process_limit`). A very fast allocation can
  overshoot for up to one interval.
- **CPU:** `--cpus` sets cargo's job count; it is not a hard cap.

The toolchain is the host's: `--toolchain` picks a rustup toolchain (the
default toolchain otherwise), and the sandbox runs its real `cargo` and
`rustc`, never the rustup proxies. On macOS, the linker is Xcode's clang,
found with `xcrun` before the sandbox starts, because Apple's `/usr/bin/cc`
shim needs far more access than the sandbox allows.

The dependency cache is read-only during build and test. Build output lives
in a build cache per project, so warm runs only recompile what changed.

### Policy

Fetching has network access, so nothing the project chooses may run during
it. Before any sandbox starts, the project's `.cargo/config.toml` is
checked, and the run is refused (`policy_denied`) if the config:

- sets `build.rustc`, `build.rustc-wrapper` or `build.rustc-workspace-wrapper` (cargo calls these while fetching)
- sets a credential provider
- is a symlink, or cannot be parsed

`rust-toolchain.toml` and `rust-toolchain` are left out of the sandbox copy, with a warning, so the
toolchain chosen by the caller is always used. Settings that only apply while
compiling, such as linkers, runners and `[env]`, are allowed. That code runs
offline, like build scripts do.

## Install

Requires a Rust toolchain (rustup recommended) and:

- macOS: the Xcode Command Line Tools (`xcode-select --install`)
- Linux: bubblewrap (`apt install bubblewrap`, `dnf install bubblewrap`). On
  Ubuntu 24.04 and later, the distribution's package ships the AppArmor
  profile that lets it create user namespaces.

```bash
cargo install --path crates/cli
```

## Usage

```bash
cargo sandbox check  --project .
cargo sandbox clippy --project . --all-targets
cargo sandbox build  --project . --release
cargo sandbox test   --project . -- --exact tests::parses_header
cargo sandbox test   --project . --format human
cargo sandbox clean
```

Useful flags: `--features`, `--all-features`, `--no-default-features`,
`-p/--package`, `--workspace`, `--locked`, `--offline`, `--env KEY=VALUE`,
`--memory-mb`, `--cpus`, `--pids`, `--fetch-timeout`, `--build-timeout`,
`--test-timeout`, `--toolchain`, `--no-cache`, `--cache-scope`.
Run `cargo sandbox help test` for the full list.

Only a fixed set of cargo flags is accepted, so a request cannot move the
target directory or override the config.

### Exit codes

| Code | Meaning |
| --- | --- |
| 0 | ok |
| 1 | the project failed to fetch, build or test: fix the code |
| 2 | the request or project was refused (`invalid_request`, `policy_denied`, or bad CLI usage) |
| 3 | timeout, out of memory, or too many processes |
| 4 | the sandbox itself failed (`infra_error`): retry or report it, don't change the code |

## The report

JSON on stdout, versioned by `schema_version`. Fields are only added within a
version. Abridged example of a failed build:

```json
{
  "schema_version": 2,
  "status": "build_failed",
  "failed_phase": "build",
  "summary": "build failed: 1 error, 2 warnings",
  "command": "check",
  "phases": [
    { "phase": "fetch", "exit_code": 0, "duration_ms": 83, "timed_out": false, "oom_killed": false, "process_limit_hit": false, "output_truncated": false },
    { "phase": "build", "exit_code": 101, "duration_ms": 412, "timed_out": false, "oom_killed": false, "process_limit_hit": false, "output_truncated": false }
  ],
  "diagnostics": [
    {
      "level": "error",
      "source": "compiler",
      "code": "E0384",
      "message": "cannot assign twice to immutable variable `x`",
      "location": { "file": "src/lib.rs", "line": 4, "column": 5, "end_line": 4, "end_column": 10 },
      "label": "cannot assign twice to immutable variable",
      "notes": [],
      "suggestions": [
        {
          "message": "consider making this binding mutable",
          "location": { "file": "src/lib.rs", "line": 3, "column": 9, "end_line": 3, "end_column": 9 },
          "replacement": "mut ",
          "applicability": "machine_applicable"
        }
      ],
      "rendered": "error[E0384]: cannot assign twice to immutable variable `x`\n --> src/lib.rs:4:5 …"
    }
  ],
  "diagnostic_counts": { "errors": 1, "warnings": 2, "omitted": 0 },
  "tests": null,
  "policy": [],
  "environment": { "backend": "seatbelt", "toolchain": "rustc 1.91.1 (ed61e7d7e 2025-11-07)", "toolchain_id": "5f0c…", "limits": { "memory_mb": 4096, "cpus": 2.0, "pids": 1024, "fetch_timeout_secs": 300, "build_timeout_secs": 900, "test_timeout_secs": 600 } },
  "cache": { "key": "0a1344b5168e64fb9c678d59da406aae", "warm": true },
  "log_dir": "/home/me/.cache/isobuild/runs/1793461952-143b9-85d1d0"
}
```

`status` is one of `ok`, `invalid_request`, `policy_denied`, `fetch_failed`,
`build_failed`, `test_failed`, `timeout`, `oom`, `process_limit`, `infra_error`.

Diagnostics come from cargo's JSON messages (compiler errors with spans and
rustc's suggestions) and from cargo's own stderr (resolution errors, build
script failures with their output, `cargo:warning=` lines). They are
deduplicated, errors first, and capped by `--max-diagnostics`.

Test results are parsed from libtest's text output, because its JSON output
is still unstable. Each failure has its panic message and location. When a
run times out, `tests.complete` is false and `tests.slow` names the tests
libtest saw running for over 60 seconds.

Full logs for every phase stay in `log_dir` for three days.

## Caching

```text
~/.cache/isobuild/             ($ISOBUILD_CACHE or $XDG_CACHE_HOME to move it)
  cargo-home/registry, git/    downloaded crates; written only by the fetch phase
  targets/<key>/target/        a build cache (CARGO_TARGET_DIR)
  runs/<id>/                   phase logs; the sandbox's directories while it runs
```

- The dependency cache is shared by all runs. Only `cargo fetch` writes to it,
  and fetches are serialized with a lock.
- The build cache is keyed by scope and toolchain. The scope is the project
  path by default (`--cache-scope` overrides it). Build output is written by
  project code, so one scope never reads another's.
- Build caches unused for 14 days are removed. `cargo sandbox clean` removes everything.

## Crates

| Crate | Purpose |
| --- | --- |
| `isobuild-report` | The report types and JSON format |
| `isobuild-diagnostics` | Parsers for cargo JSON, cargo stderr and libtest output |
| `isobuild-policy` | Project checks that run before anything else |
| `isobuild` | The runner, the `Backend` trait, and the Seatbelt and bubblewrap backends |
| `cargo-sandbox` | The CLI, usable as `cargo sandbox` |

The runner talks to isolation through the `Backend` trait, so a microVM or
hosted-sandbox backend can be added without touching the run logic.

## Limitations of v0

- **The kernel is shared.** Both backends are OS sandboxes, not VMs. They
  are meant for code from your own agents and for local use. For hostile
  code from many tenants, add a microVM backend (planned).
- **Memory and process limits are enforced by sampling,** not by the kernel.
  On Linux, cgroups would make them exact; that needs a systemd user session
  or cgroup delegation and is planned.
- **macOS:** no process namespace, so a process that detaches itself (double
  fork and `setsid`) between two samples can outlive the run, still
  sandboxed. Tests that need loopback networking fail.
- **Linux:** no seccomp filter yet; the namespaces and the empty root are
  the boundary.
- **Fetch has unrestricted network.** It runs only cargo, but the network is
  not limited to crates.io yet. An allowlisting proxy is planned.
- **System C libraries are visible, Homebrew's are not.** Crates that link
  against `/opt/homebrew` or `/usr/local` libraries, or that need `cmake` or
  `pkg-config` from there, fail to build.
- **No disk quota on build caches.** A build can fill the disk.
- **No sharing of compiled dependencies across projects.** Doing that safely
  needs a trusted job that builds dependencies without ever running workspace
  code.

## Development

```bash
cargo test                                          # everything, sandbox tests included
ISOBUILD_NETWORK_TESTS=1 cargo test -p isobuild --test sandbox   # plus a crates.io download
```

The sandbox tests run real sandboxes and check the security properties:
no network in build scripts or tests, no host environment or files, no
cargo config from parent directories, no writes outside the sandbox's
directories, and the time, memory and process limits.

## License

Licensed under either of [Apache-2.0](../LICENSE-APACHE) or [MIT](../LICENSE-MIT), at your option.
