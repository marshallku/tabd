mod browser;
mod cdp;
mod cmd;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "cdp-spike", about = "Rust + Chromium CDP spike for ai-browser")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
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
    /// High-level text extraction with TS `dom.getText` semantics:
    /// default selector chain (`main, article, body` then `body` fallback),
    /// `innerText` with blank-line collapse and trim, `--raw` for unprocessed
    /// `textContent`, and a `--testid` shortcut for `[data-testid=...]`.
    GetText {
        url: String,
        /// Explicit CSS selector. Mutually exclusive with --testid.
        #[arg(long, group = "gt_target")]
        selector: Option<String>,
        /// data-testid value shortcut. Mutually exclusive with --selector.
        #[arg(long, group = "gt_target")]
        testid: Option<String>,
        /// Return raw textContent (no innerText, no collapse, no trim).
        #[arg(long)]
        raw: bool,
        #[arg(long, default_value_t = 30_000)]
        timeout: u64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Navigate { url, timeout } => cmd::navigate::run(&url, timeout).await,
        Command::Eval {
            url,
            expr,
            json,
            timeout,
        } => cmd::eval::run(&url, &expr, json, timeout).await,
        Command::FetchText {
            url,
            selector,
            timeout,
        } => cmd::fetch_text::run(&url, &selector, timeout).await,
        Command::GetText {
            url,
            selector,
            testid,
            raw,
            timeout,
        } => {
            cmd::get_text::run(
                &url,
                selector.as_deref(),
                testid.as_deref(),
                raw,
                timeout,
            )
            .await
        }
    }
}
