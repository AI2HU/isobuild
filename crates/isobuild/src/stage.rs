//! Copy a project into the directory the sandbox works in.
//!
//! The copy never follows symlinks. Links are recreated as links, so a link
//! to `~/.ssh/id_rsa` points at a file the sandbox cannot read instead of
//! carrying the key in. Build output (`target/` and anything cargo marked with
//! `CACHEDIR.TAG`) is skipped: it is large, and it was produced outside the
//! sandbox.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StageStats {
    pub files: u64,
    pub bytes: u64,
}

#[derive(Debug)]
pub enum StageError {
    TooLarge { limit: u64 },
    Io(io::Error),
}

impl fmt::Display for StageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StageError::TooLarge { limit } => write!(
                f,
                "the project is larger than {} MB, excluding target directories",
                limit / (1024 * 1024)
            ),
            StageError::Io(err) => write!(f, "{err}"),
        }
    }
}

impl From<io::Error> for StageError {
    fn from(err: io::Error) -> Self {
        StageError::Io(err)
    }
}

/// Copy `src` to `dst` (which must not exist), leaving out `excluded` paths
/// relative to `src`, and stop once more than `max_bytes` would be copied.
pub fn copy_project(
    src: &Path,
    dst: &Path,
    excluded: &[PathBuf],
    max_bytes: u64,
) -> Result<StageStats, StageError> {
    let mut stats = StageStats::default();
    fs::create_dir(dst)?;
    let mut pending = vec![PathBuf::new()];

    while let Some(relative) = pending.pop() {
        let dir = src.join(&relative);
        let mut entries: Vec<_> = fs::read_dir(&dir)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|e| e.file_name());

        for entry in entries {
            let rel = relative.join(entry.file_name());
            if excluded.contains(&rel) {
                continue;
            }
            let from = entry.path();
            let to = dst.join(&rel);
            // `DirEntry::file_type` does not follow symlinks.
            let kind = entry.file_type()?;

            if kind.is_symlink() {
                copy_symlink(&from, &to)?;
            } else if kind.is_dir() {
                if is_build_output(&from, &rel) {
                    continue;
                }
                fs::create_dir(&to)?;
                pending.push(rel);
            } else if kind.is_file() {
                stats.bytes += entry.metadata()?.len();
                if stats.bytes > max_bytes {
                    return Err(StageError::TooLarge { limit: max_bytes });
                }
                fs::copy(&from, &to)?;
                stats.files += 1;
            }
            // Sockets, FIFOs and devices are left out.
        }
    }
    Ok(stats)
}

fn is_build_output(dir: &Path, rel: &Path) -> bool {
    rel == Path::new("target") || dir.join("CACHEDIR.TAG").is_file()
}

#[cfg(unix)]
fn copy_symlink(from: &Path, to: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(fs::read_link(from)?, to)
}

#[cfg(not(unix))]
fn copy_symlink(_from: &Path, _to: &Path) -> io::Result<()> {
    // Recreating links needs extra privileges on Windows; leave them out.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, path: &str, content: &str) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn copies_sources_and_skips_build_output() {
        let src = tempfile::tempdir().unwrap();
        write(src.path(), "Cargo.toml", "[package]");
        write(src.path(), "src/lib.rs", "pub fn f() {}");
        write(src.path(), "target/debug/big", "artifact");
        write(src.path(), "crates/a/target/CACHEDIR.TAG", "Signature");
        write(src.path(), "crates/a/target/debug/x", "artifact");
        write(
            src.path(),
            "crates/a/src/target/mod.rs",
            "// a module named target",
        );
        write(src.path(), "rust-toolchain.toml", "[toolchain]");

        let out = tempfile::tempdir().unwrap();
        let dst = out.path().join("work");
        let stats = copy_project(
            src.path(),
            &dst,
            &[PathBuf::from("rust-toolchain.toml")],
            1 << 20,
        )
        .unwrap();

        assert!(dst.join("src/lib.rs").is_file());
        assert!(dst.join("crates/a/src/target/mod.rs").is_file());
        assert!(!dst.join("target").exists());
        assert!(!dst.join("crates/a/target").exists());
        assert!(!dst.join("rust-toolchain.toml").exists());
        assert_eq!(stats.files, 3);
    }

    #[test]
    fn size_limit_is_enforced() {
        let src = tempfile::tempdir().unwrap();
        write(src.path(), "Cargo.toml", &"x".repeat(100));
        write(src.path(), "big.bin", &"x".repeat(1000));
        let out = tempfile::tempdir().unwrap();
        let err = copy_project(src.path(), &out.path().join("work"), &[], 500).unwrap_err();
        assert!(matches!(err, StageError::TooLarge { limit: 500 }));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_recreated_not_followed() {
        let secret_dir = tempfile::tempdir().unwrap();
        let secret = secret_dir.path().join("id_rsa");
        fs::write(&secret, "PRIVATE KEY").unwrap();

        let src = tempfile::tempdir().unwrap();
        write(src.path(), "Cargo.toml", "[package]");
        std::os::unix::fs::symlink(&secret, src.path().join("key")).unwrap();
        std::os::unix::fs::symlink(secret_dir.path(), src.path().join("dir")).unwrap();

        let out = tempfile::tempdir().unwrap();
        let dst = out.path().join("work");
        let stats = copy_project(src.path(), &dst, &[], 1 << 20).unwrap();

        let link = dst.join("key");
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(fs::read_link(&link).unwrap(), secret);
        assert!(
            dst.join("dir")
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(stats.files, 1, "only Cargo.toml was copied as a file");
        assert_eq!(stats.bytes, "[package]".len() as u64);
    }

    #[cfg(unix)]
    #[test]
    fn executable_bits_survive() {
        use std::os::unix::fs::PermissionsExt;
        let src = tempfile::tempdir().unwrap();
        write(src.path(), "Cargo.toml", "[package]");
        write(src.path(), "scripts/gen.sh", "#!/bin/sh");
        fs::set_permissions(
            src.path().join("scripts/gen.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let out = tempfile::tempdir().unwrap();
        let dst = out.path().join("work");
        copy_project(src.path(), &dst, &[], 1 << 20).unwrap();
        let mode = fs::metadata(dst.join("scripts/gen.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111);
    }
}
