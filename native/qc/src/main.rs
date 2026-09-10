mod config;
mod dedupe;
mod filter;
mod outline;
mod repomap;
mod repomap_client;
mod repomap_daemon;
mod repomap_index;
mod repomap_protocol;
mod runner;
mod util;

use clap::{Parser, Subcommand};
use serde::Serialize;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

const NATIVE_PROTOCOL_VERSION: u32 = 1;

#[derive(Parser)]
#[command(name = "qc-native", version, about = "QuietContext native engine")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run one argv-safe command and return a bounded structured result.
    Run {
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        argv: Vec<OsString>,
    },
    /// Query the native repository structural index.
    Repo {
        #[command(subcommand)]
        action: RepoAction,
    },
    /// Report the native protocol/version contract.
    Status,
}

#[derive(Subcommand)]
enum RepoAction {
    Map {
        #[arg(long)]
        root: Option<PathBuf>,
    },
    Symbol {
        query: String,
        #[arg(long)]
        root: Option<PathBuf>,
    },
    References {
        query: String,
        #[arg(long)]
        root: Option<PathBuf>,
    },
    Outline {
        path: PathBuf,
        #[arg(long)]
        root: Option<PathBuf>,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NativeStatus<'a> {
    protocol_version: u32,
    native_version: &'a str,
    product: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StreamReceipt {
    compact: String,
    raw_path: String,
    raw_bytes: usize,
    raw_complete: bool,
    sample_complete: bool,
    compact_bytes: usize,
    capped: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RunReceipt {
    protocol_version: u32,
    native_version: &'static str,
    kind: &'static str,
    command: Vec<String>,
    exit_code: i32,
    filter: String,
    filter_input_bytes: usize,
    filtered: bool,
    deduped: bool,
    stdout: StreamReceipt,
    stderr: StreamReceipt,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RepoReceipt {
    protocol_version: u32,
    native_version: &'static str,
    kind: &'static str,
    operation: String,
    root: String,
    query: Option<String>,
    stdout: String,
    stderr: String,
    exit_code: i32,
    generation: u64,
    cache_state: String,
    fallback_reason: Option<String>,
    timings: repomap_protocol::LookupTimings,
    total_latency_us: u64,
}

fn main() {
    if repomap_client::daemon_mode_active() {
        if let Err(error) = repomap_daemon::run_daemon() {
            eprintln!("qc-native: repository daemon: {error}");
            std::process::exit(1);
        }
        return;
    }

    let cli = Cli::parse();
    match cli.command {
        Command::Run { argv } => run(argv),
        Command::Repo { action } => repo(action),
        Command::Status => {
            print_json(&NativeStatus {
                protocol_version: NATIVE_PROTOCOL_VERSION,
                native_version: env!("CARGO_PKG_VERSION"),
                product: "QuietContext",
            });
        }
    }
}

fn print_json<T: Serialize>(value: &T) {
    match serde_json::to_string(value) {
        Ok(json) => println!("{json}"),
        Err(error) => {
            eprintln!("qc-native: failed to serialize response: {error}");
            std::process::exit(70);
        }
    }
}

fn command_name(arg: &OsStr) -> String {
    Path::new(arg)
        .file_name()
        .unwrap_or(arg)
        .to_string_lossy()
        .into_owned()
}

fn run(argv: Vec<OsString>) {
    if argv.is_empty() {
        eprintln!("qc-native: no command specified");
        std::process::exit(2);
    }
    let binary = &argv[0];
    let name = command_name(binary);
    if matches!(name.as_str(), "qc" | "qc-native") {
        eprintln!("qc-native: refusing recursive wrapper invocation of {name}");
        std::process::exit(2);
    }

    let args_os = argv[1..].to_vec();
    let args: Vec<String> = args_os
        .iter()
        .map(|v| v.to_string_lossy().into_owned())
        .collect();
    let result = runner::run_command(binary.as_os_str(), &args_os);
    let cfg = config::Config::load();
    let filter_cfg = filter::FilterConfig::from(&cfg.limits);

    let input = filter::FilterInput {
        stdout: &result.stdout.sample,
        stderr: &result.stderr.sample,
        exit_code: result.exit_code,
        command: &name,
        args: &args,
    };

    // Command-specific parsers are only safe over a complete captured sample.
    // If a command exceeds the filter sample budget, fall back to the generic
    // structural reducer and mark sampleComplete=false in the receipt. Exact raw
    // evidence remains in the spool when rawComplete=true.
    let filtered = if util::should_filter() {
        let selected: Box<dyn filter::OutputFilter> = if result.stdout.sample_complete {
            filter::FilterRegistry::detect(&name, &args)
        } else {
            Box::new(filter::GenericFilter)
        };
        selected.filter(&input, &filter_cfg)
    } else {
        filter::FilterResult {
            output: String::from_utf8_lossy(&result.stdout.sample).into_owned(),
            input_bytes: result.stdout.raw_bytes,
        }
    };

    let filter_input_bytes = filtered.input_bytes;
    let mut compact_stdout = filtered.output;
    if matches!(std::env::var("QUIET_CONTEXT_HINTS").as_deref(), Ok("1"))
        && filter_input_bytes > filter_cfg.hint_threshold
        && result.stdout.sample_complete
    {
        let hint_filter = filter::FilterRegistry::detect(&name, &args);
        if let Some(hint) = hint_filter.hint(&input) {
            if !compact_stdout.ends_with('\n') {
                compact_stdout.push('\n');
            }
            compact_stdout.push_str("# qc-hint: ");
            compact_stdout.push_str(&hint);
            compact_stdout.push('\n');
        }
    }
    let pre_dedupe = compact_stdout.clone();
    let deduped =
        dedupe::maybe_dedupe(&cfg.dedupe, &name, &args, result.exit_code, &compact_stdout)
            .map(|marker| {
                compact_stdout = marker;
                true
            })
            .unwrap_or(false);

    let before_cap = compact_stdout.len();
    if !util::cap_bypassed() {
        compact_stdout =
            util::apply_global_cap(compact_stdout, cfg.limits.global_max_sent_bytes, "stdout");
    }
    let stdout_capped = compact_stdout.len() != before_cap || !result.stdout.sample_complete;

    let mut compact_stderr = String::from_utf8_lossy(&result.stderr.sample).into_owned();
    let stderr_before_cap = compact_stderr.len();
    if !util::cap_bypassed() {
        compact_stderr =
            util::apply_global_cap(compact_stderr, cfg.limits.global_max_sent_bytes, "stderr");
    }
    let stderr_capped = compact_stderr.len() != stderr_before_cap || !result.stderr.sample_complete;

    let filtered_changed =
        pre_dedupe.as_bytes() != result.stdout.sample.as_slice() || !result.stdout.sample_complete;
    let command_display = argv
        .iter()
        .map(|v| v.to_string_lossy().into_owned())
        .collect();
    let receipt = RunReceipt {
        protocol_version: NATIVE_PROTOCOL_VERSION,
        native_version: env!("CARGO_PKG_VERSION"),
        kind: "run",
        command: command_display,
        exit_code: result.exit_code,
        filter: if result.stdout.sample_complete {
            name
        } else {
            "generic-partial".to_owned()
        },
        filter_input_bytes,
        filtered: filtered_changed,
        deduped,
        stdout: StreamReceipt {
            compact_bytes: compact_stdout.len(),
            compact: compact_stdout,
            raw_path: result.stdout.raw_path.to_string_lossy().into_owned(),
            raw_bytes: result.stdout.raw_bytes,
            raw_complete: result.stdout.raw_complete,
            sample_complete: result.stdout.sample_complete,
            capped: stdout_capped,
        },
        stderr: StreamReceipt {
            compact_bytes: compact_stderr.len(),
            compact: compact_stderr,
            raw_path: result.stderr.raw_path.to_string_lossy().into_owned(),
            raw_bytes: result.stderr.raw_bytes,
            raw_complete: result.stderr.raw_complete,
            sample_complete: result.stderr.sample_complete,
            capped: stderr_capped,
        },
    };
    print_json(&receipt);
}

fn canonical_root(root: Option<PathBuf>) -> Result<PathBuf, String> {
    std::fs::canonicalize(root.unwrap_or_else(|| PathBuf::from(".")))
        .map_err(|error| format!("cannot resolve repository root: {error}"))
}

fn repo(action: RepoAction) {
    let cfg = config::Config::load();
    match action {
        RepoAction::Outline { path, root } => {
            let root = match canonical_root(root) {
                Ok(root) => root,
                Err(error) => return repo_error("outline", "", None, error),
            };
            let candidate = if path.is_absolute() {
                path
            } else {
                root.join(path)
            };
            let canonical = match std::fs::canonicalize(&candidate) {
                Ok(path) => path,
                Err(error) => {
                    return repo_error(
                        "outline",
                        &root.to_string_lossy(),
                        None,
                        format!("cannot resolve path: {error}"),
                    )
                }
            };
            if !canonical.starts_with(&root) {
                return repo_error(
                    "outline",
                    &root.to_string_lossy(),
                    None,
                    "path escapes repository root".to_owned(),
                );
            }
            let relative = canonical
                .strip_prefix(&root)
                .unwrap_or(&canonical)
                .to_string_lossy()
                .into_owned();
            match outline::build_outline(&canonical, &relative, cfg.outline.max_bytes) {
                Some(stdout) => print_json(&RepoReceipt {
                    protocol_version: NATIVE_PROTOCOL_VERSION,
                    native_version: env!("CARGO_PKG_VERSION"),
                    kind: "repo",
                    operation: "outline".to_owned(),
                    root: root.to_string_lossy().into_owned(),
                    query: Some(relative),
                    stdout,
                    stderr: String::new(),
                    exit_code: 0,
                    generation: 0,
                    cache_state: "direct".to_owned(),
                    fallback_reason: None,
                    timings: repomap_protocol::LookupTimings::default(),
                    total_latency_us: 0,
                }),
                None => repo_error(
                    "outline",
                    &root.to_string_lossy(),
                    Some(relative),
                    "no declarations found".to_owned(),
                ),
            }
        }
        RepoAction::Map { root } => {
            lookup_repo(repomap_protocol::LookupOperation::Map, None, root, &cfg.map)
        }
        RepoAction::Symbol { query, root } => lookup_repo(
            repomap_protocol::LookupOperation::Sym,
            Some(query),
            root,
            &cfg.map,
        ),
        RepoAction::References { query, root } => lookup_repo(
            repomap_protocol::LookupOperation::Refs,
            Some(query),
            root,
            &cfg.map,
        ),
    }
}

fn lookup_repo(
    operation: repomap_protocol::LookupOperation,
    query: Option<String>,
    root: Option<PathBuf>,
    map_cfg: &config::MapConfig,
) {
    let root = match canonical_root(root) {
        Ok(root) => root,
        Err(error) => return repo_error(operation.as_str(), "", query, error),
    };
    let request = repomap_protocol::LookupRequest {
        version: repomap_protocol::PROTOCOL_VERSION,
        operation,
        canonical_root: root.to_string_lossy().into_owned(),
        query: query.clone(),
        map_config: repomap_protocol::EffectiveMapConfig::from(map_cfg),
    };
    let outcome = repomap_client::lookup(request, repomap_client::ClientBudget::default());
    let response = outcome.response;
    print_json(&RepoReceipt {
        protocol_version: NATIVE_PROTOCOL_VERSION,
        native_version: env!("CARGO_PKG_VERSION"),
        kind: "repo",
        operation: operation.as_str().to_owned(),
        root: root.to_string_lossy().into_owned(),
        query,
        stdout: response.stdout,
        stderr: response.stderr,
        exit_code: response.exit_code,
        generation: response.generation,
        cache_state: response.status.as_str().to_owned(),
        fallback_reason: outcome.fallback_reason.map(|v| v.as_str().to_owned()),
        timings: response.timings,
        total_latency_us: outcome.latency.as_micros().min(u64::MAX as u128) as u64,
    });
}

fn repo_error(operation: &str, root: &str, query: Option<String>, error: String) {
    print_json(&RepoReceipt {
        protocol_version: NATIVE_PROTOCOL_VERSION,
        native_version: env!("CARGO_PKG_VERSION"),
        kind: "repo",
        operation: operation.to_owned(),
        root: root.to_owned(),
        query,
        stdout: String::new(),
        stderr: format!("qc repo {operation}: {error}\n"),
        exit_code: 1,
        generation: 0,
        cache_state: "error".to_owned(),
        fallback_reason: None,
        timings: repomap_protocol::LookupTimings::default(),
        total_latency_us: 0,
    });
}
