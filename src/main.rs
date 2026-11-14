#![forbid(unsafe_code)]

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use emx::server::{self, ServerOptions};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the session server that keeps buffers for every emx client.
    Server {
        /// Run in the background, as clients start it: a new session, no
        /// terminal, output to the log in the state directory.
        #[arg(long)]
        detach: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Server { detach }) => match server::run(ServerOptions { detach }) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("emx: {error:#}");
                ExitCode::FAILURE
            }
        },
        None => ExitCode::SUCCESS,
    }
}
