use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};
use isobuild::cache::{CacheLayout, remove_tree};
use isobuild::{CargoCommand, CargoOptions, RunRequest, default_backend, kill_active, render, run};
use isobuild_report::{Limits, Status};

/// Build and test untrusted Rust projects in a sandbox.
///
/// Dependencies are fetched with network access, then the project is built
/// and tested without any. The result is a JSON report on stdout.
///
/// Exit codes: 0 ok, 1 the project failed to fetch, build or test,
/// 2 the request or project was refused, 3 a time, memory or process limit
/// was hit, 4 the sandbox itself failed.
#[derive(Parser)]
#[command(name = "cargo-sandbox", bin_name = "cargo sandbox", version)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run `cargo check`.
    Check(RunArgs),
    /// Run `cargo clippy`.
    Clippy(RunArgs),
    /// Run `cargo build`.
    Build(RunArgs),
    /// Run `cargo test`. Arguments after `--` go to the test binaries.
    Test {
        #[command(flatten)]
        args: RunArgs,
        #[arg(last = true)]
        test_args: Vec<String>,
    },
    /// Delete the cache: dependency downloads, build caches and run logs.
    Clean {
        #[arg(long, env = "ISOBUILD_CACHE")]
        cache_dir: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    Json,
    Human,
}

#[derive(Args)]
struct RunArgs {
    /// Project directory, containing the root Cargo.toml.
    #[arg(long, default_value = ".")]
    project: PathBuf,
    /// Output format.
    #[arg(long, value_enum, default_value_t = Format::Json)]
    format: Format,
    /// Rustup toolchain, e.g. `stable` or `1.91.1` [default: the host's default].
    /// The project's rust-toolchain.toml is ignored.
    #[arg(long, env = "ISOBUILD_TOOLCHAIN")]
    toolchain: Option<String>,
    /// Cache directory [default: ~/.cache/isobuild].
    #[arg(long, env = "ISOBUILD_CACHE")]
    cache_dir: Option<PathBuf>,
    /// Runs with the same scope share compiled artifacts [default: the project path].
    #[arg(long)]
    cache_scope: Option<String>,
    /// Compile from scratch in a throwaway target directory.
    #[arg(long)]
    no_cache: bool,
    /// Skip fetching; build only from dependencies already cached.
    #[arg(long)]
    offline: bool,
    /// Fail instead of updating Cargo.lock.
    #[arg(long)]
    locked: bool,
    #[arg(long)]
    release: bool,
    /// Features to enable, comma separated.
    #[arg(long, short = 'F', value_delimiter = ',')]
    features: Vec<String>,
    #[arg(long)]
    all_features: bool,
    #[arg(long)]
    no_default_features: bool,
    /// Package to build (repeatable).
    #[arg(long, short = 'p')]
    package: Vec<String>,
    #[arg(long)]
    workspace: bool,
    #[arg(long)]
    all_targets: bool,
    /// Environment variable for the build and test phases, as KEY=VALUE (repeatable).
    /// Nothing is inherited from the host.
    #[arg(long = "env", value_name = "KEY=VALUE", value_parser = parse_env)]
    env: Vec<(String, String)>,
    #[arg(long, default_value_t = Limits::default().memory_mb)]
    memory_mb: u64,
    #[arg(long, default_value_t = Limits::default().cpus)]
    cpus: f64,
    /// Maximum number of processes and threads.
    #[arg(long, default_value_t = Limits::default().pids)]
    pids: u32,
    #[arg(long, value_name = "SECS", default_value_t = Limits::default().fetch_timeout_secs)]
    fetch_timeout: u64,
    #[arg(long, value_name = "SECS", default_value_t = Limits::default().build_timeout_secs)]
    build_timeout: u64,
    #[arg(long, value_name = "SECS", default_value_t = Limits::default().test_timeout_secs)]
    test_timeout: u64,
    /// Maximum number of diagnostics in the report.
    #[arg(long, default_value_t = 50)]
    max_diagnostics: usize,
    /// Keep the staged copy of the project in the run directory.
    #[arg(long)]
    keep_staging: bool,
}

