//! Checks a Cargo project before anything in it runs.
//!
//! The sandbox fetches dependencies with network access and then builds with
//! none. Fetching must not run any code from the project, but a few settings
//! make cargo or rustup execute a program the project chooses:
//!
//! - `build.rustc`, `build.rustc-wrapper` and `build.rustc-workspace-wrapper`
//!   in `.cargo/config.toml`: cargo calls rustc during `cargo fetch` to learn
//!   about the target.
//! - credential providers in `.cargo/config.toml`, which cargo runs to
//!   authenticate to a registry.
//! - `rust-toolchain.toml`, which can make rustup download a toolchain or use
//!   one from a path inside the project.
//!
//! The first two are refused. Toolchain files are left out of the sandbox
//! copy instead, so the image's pinned toolchain is always used.
//!
//! Settings that only take effect while compiling or testing (linkers,
//! runners, `[env]`) are allowed: that code already runs without network,
//! like build scripts do.

use std::io;
use std::path::{Path, PathBuf};

use isobuild_report::{PolicyFinding, Severity};
use toml::{Table, Value};

/// Result of checking a project.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ProjectPolicy {
    pub findings: Vec<PolicyFinding>,
    /// Paths relative to the project root that must not be copied into the sandbox.
    pub excluded: Vec<PathBuf>,
}

impl ProjectPolicy {
    pub fn is_denied(&self) -> bool {
        self.findings.iter().any(|f| f.severity == Severity::Deny)
    }
}

const CONFIG_FILES: [&str; 2] = [".cargo/config.toml", ".cargo/config"];
const TOOLCHAIN_FILES: [&str; 2] = ["rust-toolchain.toml", "rust-toolchain"];

/// Check the project rooted at `root`.
///
/// Only the root's `.cargo` directory matters: the sandbox runs cargo from
/// the root, and the directories above it belong to the image.
pub fn check_project(root: &Path) -> io::Result<ProjectPolicy> {
    let mut policy = ProjectPolicy::default();

    for file in CONFIG_FILES {
        if is_symlink(&root.join(file)) {
            // A link can point at another file in the project that cargo would
            // read inside the sandbox. Refuse rather than chase it.
            policy.findings.push(deny(
                "cargo-config.symlink",
                file,
                "the cargo config is a symlink; replace it with a regular file so it can be checked",
            ));
            continue;
        }
        let Some(text) = read_if_exists(&root.join(file))? else {
            continue;
        };
        match text.parse::<Table>() {
            Ok(table) => check_cargo_config(&table, file, &mut policy.findings),
            Err(err) => policy.findings.push(deny(
                "cargo-config.unparsable",
                file,
                format!(
                    "could not parse the file, so it cannot be checked: {}",
                    err.message()
                ),
            )),
        }
    }

    for file in TOOLCHAIN_FILES {
        let path = root.join(file);
        let text = if is_symlink(&path) {
            Some(String::new())
        } else {
            read_if_exists(&path)?
        };
        let Some(text) = text else {
            continue;
        };
        let requested = match requested_toolchain(&text) {
            Some(toolchain) => format!("requests toolchain `{toolchain}`"),
            None => "selects a toolchain".to_string(),
        };
        policy.findings.push(PolicyFinding {
            severity: Severity::Warn,
            rule: "toolchain.file-ignored".into(),
            file: Some(file.into()),
            message: format!(
                "{file} {requested}; it is left out of the sandbox and the image's pinned toolchain is used instead"
            ),
        });
        policy.excluded.push(PathBuf::from(file));
    }

    Ok(policy)
}

