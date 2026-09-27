mod browser;
mod cdp;
mod cli;
mod cmd;
mod daemon;
mod platform;
mod profile;
mod secrets;
mod service;
mod skill;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::ffi::OsString;
use std::process::ExitCode;

#[derive(Subcommand)]
enum DaemonCmd {
    /// Run the daemon in the foreground (blocks until SIGTERM or daemon.shutdown).
    Start {
        /// Override base directory. Defaults to $TABD_BASE_DIR or $XDG_RUNTIME_DIR/tabd
        /// (visual mode: $XDG_STATE_HOME/tabd/visual, macOS ~/Library/Application Support/tabd/visual).
        #[arg(long)]
        base_dir: Option<String>,

        /// Drive the human's everyday browser over the debugging pipe instead
        /// of a throwaway headless profile. Serves owner lifecycle actions
        /// only (`browser.ensure`, `browser.status`) — see
        /// docs/visual-mode-plan.md.
        #[arg(long)]
        visual: bool,
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
#[command(
    name = "tabd",
    version,
    about = "Rust + Chromium CDP browser controller"
)]
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
    /// Open the human's everyday browser, and the given urls in it.
    ///
    /// This is the owner entry point the default-browser registration calls
    /// (Linux `.desktop` `Exec=tabd browser %U`, macOS wrapper app). Starts
    /// the visual daemon if it is not running. With no url it just makes sure
    /// the browser is up.
    Browser {
        /// Urls to open. `http`, `https` and `file` only.
        urls: Vec<String>,

        /// Override the visual daemon's base directory.
        #[arg(long)]
        base_dir: Option<String>,

        /// Print the raw daemon response instead of a human summary.
        #[arg(long)]
        json: bool,
    },
    /// Move your everyday browser profile under tabd's control (owner only).
    Profile {
        #[command(subcommand)]
        cmd: ProfileCmd,
    },
    /// Register tabd with the OS so it can be launched as a browser.
    Service {
        #[command(subcommand)]
        cmd: ServiceCmd,
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
enum ServiceCmd {
    /// Install the desktop entry / wrapper app and the background service.
    ///
    /// Does NOT make tabd your default browser, and does not start the
    /// service, unless you ask for those explicitly.
    Install {
        /// Make tabd the default web browser (Linux only; on macOS this is a
        /// user gesture the command prints instructions for).
        #[arg(long)]
        set_default: bool,
        /// Enable and start the background service now.
        #[arg(long)]
        enable_service: bool,
        /// Install under this directory instead of $HOME. For testing.
        #[arg(long)]
        prefix: Option<String>,
    },
    /// Remove what `install` added. Refuses while tabd is the default browser.
    Uninstall {
        #[arg(long)]
        prefix: Option<String>,
    },
    /// Show what is installed.
    Status {
        #[arg(long)]
        prefix: Option<String>,
        /// Which visual daemon's state to read (the url-delivery log).
        /// Defaults to $TABD_BASE_DIR or the platform default, matching
        /// `tabd browser --base-dir`.
        #[arg(long)]
        base_dir: Option<String>,
    },
}

#[derive(Subcommand)]
enum ProfileCmd {
    /// Copy your real browser profile to a staging directory, for you to
    /// check before anything is published. Reads the original and nothing
    /// else — it is never moved, modified or deleted.
    ///
    /// Requires the browser to be fully quit: a live profile copies as an
    /// inconsistent SQLite snapshot.
    Import {
        /// The profile to copy. Defaults to the real profile of the browser
        /// tabd would launch.
        #[arg(long)]
        from: Option<String>,
        /// Where visual mode will look for it. Defaults to the visual profile
        /// directory.
        #[arg(long)]
        to: Option<String>,
    },
    /// Publish the staged copy with an atomic rename, once you have checked
    /// it. Refuses if the destination is not empty.
    Cutover {
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: Option<String>,
    },
    /// Delete the staging copy. It holds real cookies and passwords, so this
    /// exists rather than asking you to `rm -rf` the right path by hand.
    Discard {
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: Option<String>,
    },
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

fn main() -> ExitCode {
    quiet_broken_pipe();
    let cli = Cli::parse();

    // Runtime sizing by command. Every path is IO-bound (the heavy lifting is
    // in Chromium, reached over a socket), so worker threads buy little:
    //   - `daemon start` is long-lived and juggles a few concurrent tasks (CDP
    //     reader/writer, supervisor, per-connection handlers), but all driver
    //     actions serialize through one mutex — 2 workers is plenty and leaves
    //     headroom for an inline pbkdf2 in the secrets path.
    //   - Every other invocation is a single short-lived request/response over
    //     the daemon socket; a current-thread runtime avoids spawning 1 worker
    //     per CPU (was 12 threads on a 12-core box) for a ~2ms round-trip.
    // Borrow in the match so `cli.command` is only read here, not moved — it is
    // consumed below in `block_on`. (The `{ .. }` pattern binds nothing, so a
    // by-value match wouldn't move either, but `&` makes that explicit.)
    let is_daemon_start = matches!(
        &cli.command,
        Command::Daemon {
            cmd: DaemonCmd::Start { .. }
        }
    );

    let runtime = if is_daemon_start {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
    } else {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
    };
    let runtime = match runtime {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("error: failed to build tokio runtime: {err}");
            return ExitCode::from(1);
        }
    };

    let code: i32 = runtime.block_on(async {
        match cli.command {
            Command::Daemon { cmd } => match run_daemon_cmd(cmd).await {
                Ok(()) => 0,
                Err(err) => {
                    eprintln!("error: {err:#}");
                    1
                }
            },
            Command::Browser {
                urls,
                base_dir,
                json,
            } => match cli::run_browser(urls, base_dir.as_deref(), json).await {
                Ok(code) => code,
                Err(err) => {
                    eprintln!("error: {err:#}");
                    1
                }
            },
            Command::Profile { cmd } => match run_profile_cmd(cmd) {
                Ok(()) => 0,
                Err(err) => {
                    eprintln!("error: {err:#}");
                    1
                }
            },
            Command::Service { cmd } => match run_service_cmd(cmd) {
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
        }
    });
    ExitCode::from(code.clamp(0, 255) as u8)
}

/// Exit quietly when stdout goes away, instead of aborting.
///
/// Rust ignores `SIGPIPE`, so a `println!` into a closed pipe returns an error
/// that the macro turns into a panic — and this crate builds with
/// `panic = "abort"`, so `tabd … | head -1` died with "Abort trap: 6".
///
/// The obvious fix, restoring `SIGPIPE` to `SIG_DFL`, is wrong here: it
/// applies to *every* write in the process, including the daemon's writes to
/// Chromium's debugging pipe. A browser exiting mid-write would then kill the
/// daemon outright rather than producing the `EPIPE` the transport is written
/// to handle. So the signal disposition is left alone and only the specific
/// panic is intercepted.
fn quiet_broken_pipe() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = info.to_string();
        // Matched on the print macros' own wording, not on "Broken pipe"
        // anywhere in the panic. Any panic carrying an `io::Error` with
        // `ErrorKind::BrokenPipe` would otherwise become a *successful* exit —
        // and both service definitions this crate generates read exit 0 as
        // "do not restart" (`Restart=on-failure`, `KeepAlive
        // { SuccessfulExit: false }`), so a daemon crash would turn into a
        // permanently dead service.
        let from_stdout = message.contains("failed printing to stdout")
            || message.contains("failed writing to stdout");
        // Both halves are needed. The macro wording alone covers *every*
        // stdout write failure — a full disk or a would-block on a nonblocking
        // pipe would exit 0 with truncated output and no diagnostic — and the
        // broken-pipe text alone catches unrelated panics carrying an EPIPE.
        let broken_pipe = message.contains("Broken pipe") || message.contains("os error 32");
        if from_stdout && broken_pipe {
            // The reader is gone; there is nobody to report anything to.
            std::process::exit(0);
        }
        previous(info);
    }));
}

