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

isobuild runs the whole build in an isolated container with no
network, no host environment and no host files except a copy of the project.

## How a run works

| Phase | Where | Network | Runs project code | What happens |
| --- | --- | --- | --- | --- |
| prepare | host | – | no | Policy check; copy the project without following symlinks; skip `target/` |
| fetch | container | **on** | no | `cargo fetch` into the shared dependency cache |
| build | container | off | **yes** | `cargo check/clippy/build`, or `cargo test --no-run` |
| test | container | off | **yes** | `cargo test --no-fail-fast` |

Every container:

- has a read-only root filesystem, no capabilities and `no-new-privileges`
- has limits on memory (no swap), CPU, processes and open files, plus a wall-clock timeout per phase
- runs as the host user, never as root
- gets only the environment the sandbox sets plus your `--env` values; nothing is inherited from the host
- is pinned to the image id inspected at the start of the run

The dependency cache is mounted read-only during build and test. Build output
lives in a Docker volume per project, so warm runs only recompile what changed.

### Policy

Fetching has network access, so nothing the project chooses may run during
it. Before any container starts, the project's `.cargo/config.toml` is
checked, and the run is refused (`policy_denied`) if the config:

- sets `build.rustc`, `build.rustc-wrapper` or `build.rustc-workspace-wrapper` (cargo calls these while fetching)
- sets a credential provider
- is a symlink, or cannot be parsed

`rust-toolchain.toml` and `rust-toolchain` are left out of the sandbox copy, with a warning, so the
image's pinned toolchain is always used. Settings that only apply while
compiling, such as linkers, runners and `[env]`, are allowed. That code runs
offline, like build scripts do.

## Install

Requires Docker (or a compatible CLI via `ISOBUILD_DOCKER`).

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
`--test-timeout`, `--image`, `--no-cache`, `--cache-scope`.
Run `cargo sandbox help test` for the full list.

Only a fixed set of cargo flags is accepted, so a request cannot move the
target directory or override the config.

### Exit codes

| Code | Meaning |
| --- | --- |
| 0 | ok |
| 1 | the project failed to fetch, build or test: fix the code |
| 2 | the request or project was refused (`invalid_request`, `policy_denied`, or bad CLI usage) |
| 3 | timeout or out of memory |
| 4 | the sandbox itself failed (`infra_error`): retry or report it, don't change the code |

## The report

JSON on stdout, versioned by `schema_version`. Fields are only added within a
version. Abridged example of a failed build:

```json
{
  "schema_version": 1,
  "status": "build_failed",
  "failed_phase": "build",
  "summary": "build failed: 1 error, 2 warnings",
  "command": "check",
  "phases": [
    { "phase": "fetch", "exit_code": 0, "duration_ms": 2879, "timed_out": false, "oom_killed": false, "output_truncated": false },
    { "phase": "build", "exit_code": 101, "duration_ms": 2685, "timed_out": false, "oom_killed": false, "output_truncated": false }
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
  "environment": { "image": "rust:1.91.1-slim-bookworm", "image_id": "sha256:…", "limits": { "memory_mb": 4096, "cpus": 2.0, "pids": 1024, "fetch_timeout_secs": 300, "build_timeout_secs": 900, "test_timeout_secs": 600 } },
  "cache": { "key": "0a1344b5168e64fb9c678d59da406aae", "warm": true },
  "log_dir": "/home/me/.cache/isobuild/runs/1793461952-143b9-85d1d0"
}
```

`status` is one of `ok`, `invalid_request`, `policy_denied`, `fetch_failed`,
`build_failed`, `test_failed`, `timeout`, `oom`, `infra_error`.

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
  targets/<key>/               last-use stamp for volume isobuild-target-<key>
  runs/<id>/                   phase logs
```

- The dependency cache is shared by all runs. Only `cargo fetch` writes to it,
  and fetches are serialized with a lock.
- The build cache is keyed by scope and image id. The scope is the project
  path by default (`--cache-scope` overrides it). Build output is written by
  project code, so one scope never reads another's.
- Build caches unused for 14 days are removed. `cargo sandbox clean` removes everything.

## Crates

| Crate | Purpose |
| --- | --- |
| `isobuild-report` | The report types and JSON format |
| `isobuild-diagnostics` | Parsers for cargo JSON, cargo stderr and libtest output |
| `isobuild-policy` | Project checks that run before anything else |
| `isobuild` | The runner, the `Backend` trait and the Docker backend |
| `cargo-sandbox` | The CLI, usable as `cargo sandbox` |

The runner talks to isolation through the `Backend` trait, so a gVisor,
microVM or hosted-sandbox backend can replace Docker.

## Limitations of v0

- **Docker is the only backend.** Containers share the host kernel. For
  hostile code from many tenants, use a microVM or gVisor backend (planned),
  or at least run Docker with the gVisor runtime.
- **Fetch has unrestricted network.** It runs only cargo, but the network is
  not limited to crates.io yet. An allowlisting proxy is planned.
- **No disk quota on build volumes.** A build can fill the Docker disk.
- **No sharing of compiled dependencies across projects.** Doing that safely
  needs a trusted job that builds dependencies without ever running workspace
  code.
- Linux and macOS hosts. On macOS, Docker Desktop's file sharing makes
  containers slower to start; build output already lives in volumes to avoid
  its slowest path.

## Development

```bash
cargo test                                   # unit, fixture and orchestration tests
ISOBUILD_DOCKER_TESTS=1 cargo test -p isobuild --test docker
```

The Docker tests run real containers and check the security properties:
no network in build scripts or tests, no host environment, symlinks that
can't reach host files, a read-only root filesystem, timeouts and memory
limits.

## License

Licensed under either of [Apache-2.0](../LICENSE-APACHE) or [MIT](../LICENSE-MIT), at your option.
