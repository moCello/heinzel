//! heinzel starts, watches, continues and stops headless agent sessions.
//!
//! A caller names each session by an opaque key. heinzel knows keys, runtimes
//! and processes, and nothing about what the caller does with a session.
//!
//! - [`store`] keeps what heinzel knows about each key on disk.
//! - [`state`] is the state of a session, which only a lock holder writes.
//! - [`config`] reads the programs and permission profiles.
//! - [`runtime`] holds one adapter per agent CLI.
//! - [`holder`] is the detached process that owns an agent run.
//! - [`session`] is what a caller does to a session.
//! - [`cli`] parses the command line.

mod cli;
mod config;
mod holder;
mod runtime;
mod session;
mod state;
mod store;

use std::process::ExitCode;

fn main() -> ExitCode {
    cli::run(std::env::args_os().skip(1).collect())
}
