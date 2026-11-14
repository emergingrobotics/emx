#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use emx::client::{self, ClientOptions};
use emx::noserver;
use emx::server::{self, ServerOptions};

#[derive(Parser)]
#[command(version, about, args_conflicts_with_subcommands = true)]
struct Cli {
    /// Run the editor in this process, without the session server.
    #[arg(long)]
    no_server: bool,
    /// Return only when every FILE is done with (C-x # or killing its
    /// buffer), as $EDITOR for git commit messages.
    #[arg(long, requires = "files", conflicts_with = "no_server")]
    wait: bool,
    /// Files to visit; FILE:LINE[:COL] goes to a position when no file has
    /// that whole name.
    files: Vec<OsString>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the session server that keeps buffers for every emx client.
    Server {
        /// Start the server in the background, as clients do, and return
        /// once it accepts connections.
        #[arg(long)]
        detach: bool,
        /// Lead a new session, away from any terminal (how --detach runs
        /// the server).
        #[arg(long, hide = true, conflicts_with = "detach")]
        new_session: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let outcome = match cli.command {
        Some(Command::Server {
            detach,
            new_session,
        }) => server::run(ServerOptions {
            detach,
            new_session,
        })
        .map(|()| ExitCode::SUCCESS),
        None if cli.no_server => noserver::run(&cli.files),
        None => client::run(ClientOptions {
            files: cli.files,
            wait: cli.wait,
        }),
    };
    match outcome {
        Ok(code) => code,
        Err(error) => {
            eprintln!("emx: {error:#}");
            ExitCode::FAILURE
        }
    }
}
