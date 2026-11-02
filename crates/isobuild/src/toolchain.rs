//! The Rust toolchain a run uses, found on the host before anything is
//! sandboxed.
//!
//! The sandbox runs the toolchain's real binaries, never the rustup proxies:
//! a proxy reads override files and may try to download a toolchain. On
//! macOS the C compiler and linker are resolved here too, because Apple's
//! `/usr/bin/cc` shim needs access far beyond what the sandbox allows.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::process::{bounded_output, last_line};

/// Generous: the first rustup call after an update can be slow.
const TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toolchain {
    /// `rustc --version`, e.g. `rustc 1.91.1 (ed61e7d7e 2025-11-07)`.
    pub version: String,
    /// Changes whenever rustc, cargo or the install location does.
    pub id: String,
    /// Directory holding `bin/cargo`, `bin/rustc` and the standard library.
    pub sysroot: PathBuf,
    /// Directories the sandbox must be able to read for this toolchain.
    pub read_only: Vec<PathBuf>,
    /// Environment the toolchain needs, e.g. `SDKROOT` on macOS.
    pub env: Vec<(String, String)>,
    /// Programs to put on `PATH` under another name, e.g. `cc` for Xcode's clang.
    pub tools: Vec<(String, PathBuf)>,
}

impl Toolchain {
    pub fn bin_dir(&self) -> PathBuf {
        self.sysroot.join("bin")
    }

    pub fn cargo(&self) -> PathBuf {
        self.bin_dir().join("cargo")
    }

    pub fn has_clippy(&self) -> bool {
        self.bin_dir().join("cargo-clippy").is_file()
    }
}

