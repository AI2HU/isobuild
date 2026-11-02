//! Layout of the host-side cache directory.
//!
//! ```text
//! <root>/
//!   cargo-home/            CARGO_HOME for the fetch phase (read-write)
//!     registry/ git/       read-only in the build and test phases
//!   fetch.lock             serialises fetches that write to cargo-home
//!   targets/<key>/         one build cache
//!     target/              CARGO_TARGET_DIR (read-write)
//!     .isobuild-last-used  stamp for pruning, out of the sandbox's reach
//!   runs/<id>/             logs of one run, plus its sandbox directories while it runs
//! ```
//!
//! Trust rules: only the fetch phase writes to `cargo-home`, and it runs
//! nothing but cargo. A `targets/<key>` directory is written by project code,
//! so it is only ever reused by runs of the same scope.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

/// Marker that tells `clean` a directory really is an isobuild cache.
const MARKER: &str = "ISOBUILD_CACHE";
const LAST_USED: &str = ".isobuild-last-used";

#[derive(Debug, Clone)]
pub struct CacheLayout {
    root: PathBuf,
}

impl CacheLayout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        CacheLayout { root: root.into() }
    }

    /// `$ISOBUILD_CACHE`, else `$XDG_CACHE_HOME/isobuild`, else `~/.cache/isobuild`.
    pub fn default_root() -> PathBuf {
        if let Some(dir) = std::env::var_os("ISOBUILD_CACHE") {
            return PathBuf::from(dir);
        }
        if let Some(dir) = std::env::var_os("XDG_CACHE_HOME") {
            return PathBuf::from(dir).join("isobuild");
        }
        let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
        home.join(".cache").join("isobuild")
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn cargo_home(&self) -> PathBuf {
        self.root.join("cargo-home")
    }

    pub fn registry(&self) -> PathBuf {
        self.cargo_home().join("registry")
    }

    pub fn git(&self) -> PathBuf {
        self.cargo_home().join("git")
    }

    /// The build cache `key`: its target directory and last-use stamp.
    pub fn target(&self, key: &str) -> PathBuf {
        self.root.join("targets").join(key)
    }

    /// The target directory of the build cache `key`.
    pub fn target_dir(&self, key: &str) -> PathBuf {
        self.target(key).join("target")
    }

    pub fn runs(&self) -> PathBuf {
        self.root.join("runs")
    }

    /// Create the directory tree. Safe to call concurrently.
    pub fn ensure(&self) -> io::Result<()> {
        for dir in [
            self.registry(),
            self.git(),
            self.root.join("targets"),
            self.runs(),
        ] {
            fs::create_dir_all(dir)?;
        }
        let marker = self.root.join(MARKER);
        if !marker.exists() {
            fs::write(marker, "This directory is managed by isobuild.\n")?;
        }
        Ok(())
    }

    /// True when `root` holds an isobuild cache (or nothing at all).
    pub fn is_cache_dir(&self) -> bool {
        self.root.join(MARKER).is_file()
    }

    /// Create a fresh, uniquely named run directory.
    pub fn new_run_dir(&self) -> io::Result<(String, PathBuf)> {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let id = format!(
            "{}-{:x}-{:x}",
            now.as_secs(),
            std::process::id(),
            now.subsec_nanos() ^ COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let dir = self.runs().join(&id);
        fs::create_dir(&dir)?;
        Ok((id, dir))
    }

    /// Take the lock that serialises writers of `cargo-home`. Released on drop.
    pub fn lock_fetch(&self) -> io::Result<File> {
        let file = File::create(self.root.join("fetch.lock"))?;
        file.lock()?;
        Ok(file)
    }

    /// Record that a build cache was used, for [`CacheLayout::prune`].
    pub fn touch_target(&self, key: &str) -> io::Result<()> {
        fs::write(self.target(key).join(LAST_USED), "")
    }

    /// Delete run logs older than `runs_older_than` and build caches unused
    /// for `targets_older_than`.
    pub fn prune(&self, runs_older_than: Duration, targets_older_than: Duration) -> Pruned {
        let runs = remove_stale(&self.runs(), runs_older_than, |dir| dir.to_path_buf());
        let targets = remove_stale(&self.root.join("targets"), targets_older_than, |dir| {
            dir.join(LAST_USED)
        });
        Pruned {
            runs: runs.len(),
            targets,
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Pruned {
    pub runs: usize,
    pub targets: Vec<String>,
}

/// Remove stale subdirectories of `parent`, returning their names.
fn remove_stale(
    parent: &Path,
    older_than: Duration,
    stamp: impl Fn(&Path) -> PathBuf,
) -> Vec<String> {
    let Ok(entries) = fs::read_dir(parent) else {
        return Vec::new();
    };
    let now = SystemTime::now();
    let mut removed = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        // Fall back to the directory's own time if the stamp is missing.
        let modified = fs::metadata(stamp(&dir))
            .or_else(|_| fs::metadata(&dir))
            .and_then(|m| m.modified());
        let stale = match modified {
            Ok(time) => now.duration_since(time).unwrap_or_default() > older_than,
            Err(_) => false,
        };
        if stale && remove_tree(&dir).is_ok() {
            removed.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    removed
}

/// Delete `path` and everything under it without following symlinks, even
/// when sandboxed code removed write permission from directories inside it.
pub fn remove_tree(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(_) => {
            make_writable(path);
            fs::remove_dir_all(path)
        }
        ok => ok,
    }
}

fn make_writable(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if !fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir()) {
        return;
    }
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        make_writable(&entry.path());
    }
}

/// Key of the build cache for a scope and a toolchain.
///
/// A toolchain upgrade starts a new cache instead of mixing artifacts.
/// Cargo's own fingerprints take care of feature, profile and dependency
/// changes within one cache.
pub fn build_cache_key(scope: &str, toolchain_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(scope.as_bytes());
    hasher.update([0]);
    hasher.update(toolchain_id.as_bytes());
    let digest = hasher.finalize();
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// Default scope: the project's canonical path.
pub fn default_scope(project: &Path) -> String {
    let path = project
        .canonicalize()
        .unwrap_or_else(|_| project.to_path_buf());
    format!("path:{}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_stable_and_separate_scopes_and_images() {
        let key = build_cache_key("path:/a", "sha256:1");
        assert_eq!(key.len(), 32);
        assert_eq!(key, build_cache_key("path:/a", "sha256:1"));
        assert_ne!(key, build_cache_key("path:/b", "sha256:1"));
        assert_ne!(key, build_cache_key("path:/a", "sha256:2"));
        // The separator keeps ("ab", "c") and ("a", "bc") apart.
        assert_ne!(build_cache_key("ab", "c"), build_cache_key("a", "bc"));
    }

    #[test]
    fn ensure_creates_layout_and_marker() {
        let dir = tempfile::tempdir().unwrap();
        let layout = CacheLayout::new(dir.path().join("cache"));
        assert!(!layout.is_cache_dir());
        layout.ensure().unwrap();
        layout.ensure().unwrap();
        assert!(layout.is_cache_dir());
        assert!(layout.registry().is_dir());
        assert!(layout.git().is_dir());
        assert!(layout.runs().is_dir());
    }

    #[test]
    fn run_dirs_are_unique() {
        let dir = tempfile::tempdir().unwrap();
        let layout = CacheLayout::new(dir.path());
        layout.ensure().unwrap();
        let (a, _) = layout.new_run_dir().unwrap();
        let (b, _) = layout.new_run_dir().unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn prune_removes_only_stale_entries() {
        let dir = tempfile::tempdir().unwrap();
        let layout = CacheLayout::new(dir.path());
        layout.ensure().unwrap();
        let (_, run) = layout.new_run_dir().unwrap();
        fs::create_dir_all(layout.target("k")).unwrap();
        layout.touch_target("k").unwrap();
        fs::create_dir_all(layout.target("never-touched")).unwrap();

        assert_eq!(
            layout.prune(Duration::from_secs(3600), Duration::from_secs(3600)),
            Pruned::default()
        );
        assert!(run.exists());

        let mut pruned = layout.prune(Duration::ZERO, Duration::ZERO);
        pruned.targets.sort();
        assert_eq!(
            pruned,
            Pruned {
                runs: 1,
                targets: vec!["k".into(), "never-touched".into()]
            }
        );
        assert!(!run.exists());
        assert!(!layout.target("k").exists());
        assert!(!layout.target("never-touched").exists());
    }

    #[test]
    fn remove_tree_handles_read_only_directories() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("tree");
        fs::create_dir_all(tree.join("locked/inner")).unwrap();
        fs::write(tree.join("locked/inner/file"), "x").unwrap();
        let outside = dir.path().join("outside");
        fs::write(&outside, "keep").unwrap();
        std::os::unix::fs::symlink(&outside, tree.join("locked/link")).unwrap();
        for d in ["locked/inner", "locked"] {
            fs::set_permissions(tree.join(d), fs::Permissions::from_mode(0o500)).unwrap();
        }
        remove_tree(&tree).unwrap();
        assert!(!tree.exists());
        assert_eq!(
            fs::read_to_string(&outside).unwrap(),
            "keep",
            "links are not followed"
        );
        remove_tree(&tree).unwrap();
    }

    #[test]
    fn fetch_lock_can_be_taken_again_after_release() {
        let dir = tempfile::tempdir().unwrap();
        let layout = CacheLayout::new(dir.path());
        layout.ensure().unwrap();
        drop(layout.lock_fetch().unwrap());
        drop(layout.lock_fetch().unwrap());
    }
}
