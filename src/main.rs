#![forbid(unsafe_code)]

// Static musl builds use mimalloc as the global allocator: musl's default
// allocator is too slow for this workload (DESIGN 8). Only the musl target
// pulls it in, so the ordinary glibc build is unchanged.
#[cfg(target_env = "musl")]
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::ffi::OsString;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use emx::cli::{self, Tool};
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
    /// Show FILE in the most recently active emx terminal; FILE:LINE[:COL]
    /// goes to a position when no file has that whole name.
    Open {
        file: OsString,
        /// Return only when the buffer is done with (C-x # or killing it),
        /// as $EDITOR for git commit messages.
        #[arg(long)]
        wait: bool,
    },
    /// List the session's buffers, most recently shown first.
    ListBuffers {
        /// One JSON array; paths that are not UTF-8 are {"bytes_b64": ...}.
        #[arg(long)]
        json: bool,
    },
    /// Read unmodified buffers again from disk: FILE's, or every file
    /// buffer's.
    Reload { file: Option<OsString> },
    /// Move point: LINE[:COL] in the terminal's current buffer, or
    /// FILE:LINE[:COL].
    Goto {
        #[arg(value_name = "[FILE:]LINE[:COL]")]
        target: OsString,
    },
    /// Save every modified file buffer without asking; saves that would
    /// ask are reported instead.
    SaveAll,
    /// Print buffer, file and cursor events as JSON lines until
    /// interrupted.
    Events,
    /// Show how FILE changed on disk against its buffer.
    DiffExternal { file: OsString },
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
        Some(Command::Open { file, wait }) => cli::run(Tool::Open { file, wait }),
        Some(Command::ListBuffers { json }) => cli::run(Tool::ListBuffers { json }),
        Some(Command::Reload { file }) => cli::run(Tool::Reload { file }),
        Some(Command::Goto { target }) => cli::run(Tool::Goto { target }),
        Some(Command::SaveAll) => cli::run(Tool::SaveAll),
        Some(Command::Events) => cli::run(Tool::Events),
        Some(Command::DiffExternal { file }) => cli::run(Tool::DiffExternal { file }),
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
