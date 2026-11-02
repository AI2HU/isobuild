//! Run `cargo check`, `clippy`, `build` or `test` on an untrusted project in
//! a sandbox and get back a structured [`Report`].
//!
//! A run has four phases:
//!
//! 1. **prepare** (host): check the project with [`isobuild_policy`],
//!    copy it into a staging directory without following symlinks, and find
//!    the Rust toolchain.
//! 2. **fetch** (sandbox, network on): `cargo fetch`. Runs no project code.
//! 3. **build** (sandbox, network off): compile, including build scripts
//!    and proc macros. The dependency cache is read-only.
//! 4. **test** (sandbox, network off): run the test binaries.
//!
//! The isolation itself comes from a [`Backend`]: Seatbelt on macOS and
//! bubblewrap on Linux, chosen by [`default_backend`]. The `cargo-sandbox`
//! CLI is a thin front end over [`run`].

#[cfg(not(unix))]
compile_error!("isobuild runs on Linux and macOS only");

pub mod backend;
pub mod bwrap;
pub mod cache;
pub mod process;
pub mod render;
pub mod request;
pub mod run;
pub mod seatbelt;
pub mod stage;
pub mod toolchain;

pub use backend::{Backend, BackendError, SandboxOutcome, SandboxSpec, default_backend};
pub use isobuild_report::Report;
pub use process::kill_active;
pub use request::{CargoCommand, CargoOptions, RunRequest};
pub use run::run;
pub use toolchain::Toolchain;