fn parse_env(value: &str) -> Result<(String, String), String> {
    value
        .split_once('=')
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .ok_or_else(|| format!("expected KEY=VALUE, got `{value}`"))
}

impl RunArgs {
    fn into_request(self, command: CargoCommand, test_args: Vec<String>) -> (RunRequest, Format) {
        let cache_dir = self.cache_dir.unwrap_or_else(CacheLayout::default_root);
        let mut req = RunRequest::new(command, self.project, cache_dir);
        req.cargo = CargoOptions {
            release: self.release,
            features: self.features,
            all_features: self.all_features,
            no_default_features: self.no_default_features,
            packages: self.package,
            workspace: self.workspace,
            all_targets: self.all_targets,
            locked: self.locked,
        };
        req.test_args = test_args;
        req.env = self.env;
        req.toolchain = self.toolchain;
        req.limits = Limits {
            memory_mb: self.memory_mb,
            cpus: self.cpus,
            pids: self.pids,
            fetch_timeout_secs: self.fetch_timeout,
            build_timeout_secs: self.build_timeout,
            test_timeout_secs: self.test_timeout,
        };
        req.offline = self.offline;
        req.cache_scope = self.cache_scope;
        req.build_cache = !self.no_cache;
        req.keep_staging = self.keep_staging;
        req.max_diagnostics = self.max_diagnostics;
        (req, self.format)
    }
}

fn exit_code(status: Status) -> u8 {
    match status {
        Status::Ok => 0,
        Status::FetchFailed | Status::BuildFailed | Status::TestFailed => 1,
        Status::InvalidRequest | Status::PolicyDenied => 2,
        Status::Timeout | Status::Oom | Status::ProcessLimit => 3,
        Status::InfraError => 4,
    }
}

fn main() -> ExitCode {
    // `cargo sandbox …` runs us as `cargo-sandbox sandbox …`.
    let mut argv: Vec<_> = std::env::args_os().collect();
    if argv.get(1).is_some_and(|a| a == "sandbox") {
        argv.remove(1);
    }
    let cli = Cli::parse_from(argv);

    let (req, format) = match cli.command {
        Cmd::Check(args) => args.into_request(CargoCommand::Check, Vec::new()),
        Cmd::Clippy(args) => args.into_request(CargoCommand::Clippy, Vec::new()),
        Cmd::Build(args) => args.into_request(CargoCommand::Build, Vec::new()),
        Cmd::Test { args, test_args } => args.into_request(CargoCommand::Test, test_args),
        Cmd::Clean { cache_dir } => {
            return clean(cache_dir.unwrap_or_else(CacheLayout::default_root));
        }
    };

    let _ = ctrlc::set_handler(|| {
        kill_active();
        std::process::exit(130);
    });

    let report = run(&req, default_backend().as_ref());
    match format {
        Format::Json => match serde_json::to_string_pretty(&report) {
            Ok(json) => println!("{json}"),
            Err(err) => {
                eprintln!("cargo-sandbox: could not serialise the report: {err}");
                return ExitCode::from(4);
            }
        },
        Format::Human => print!("{}", render::human(&report)),
    }
    ExitCode::from(exit_code(report.status))
}

fn clean(root: PathBuf) -> ExitCode {
    let layout = CacheLayout::new(&root);
    if !root.exists() {
        return ExitCode::SUCCESS;
    }
    if !layout.is_cache_dir() {
        eprintln!(
            "cargo-sandbox: refusing to delete {}: it is not an isobuild cache",
            root.display()
        );
        return ExitCode::from(2);
    }
    match remove_tree(&root) {
        Ok(()) => {
            eprintln!("removed {}", root.display());
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("cargo-sandbox: could not remove {}: {err}", root.display());
            ExitCode::from(4)
        }
    }
}
