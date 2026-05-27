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
    /// Extract texts from all matching elements as a JSON array.
    /// Unlike `get-text`, exactly one TARGET (--selector/--testid/--role) is
    /// required — no default chain. Output is a JSON string array on stdout.
    QueryAll {
        url: String,
        /// Explicit CSS selector. Mutually exclusive with --testid / --role.
        #[arg(long, group = "qa_target")]
        selector: Option<String>,
        /// data-testid value shortcut. Mutually exclusive with --selector / --role.
        #[arg(long, group = "qa_target")]
        testid: Option<String>,
        /// ARIA role for Accessibility.queryAXTree. Mutually exclusive with --selector / --testid.
        #[arg(long, group = "qa_target")]
        role: Option<String>,
        /// Exact accessible name match. Requires --role.
        #[arg(long, requires = "role")]
        name: Option<String>,
        /// Return raw textContent (no innerText, no collapse, no trim).
        #[arg(long)]
        raw: bool,
        /// Cap on number of returned texts. Skipped (ignored/virtual) nodes do not count.
        #[arg(long, default_value_t = 100)]
        limit: u32,
        #[arg(long, default_value_t = 30_000)]
        timeout: u64,
    },
    /// High-level text extraction with TS `dom.getText` semantics:
    /// default selector chain (`main, article, body` then `body` fallback),
    /// `innerText` with blank-line collapse and trim, `--raw` for unprocessed
    /// `textContent`. Element identification supports three mutually exclusive
    /// modes: `--selector <CSS>`, `--testid <ID>` shortcut for
    /// `[data-testid=...]`, or `--role <ROLE>` (with optional `--name <NAME>`)
    /// via CDP `Accessibility.queryAXTree` — first visible (non-ignored) match.
    GetText {
        url: String,
        /// Explicit CSS selector. Mutually exclusive with --testid / --role.
        #[arg(long, group = "gt_target")]
        selector: Option<String>,
        /// data-testid value shortcut. Mutually exclusive with --selector / --role.
        #[arg(long, group = "gt_target")]
        testid: Option<String>,
        /// ARIA role for Accessibility.queryAXTree. Mutually exclusive with --selector / --testid.
        #[arg(long, group = "gt_target")]
        role: Option<String>,
        /// Exact accessible name match. Requires --role.
        #[arg(long, requires = "role")]
        name: Option<String>,
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
            role,
            name,
            raw,
            timeout,
        } => {
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
        Command::QueryAll {
            url,
            selector,
            testid,
            role,
            name,
            raw,
            limit,
            timeout,
        } => {
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
    }
}
