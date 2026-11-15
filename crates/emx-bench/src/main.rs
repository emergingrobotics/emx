//! Benchmark harness for `emx` (DESIGN section 8). It generates file
//! corpora on local disk, drives `emx --no-server` on a pseudo-terminal the
//! way a Kitty-protocol terminal would, and measures startup, open, keypress
//! and scroll latency, search throughput, reload latency and resident
//! memory, then appends the results and their pass/fail against the DESIGN 8
//! targets to `bench/README.md`.
//!
//! Corpora are written to local disk, never the repository: the repository
//! is on a network filesystem, where multi-gigabyte writes are slow, risk
//! dropping the mount, and would measure the network rather than the editor.
//! The corpus directory is `$EMX_BENCH_CORPUS_DIR` or the system temporary
//! directory. The editor binary is `$EMX_BIN` (the Makefile sets it to the
//! release build).

#![forbid(unsafe_code)]

mod corpus;
mod markdown;
mod measure;
mod pty;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use corpus::Size;

#[derive(Parser)]
#[command(about = "emx performance benchmarks against the DESIGN 8 targets")]
struct Args {
    /// Which corpus to benchmark.
    #[arg(long, value_name = "10MB|100MB-line|1GB|10GB")]
    size: Size,
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("emx-bench: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> Result<(), String> {
    let emx = emx_binary()?;
    let corpus_dir = corpus::directory();
    std::fs::create_dir_all(&corpus_dir)
        .map_err(|error| format!("creating {}: {error}", corpus_dir.display()))?;

    match corpus::ensure(&corpus_dir, args.size)? {
        corpus::Prepared::Ready(corpus) => {
            let report = measure::run(&emx, &corpus)?;
            markdown::append(&report)?;
            println!("{}", report.summary());
            Ok(())
        }
        corpus::Prepared::Skipped { reason } => {
            // A size that does not fit local disk is skipped, not failed: the
            // 10 GB corpus is optional, and a miss must be reported, not hidden.
            let note = markdown::skip_note(args.size, &reason);
            markdown::append_raw(&note)?;
            println!("emx-bench: skipped {}: {reason}", args.size.label());
            Ok(())
        }
    }
}

/// The editor binary to benchmark, from `$EMX_BIN`. The Makefile builds
/// `emx` and sets this so the bench measures the real release binary.
fn emx_binary() -> Result<PathBuf, String> {
    let value = std::env::var_os("EMX_BIN")
        .ok_or_else(|| "EMX_BIN is not set; run the benchmarks through `make bench`".to_string())?;
    let path = PathBuf::from(value);
    if !path.exists() {
        return Err(format!(
            "EMX_BIN points at {}, which does not exist; run `make build` first",
            path.display()
        ));
    }
    Ok(path)
}