fn check_cargo_config(config: &Table, file: &str, findings: &mut Vec<PolicyFinding>) {
    let build = table(config, "build");
    if build.is_some_and(|b| key(b, "rustc").is_some()) {
        findings.push(deny(
            "cargo-config.custom-rustc",
            file,
            "`build.rustc` replaces the compiler with a program chosen by the project, which cargo runs while fetching dependencies with network access. Remove it to use the sandbox's toolchain.",
        ));
    }
    for wrapper in ["rustc-wrapper", "rustc-workspace-wrapper"] {
        if build.is_some_and(|b| key(b, wrapper).is_some()) {
            findings.push(deny(
                "cargo-config.rustc-wrapper",
                file,
                format!("`build.{wrapper}` runs a program chosen by the project around every rustc call, including while fetching dependencies with network access. Remove it."),
            ));
        }
    }

    let mut providers = Vec::new();
    if let Some(registry) = table(config, "registry") {
        for name in ["credential-provider", "global-credential-providers"] {
            if key(registry, name).is_some() {
                providers.push(format!("registry.{name}"));
            }
        }
    }
    if let Some(registries) = table(config, "registries") {
        for (name, value) in registries {
            if value
                .as_table()
                .is_some_and(|r| key(r, "credential-provider").is_some())
            {
                providers.push(format!("registries.{name}.credential-provider"));
            }
        }
    }
    for provider in providers {
        findings.push(deny(
            "cargo-config.credential-provider",
            file,
            format!("`{provider}` makes cargo run a program to authenticate while fetching dependencies. The sandbox only supports public registries."),
        ));
    }
}

/// Look up `name`, accepting `snake_case` for kebab-case keys.
fn key<'a>(table: &'a Table, name: &str) -> Option<&'a Value> {
    table
        .get(name)
        .or_else(|| table.get(&name.replace('-', "_")))
}

fn table<'a>(config: &'a Table, name: &str) -> Option<&'a Table> {
    key(config, name)?.as_table()
}

fn deny(rule: &str, file: &str, message: impl Into<String>) -> PolicyFinding {
    PolicyFinding {
        severity: Severity::Deny,
        rule: rule.into(),
        file: Some(file.into()),
        message: message.into(),
    }
}

fn is_symlink(path: &Path) -> bool {
    path.symlink_metadata()
        .is_ok_and(|meta| meta.file_type().is_symlink())
}

