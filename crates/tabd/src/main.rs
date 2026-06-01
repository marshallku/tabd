mod browser;
mod cdp;
mod cli;
mod cmd;
mod daemon;
mod secrets;
mod skill;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::ffi::OsString;
use std::process::ExitCode;

#[derive(Subcommand)]
enum DaemonCmd {
    /// Run the daemon in the foreground (blocks until SIGTERM or daemon.shutdown).
    Start {
        /// Override base directory. Defaults to $TABD_BASE_DIR or $XDG_RUNTIME_DIR/tabd.
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

#[derive(Parser)]
#[command(name = "tabd", version, about = "Rust + Chromium CDP browser controller")]
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
    /// Install the Claude Code / Codex CLI skill (SKILL.md + 4 docs) onto disk.
    Skill {
        #[command(subcommand)]
        cmd: SkillCmd,
    },
    /// Catch-all for action subcommands (navigate, get-text, click, etc.).
    /// Routed through the daemon — auto-spawned if needed. See `src/cli.rs`
    /// for the dispatch table and `secret-put` for the plaintext-safe branch.
    #[command(external_subcommand)]
    Other(Vec<OsString>),
}

#[derive(Subcommand)]
enum SkillCmd {
    /// Copy the embedded SKILL.md + docs into ~/.claude/skills/tabd and/or
    /// ~/.codex/skills/tabd. Auto-detects which clients are installed.
    Install {
        /// Comma-separated subset: `claude`, `codex`, or `claude,codex`.
        /// Overrides auto-detection.
        #[arg(long)]
        target: Option<String>,

        /// Skip the Claude install even if Claude is detected.
        #[arg(long)]
        no_claude: bool,

        /// Skip the Codex install even if Codex is detected.
        #[arg(long)]
        no_codex: bool,

        /// Install into this directory instead of the client default.
        /// Useful for project-local skills (e.g. `.claude/skills/tabd`).
        #[arg(long)]
        path: Option<String>,

        /// Overwrite existing files in the destination directory.
        #[arg(long)]
        force: bool,
    },
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
        Command::Skill { cmd } => match run_skill_cmd(cmd) {
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

fn run_skill_cmd(cmd: SkillCmd) -> Result<()> {
    match cmd {
        SkillCmd::Install {
            target,
            no_claude,
            no_codex,
            path,
            force,
        } => {
            let plan =
                skill::build_plan(target.as_deref(), no_claude, no_codex, path.as_deref())?;
            skill::install(&plan, force)?;
            eprintln!(
                "Restart Claude Code or Codex CLI so the skill metadata is picked up."
            );
            Ok(())
        }
    }
}

async fn run_daemon_cmd(cmd: DaemonCmd) -> Result<()> {
    match cmd {
        DaemonCmd::Start { base_dir } => daemon::run(base_dir.as_deref()).await,
        DaemonCmd::Stop { base_dir } => print_control(base_dir.as_deref(), "daemon.shutdown").await,
        DaemonCmd::Ping { base_dir } => print_control(base_dir.as_deref(), "daemon.ping").await,
        DaemonCmd::Health { base_dir } => print_control(base_dir.as_deref(), "daemon.health").await,
    }
}

async fn print_control(base_dir: Option<&str>, action: &str) -> Result<()> {
    let paths = daemon::resolve_paths(base_dir)?;
    let resp = daemon::send_control_action(&paths.socket_path, action).await?;
    // Unwrap the bridge envelope: emit only the `data` payload (or the error
    // text on failure) so the CLI output looks like a plain JSON response,
    // not an `{id, success, data}` wrapper.
    let success = resp.get("success").and_then(serde_json::Value::as_bool).unwrap_or(false);
    if !success {
        let err = resp
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        anyhow::bail!("{err}");
    }
    let data = resp.get("data").cloned().unwrap_or(serde_json::Value::Null);
    println!("{}", serde_json::to_string(&data)?);
    Ok(())
}
