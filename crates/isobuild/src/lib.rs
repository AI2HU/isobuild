//! Run `cargo check`, `clippy`, `build` or `test` on an untrusted project in
//! a sandbox and get back a structured [`Report`].
//!
//! A run has four phases:
//!
//! 1. **prepare** (host): check the project with [`isobuild_policy`],
//!    copy it into a staging directory without following symlinks.
//! 2. **fetch** (container, network on): `cargo fetch`. Runs no project code.
//! 3. **build** (container, network off): compile, including build scripts
//!    and proc macros. The dependency cache is mounted read-only.
//! 4. **test** (container, network off): run the test binaries.
//!
//! The isolation itself comes from a [`Backend`]; [`DockerBackend`] is the
//! one shipped today. The `cargo-sandbox` CLI is a thin front end over [`run`].

pub mod backend;
pub mod cache;
pub mod docker;
pub mod render;
pub mod request;
pub mod run;
pub mod stage;

pub use backend::{Backend, BackendError, ContainerOutcome, ContainerSpec, Mount, MountSource};
pub use docker::DockerBackend;
pub use isobuild_report::Report;
pub use request::{CargoCommand, CargoOptions, DEFAULT_IMAGE, RunRequest};
pub use run::run;