fn run_service_cmd(cmd: ServiceCmd) -> Result<()> {
    fn resolve(dir: Option<String>) -> Result<service::Prefix> {
        match dir {
            Some(dir) => Ok(service::Prefix::at(&dir)),
            None => service::Prefix::home(),
        }
    }
    match cmd {
        ServiceCmd::Install {
            set_default,
            enable_service,
            prefix,
        } => service::install(
            &resolve(prefix)?,
            &service::Options {
                set_default,
                enable_service,
            },
        ),
        ServiceCmd::Uninstall { prefix } => service::uninstall(&resolve(prefix)?),
        ServiceCmd::Status { prefix, base_dir } => {
            service::status(&resolve(prefix)?, base_dir.as_deref())
        }
    }
}

fn run_profile_cmd(cmd: ProfileCmd) -> Result<()> {
    let (from, to) = match &cmd {
        ProfileCmd::Import { from, to }
        | ProfileCmd::Cutover { from, to }
        | ProfileCmd::Discard { from, to } => (from.clone(), to.clone()),
    };
    let plan = profile::plan(from.as_deref(), to.as_deref())?;
    match cmd {
        ProfileCmd::Import { .. } => profile::import(&plan),
        ProfileCmd::Cutover { .. } => profile::cutover(&plan),
        ProfileCmd::Discard { .. } => profile::discard(&plan),
    }
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
            let plan = skill::build_plan(target.as_deref(), no_claude, no_codex, path.as_deref())?;
            skill::install(&plan, force)?;
            eprintln!("Restart Claude Code or Codex CLI so the skill metadata is picked up.");
            Ok(())
        }
    }
}

async fn run_daemon_cmd(cmd: DaemonCmd) -> Result<()> {
    match cmd {
        DaemonCmd::Start { base_dir, visual } => {
            let mode = if visual {
                daemon::DaemonMode::Visual
            } else {
                daemon::DaemonMode::Headless
            };
            daemon::run_mode(base_dir.as_deref(), mode).await
        }
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
    let success = resp
        .get("success")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
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
