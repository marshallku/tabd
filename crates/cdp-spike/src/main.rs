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
    }
}