/// Find the toolchain named `requested` (a rustup name such as `stable` or
/// `1.91.1`), or the host's default. Commands run in `cwd`, which must not
/// contain a toolchain override.
pub fn resolve(requested: Option<&str>, cwd: &Path) -> Result<Toolchain, String> {
    let mut rustc = Command::new("rustc");
    if let Some(name) = requested {
        check_name(name)?;
        rustc.arg(format!("+{name}"));
    }
    rustc
        .args(["--print", "sysroot"])
        .current_dir(cwd)
        .env("RUSTUP_AUTO_INSTALL", "0");
    let out = bounded_output(rustc, TIMEOUT).map_err(|_| {
        "no Rust toolchain found; install one with rustup (https://rustup.rs)".to_string()
    })?;
    if !out.status.success() {
        return Err(format!(
            "could not find the Rust toolchain {}: {}",
            requested.map_or("default".to_string(), |n| format!("`{n}`")),
            last_line(&out.stderr)
        ));
    }
    let sysroot = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())
        .canonicalize()
        .map_err(|err| format!("the toolchain's sysroot is unreadable: {err}"))?;
    let bin = sysroot.join("bin");
    for tool in ["cargo", "rustc"] {
        if !bin.join(tool).is_file() {
            return Err(format!(
                "the toolchain at {} has no `{tool}`",
                sysroot.display()
            ));
        }
    }

    let version_of = |program: PathBuf, arg: &str| {
        let mut command = Command::new(&program);
        command.arg(arg).current_dir(cwd);
        match bounded_output(command, TIMEOUT) {
            Ok(out) if out.status.success() => {
                Ok(String::from_utf8_lossy(&out.stdout).into_owned())
            }
            Ok(out) => Err(format!(
                "`{} {arg}` failed: {}",
                program.display(),
                last_line(&out.stderr)
            )),
            Err(err) => Err(err),
        }
    };
    let rustc_verbose = version_of(bin.join("rustc"), "-vV")?;
    let cargo_version = version_of(bin.join("cargo"), "-V")?;

    let mut hasher = Sha256::new();
    for part in [
        &rustc_verbose,
        &cargo_version,
        &sysroot.display().to_string(),
    ] {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    let id = hasher.finalize()[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();

    #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
    let mut toolchain = Toolchain {
        version: rustc_verbose.lines().next().unwrap_or_default().to_string(),
        id,
        read_only: vec![sysroot.clone()],
        sysroot,
        env: Vec::new(),
        tools: Vec::new(),
    };
    #[cfg(target_os = "macos")]
    add_xcode_tools(&mut toolchain, cwd)?;
    Ok(toolchain)
}

/// Rustup toolchain names: `stable`, `1.91.1`, `nightly-2025-06-01-aarch64-apple-darwin`.
fn check_name(name: &str) -> Result<(), String> {
    let valid = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        && name.starts_with(|c: char| c.is_ascii_alphanumeric());
    if valid {
        Ok(())
    } else {
        Err(format!("invalid toolchain name `{name}`"))
    }
}

/// Programs exposed on `PATH`, as (name, file in Xcode's toolchain).
#[cfg(target_os = "macos")]
const XCODE_TOOLS: [(&str, &str); 11] = [
    ("cc", "clang"),
    ("c++", "clang++"),
    ("clang", "clang"),
    ("clang++", "clang++"),
    ("ar", "ar"),
    ("ranlib", "ranlib"),
    ("ld", "ld"),
    ("nm", "nm"),
    ("strip", "strip"),
    ("libtool", "libtool"),
    ("lipo", "lipo"),
];

/// Point the sandbox at Xcode's clang and SDK directly. With `SDKROOT` set,
/// rustc and the `cc` crate never call `xcrun` themselves.
#[cfg(target_os = "macos")]
fn add_xcode_tools(toolchain: &mut Toolchain, cwd: &Path) -> Result<(), String> {
    const MISSING: &str = "linking Rust code on macOS needs the Xcode Command Line Tools; install them with `xcode-select --install`";
    let xcrun = |args: &[&str]| -> Result<PathBuf, String> {
        let mut command = Command::new("/usr/bin/xcrun");
        command.args(args).current_dir(cwd);
        let out = bounded_output(command, TIMEOUT).map_err(|_| MISSING.to_string())?;
        if !out.status.success() {
            return Err(MISSING.to_string());
        }
        PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())
            .canonicalize()
            .map_err(|_| MISSING.to_string())
    };
    let clang = xcrun(&["--find", "clang"])?;
    let sdk = xcrun(&["--sdk", "macosx", "--show-sdk-path"])?;
    let bin = clang.parent().ok_or(MISSING)?;
    for (name, file) in XCODE_TOOLS {
        let path = bin.join(file);
        if path.is_file() {
            toolchain.tools.push((name.to_string(), path));
        }
    }
    // `usr/` of Xcode's toolchain: the tools, plus clang's headers and runtime in `usr/lib`.
    toolchain
        .read_only
        .push(bin.parent().ok_or(MISSING)?.to_path_buf());
    toolchain.read_only.push(sdk.clone());
    toolchain
        .env
        .push(("SDKROOT".to_string(), sdk.display().to_string()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toolchain_names_cannot_smuggle_flags_or_paths() {
        for good in [
            "stable",
            "1.91.1",
            "nightly-2025-06-01",
            "stable-aarch64-apple-darwin",
        ] {
            assert!(check_name(good).is_ok(), "{good}");
        }
        for bad in ["", "-vV", "../evil", "a b", "/opt/rust", "stable;rm"] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_host_toolchain_resolves() {
        let dir = tempfile::tempdir().unwrap();
        let toolchain = resolve(None, dir.path()).unwrap();
        assert!(toolchain.version.starts_with("rustc "));
        assert_eq!(toolchain.id.len(), 32);
        assert!(toolchain.cargo().is_file());
        assert!(toolchain.read_only.contains(&toolchain.sysroot));
        if cfg!(target_os = "macos") {
            assert!(toolchain.tools.iter().any(|(name, _)| name == "cc"));
            assert!(toolchain.env.iter().any(|(key, _)| key == "SDKROOT"));
        }
    }

    #[test]
    fn unknown_toolchains_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve(Some("0.0.1-not-installed"), dir.path()).unwrap_err();
        assert!(err.contains("0.0.1-not-installed"), "{err}");
    }
}