fn read_if_exists(path: &Path) -> io::Result<Option<String>> {
    match path.symlink_metadata() {
        Ok(meta) if meta.is_file() => Ok(Some(std::fs::read_to_string(path)?)),
        Ok(_) => Ok(None),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// The channel named by a toolchain file, in either its TOML or legacy one-line form.
fn requested_toolchain(text: &str) -> Option<String> {
    if let Ok(table) = text.parse::<Table>() {
        let toolchain = table.get("toolchain")?.as_table()?;
        if let Some(channel) = toolchain.get("channel").and_then(Value::as_str) {
            return Some(channel.to_string());
        }
        return toolchain
            .get("path")
            .and_then(Value::as_str)
            .map(|path| format!("path:{path}"));
    }
    let line = text.lines().next()?.trim();
    (!line.is_empty()).then(|| line.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\n",
        )
        .unwrap();
        for (path, content) in files {
            let path = dir.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
        dir
    }

    fn rules(policy: &ProjectPolicy) -> Vec<(&str, Severity)> {
        policy
            .findings
            .iter()
            .map(|f| (f.rule.as_str(), f.severity))
            .collect()
    }

    #[test]
    fn plain_project_passes() {
        let dir = project(&[]);
        let policy = check_project(dir.path()).unwrap();
        assert_eq!(policy, ProjectPolicy::default());
        assert!(!policy.is_denied());
    }

    #[test]
    fn harmless_cargo_config_passes() {
        let dir = project(&[(
            ".cargo/config.toml",
            "[build]\njobs = 4\nrustflags = [\"-Dwarnings\"]\n[target.x86_64-unknown-linux-gnu]\nlinker = \"clang\"\nrunner = \"valgrind\"\n[env]\nFOO = \"bar\"\n",
        )]);
        let policy = check_project(dir.path()).unwrap();
        assert!(policy.findings.is_empty(), "{:?}", policy.findings);
    }

    #[test]
    fn compiler_overrides_are_denied() {
        let dir = project(&[(
            ".cargo/config.toml",
            "[build]\nrustc = \"./evil\"\nrustc-wrapper = \"./evil\"\nrustc_workspace_wrapper = \"./evil\"\n",
        )]);
        let policy = check_project(dir.path()).unwrap();
        assert!(policy.is_denied());
        assert_eq!(
            rules(&policy),
            [
                ("cargo-config.custom-rustc", Severity::Deny),
                ("cargo-config.rustc-wrapper", Severity::Deny),
                ("cargo-config.rustc-wrapper", Severity::Deny),
            ]
        );
        assert_eq!(
            policy.findings[0].file.as_deref(),
            Some(".cargo/config.toml")
        );
    }

    #[test]
    fn dotted_keys_are_seen() {
        let dir = project(&[(".cargo/config", "build.rustc-wrapper = \"sccache\"\n")]);
        let policy = check_project(dir.path()).unwrap();
        assert_eq!(
            rules(&policy),
            [("cargo-config.rustc-wrapper", Severity::Deny)]
        );
        assert_eq!(policy.findings[0].file.as_deref(), Some(".cargo/config"));
    }

    #[test]
    fn credential_providers_are_denied() {
        let dir = project(&[(
            ".cargo/config.toml",
            "[registry]\nglobal-credential-providers = [\"./helper\"]\n[registries.private]\nindex = \"sparse+https://example.com/\"\ncredential-provider = \"./helper\"\n",
        )]);
        let policy = check_project(dir.path()).unwrap();
        let messages: Vec<_> = policy.findings.iter().map(|f| f.message.as_str()).collect();
        assert_eq!(policy.findings.len(), 2);
        assert!(messages[0].contains("registry.global-credential-providers"));
        assert!(messages[1].contains("registries.private.credential-provider"));
    }

    #[test]
    fn unparsable_config_is_denied() {
        let dir = project(&[(".cargo/config.toml", "[build\nrustc = ")]);
        let policy = check_project(dir.path()).unwrap();
        assert_eq!(
            rules(&policy),
            [("cargo-config.unparsable", Severity::Deny)]
        );
    }

    #[test]
    fn toolchain_files_are_excluded_with_a_warning() {
        let dir = project(&[
            ("rust-toolchain.toml", "[toolchain]\nchannel = \"1.80.0\"\n"),
            ("rust-toolchain", "nightly-2024-01-01\n"),
        ]);
        let policy = check_project(dir.path()).unwrap();
        assert!(!policy.is_denied());
        assert_eq!(
            policy.excluded,
            [
                PathBuf::from("rust-toolchain.toml"),
                PathBuf::from("rust-toolchain")
            ]
        );
        assert!(policy.findings[0].message.contains("`1.80.0`"));
        assert!(policy.findings[1].message.contains("`nightly-2024-01-01`"));
    }

    #[test]
    fn toolchain_path_is_reported() {
        let dir = project(&[(
            "rust-toolchain.toml",
            "[toolchain]\npath = \"./my-rustc\"\n",
        )]);
        let policy = check_project(dir.path()).unwrap();
        assert!(policy.findings[0].message.contains("path:./my-rustc"));
        assert_eq!(policy.excluded, [PathBuf::from("rust-toolchain.toml")]);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_config_is_denied() {
        let dir = project(&[("conf/evil.toml", "[build]\nrustc = \"./evil\"\n")]);
        fs::create_dir(dir.path().join(".cargo")).unwrap();
        std::os::unix::fs::symlink("../conf/evil.toml", dir.path().join(".cargo/config.toml"))
            .unwrap();
        let policy = check_project(dir.path()).unwrap();
        assert_eq!(rules(&policy), [("cargo-config.symlink", Severity::Deny)]);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_toolchain_file_is_excluded() {
        let dir = project(&[("elsewhere.toml", "[toolchain]\nchannel = \"nightly\"\n")]);
        std::os::unix::fs::symlink("elsewhere.toml", dir.path().join("rust-toolchain.toml"))
            .unwrap();
        let policy = check_project(dir.path()).unwrap();
        assert!(!policy.is_denied());
        assert_eq!(policy.excluded, [PathBuf::from("rust-toolchain.toml")]);
    }
}
