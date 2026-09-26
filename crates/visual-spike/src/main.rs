//! `visual-spike` — V0 verification spike for tabd visual mode.
//!
//! Answers the open questions in `docs/visual-mode-plan.md` §9 against a real
//! browser and prints one JSON object per probe (JSONL) on stdout. Nothing
//! here ships: it is a separate crate precisely so the tabd binary, install.sh
//! and CI stay untouched.

mod pipe;
mod probes;
mod scratch;
mod sys;

use probes::{Ctx, PROBE_IDS, Verdict, run_probe};
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
usage: visual-spike run [--all | <probe-id>...] [options]

options:
  --all              run every probe (q6 always runs last)
  --interactive      also run probes that need a human click (q8)
  --keep             keep scratch profile directories for inspection
  --login-url <url>  a site you are already logged into, for q6's functional check
  --out-dir <dir>    where screenshots go (default: ./visual-spike-out)

probes:
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) != Some("run") {
        eprint!("{USAGE}");
        for id in PROBE_IDS {
            eprintln!("  {id}");
        }
        return ExitCode::from(2);
    }

    let mut selected: Vec<&'static str> = Vec::new();
    let mut ctx = Ctx {
        exe: pipe::brave_executable(),
        keep: false,
        interactive: false,
        login_url: None,
        out_dir: PathBuf::from("visual-spike-out"),
    };
    let mut rest = args[1..].iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--all" => selected.extend_from_slice(PROBE_IDS),
            "--interactive" => ctx.interactive = true,
            "--keep" => ctx.keep = true,
            "--login-url" => ctx.login_url = rest.next().cloned(),
            "--out-dir" => {
                if let Some(dir) = rest.next() {
                    ctx.out_dir = PathBuf::from(dir);
                }
            }
            other => match PROBE_IDS.iter().find(|id| **id == other) {
                Some(id) => selected.push(id),
                None => {
                    eprintln!("unknown probe {other:?}");
                    return ExitCode::from(2);
                }
            },
        }
    }
    if selected.is_empty() {
        eprintln!("nothing to run; pass --all or a probe id");
        return ExitCode::from(2);
    }
    // Deduplicate while keeping the canonical order, so q6's "no Brave may be
    // running" guard can never be scheduled before another probe's browser.
    selected = PROBE_IDS
        .iter()
        .copied()
        .filter(|id| selected.contains(id))
        .collect();

    let mut failures = 0;
    for id in selected {
        eprintln!("[visual-spike] running {id}");
        let outcome = run_probe(id, &ctx);
        if outcome.verdict == Verdict::Fail {
            failures += 1;
        }
        println!("{}", outcome.to_json());
        eprintln!("[visual-spike] {id}: {}", outcome.answer);
    }

    if failures > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}
