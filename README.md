# isobuild

> **Status: in active development.** The CLI, the report format and the crate APIs may change without notice. Not yet published to crates.io.

isobuild builds and tests untrusted code in a sandbox and returns a JSON report a coding agent can act on.

It exists because building Rust runs code: `build.rs` scripts and proc macros execute with your permissions before the program even compiles. isobuild runs the whole build in a locked-down container:

- dependencies are fetched with network on, but no project code runs
- build and test run with **no network**, no host environment and no host files
- memory, CPU, process and time limits apply
- results come back as structured errors, suggestions and test failures

Rust is supported first, through the `cargo-sandbox` CLI.

## Setup

Requirements: Rust 1.89+ and Docker.

```bash
git clone https://github.com/AI2HU/isobuild.git
cd isobuild
cargo install --path crates/cli
```

## Usage

```bash
cargo sandbox test  --project path/to/crate
cargo sandbox check --project path/to/crate --format human
cargo sandbox clean
```

The JSON report goes to stdout. The exit code is `0` for ok, `1` when the code failed to build or test, `2` when the request was refused, `3` for a timeout or out of memory, and `4` for a sandbox error.

## Development

```bash
cargo test                                                    # no Docker needed
ISOBUILD_DOCKER_TESTS=1 cargo test -p isobuild --test docker  # real containers
```

How it works, the report format, the policy rules and the known limits are in [docs/design.md](docs/design.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option.

Unless you explicitly state otherwise, any contribution you intentionally submit for inclusion in this project, as defined in the Apache-2.0 license, is dual licensed as above, without any additional terms or conditions.
