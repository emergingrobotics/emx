//! The `emx` program: the session server, the terminal client, the
//! in-process `--no-server` mode, and the command-line tools, over the
//! editor core in `emx-core`.

#![forbid(unsafe_code)]

pub mod cli;
pub mod client;
pub mod noserver;
pub mod paths;
pub mod server;
