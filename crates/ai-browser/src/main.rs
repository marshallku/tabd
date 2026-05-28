mod browser;
mod cdp;
mod cli;
mod cmd;
mod daemon;
mod secrets;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::ffi::OsString;
use std::process::ExitCode;

#[derive(Subcommand)]
enum DaemonCmd {
    /// Run the daemon in the foreground (blocks until SIGTERM or daemon.shutdown).
    Start {
        /// Override base directory. Defaults to $AI_BROWSER_BASE_DIR or $XDG_RUNTIME_DIR/ai-browser-rs.
        #[arg(long)]
        base_dir: Option<String>,
    },
    /// Send daemon.shutdown to a running daemon.
    Stop {
        #[arg(long)]
        base_dir: Option<String>,
    },
    /// Send daemon.ping. Prints raw JSON response.
    Ping {
        #[arg(long)]
        base_dir: Option<String>,
    },
    /// Send daemon.health. Prints raw JSON response.
    Health {
        #[arg(long)]
        base_dir: Option<String>,
    },
}

/// Legacy in-process subcommands from phase 0~1. Each spawns its own Chromium
/// for a single one-shot operation — no daemon needed. These are kept under
/// `_legacy` so the spike-parity smoke (53 cases, including query-all /
/// find-all / AX paths that aren't in Tier 1) keeps working. Once Tier 3~5
/// daemon actions land (3c~3e), most of these become removable in 3i.
#[derive(Subcommand)]
enum LegacyCmd {
    Navigate {
        url: String,
        #[arg(long, default_value_t = 30_000)]
        timeout: u64,
    },
    Eval {
        url: String,
        expr: String,
        #[arg(long)]
        json: bool,
        #[arg(long, default_value_t = 30_000)]
        timeout: u64,
    },
    FetchText {
        url: String,
        selector: String,
        #[arg(long, default_value_t = 30_000)]
        timeout: u64,
    },
    GetText {
        url: String,
        #[arg(long, group = "gt_target")]
        selector: Option<String>,
        #[arg(long, group = "gt_target")]
        testid: Option<String>,
        #[arg(long, group = "gt_target")]
        role: Option<String>,
        #[arg(long, requires = "role")]
        name: Option<String>,
        #[arg(long)]
        raw: bool,
        #[arg(long, default_value_t = 30_000)]
        timeout: u64,
    },
    QueryAll {
        url: String,
        #[arg(long, group = "qa_target")]
        selector: Option<String>,
        #[arg(long, group = "qa_target")]
        testid: Option<String>,
        #[arg(long, group = "qa_target")]
        role: Option<String>,
        #[arg(long, requires = "role")]
        name: Option<String>,
        #[arg(long)]
        raw: bool,
        #[arg(long, default_value_t = 100)]
        limit: u32,
        #[arg(long, default_value_t = 30_000)]
        timeout: u64,
    },
    FindAll {
        url: String,
        #[arg(long, group = "fa_target")]
        selector: Option<String>,
        #[arg(long, group = "fa_target")]
        testid: Option<String>,
        #[arg(long, group = "fa_target")]
        role: Option<String>,
        #[arg(long, requires = "role")]
        name: Option<String>,
        #[arg(long)]
        raw: bool,
        #[arg(long, default_value_t = 100)]
        limit: u32,
        #[arg(long, default_value_t = 30_000)]
        timeout: u64,
    },
}

#[derive(Parser)]
#[command(name = "ai-browser", about = "Rust + Chromium CDP browser controller")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// TS-protocol-compatible daemon over UDS (start/stop/ping/health).
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
    },
    /// Phase 0~1 legacy in-process commands. Keep for spike-parity coverage.
    #[command(name = "_legacy")]
    Legacy {
        #[command(subcommand)]
        cmd: LegacyCmd,
    },
    /// Catch-all for TS-CLI-compatible subcommands (navigate, get-text, click,
    /// etc.). Routed through the daemon — auto-spawned if needed. See
    /// `src/cli.rs` for the dispatch table.
    #[command(external_subcommand)]
    Other(Vec<OsString>),
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let code: i32 = match cli.command {
        Command::Daemon { cmd } => match run_daemon_cmd(cmd).await {
            Ok(()) => 0,
            Err(err) => {
                eprintln!("error: {err:#}");
                1
            }
        },
        Command::Legacy { cmd } => match run_legacy_cmd(cmd).await {
            Ok(()) => 0,
            Err(err) => {
                eprintln!("error: {err:#}");
                1
            }
        },
        Command::Other(args) => match cli::run(args).await {
            Ok(code) => code,
            Err(err) => {
                eprintln!("error: {err:#}");
                1
            }
        },
    };
    ExitCode::from(code.clamp(0, 255) as u8)
}

async fn run_daemon_cmd(cmd: DaemonCmd) -> Result<()> {
    match cmd {
        DaemonCmd::Start { base_dir } => daemon::run(base_dir.as_deref()).await,
        DaemonCmd::Stop { base_dir } => print_control(base_dir.as_deref(), "daemon.shutdown").await,
        DaemonCmd::Ping { base_dir } => print_control(base_dir.as_deref(), "daemon.ping").await,
        DaemonCmd::Health { base_dir } => print_control(base_dir.as_deref(), "daemon.health").await,
    }
}

async fn run_legacy_cmd(cmd: LegacyCmd) -> Result<()> {
    match cmd {
        LegacyCmd::Navigate { url, timeout } => cmd::navigate::run(&url, timeout).await,
        LegacyCmd::Eval { url, expr, json, timeout } => {
            cmd::eval::run(&url, &expr, json, timeout).await
        }
        LegacyCmd::FetchText { url, selector, timeout } => {
            cmd::fetch_text::run(&url, &selector, timeout).await
        }
        LegacyCmd::GetText { url, selector, testid, role, name, raw, timeout } => {
            cmd::get_text::run(
                &url,
                selector.as_deref(),
                testid.as_deref(),
                role.as_deref(),
                name.as_deref(),
                raw,
                timeout,
            )
            .await
        }
        LegacyCmd::QueryAll { url, selector, testid, role, name, raw, limit, timeout } => {
            cmd::query_all::run(
                &url,
                selector.as_deref(),
                testid.as_deref(),
                role.as_deref(),
                name.as_deref(),
                raw,
                limit,
                timeout,
            )
            .await
        }
        LegacyCmd::FindAll { url, selector, testid, role, name, raw, limit, timeout } => {
            cmd::find_all::run(
                &url,
                selector.as_deref(),
                testid.as_deref(),
                role.as_deref(),
                name.as_deref(),
                raw,
                limit,
                timeout,
            )
            .await
        }
    }
}

async fn print_control(base_dir: Option<&str>, action: &str) -> Result<()> {
    let paths = daemon::resolve_paths(base_dir)?;
    let resp = daemon::send_control_action(&paths.socket_path, action).await?;
    println!("{}", serde_json::to_string(&resp)?);
    Ok(())
}
