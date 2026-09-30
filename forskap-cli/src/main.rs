//! `forskap` — client for the [`forskapd`] varlink daemon.
//!
//! See the modules under [`cmd`], which mirror the command tree. The binary is
//! deliberately thin: all GitLab access goes through the daemon over a unix
//! socket, so `forskap` only handles argument parsing, local state (last-prompt
//! timestamp, last-used issue) and the interactive UI.
//!
//! [`forskapd`]: ../../forskapd/README.md

use std::ffi::OsStr;
use std::io::IsTerminal;
use std::path::Path;

use anyhow::Result;
use clap::Parser;

/// Clap-derived command-line surface.
///
/// GitLab terminology note: every issue/MR has both a global `id` and a
/// per-project `iid` (the `#42` / `!7` shown in the UI). The varlink API needs
/// `project_id`, `iid`, and the kind to address one; users almost always know
/// the `iid` but rarely the project, so `forskap issue` / `forskap mr` take the `iid`
/// positionally and resolve the project lazily (see [`cmd::project`]).
mod cli;
#[cfg(test)]
mod cli_tests;
mod client;
mod cmd;
mod config;
mod friendly;
mod migrate;
mod output;
mod refspec;
mod state;

use cli::{Cli, Command};
use refspec::RefKind;

/// Single-thread tokio flavour: each invocation does at most a few varlink
/// round-trips plus stdin/stdout work, so a multi-thread runtime would just add
/// startup overhead to the hot `forskap tick` path (fires on every shell prompt).
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    die_on_closed_pipe();
    migrate::run();
    let command = Cli::parse().command;
    if !matches!(command, Command::Tick { .. } | Command::Prompt) {
        note_old_name();
    }
    match command {
        Command::Issue { command } => cmd::item::run(RefKind::Issue, command).await,
        Command::Mr { command } => cmd::item::run(RefKind::Mr, command).await,
        Command::Search {
            query,
            kinds,
            limit,
            output,
        } => cmd::search::run(query, kinds, limit, output.output).await,
        Command::Time { command } => cmd::time::run(command).await,
        Command::Auth { command } => cmd::auth::run(command).await,
        Command::Sync { command } => cmd::sync::run(command).await,
        Command::Queue { command } => cmd::queue::run(command).await,
        Command::Config { command } => {
            cmd::config::run(command);
            Ok(())
        }
        #[cfg(target_os = "linux")]
        Command::Integration { command } => cmd::integration::run(command).await,
        Command::Tick { mode } => cmd::time::tick::run(mode).await,
        Command::Prompt => cmd::time::prompt::run().await,
    }
}

/// Rust ignores SIGPIPE, so `forskap issue list | head` would panic in
/// `println!` once `head` is done. Take the signal's default again: exit
/// quietly, like any other filter.
fn die_on_closed_pipe() {
    // SAFETY: called first in `main`, before another thread exists; setting
    // a signal's default disposition has no other precondition.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

/// Tell interactive users of the packaged `tt` symlink about the new name.
fn note_old_name() {
    let as_tt = std::env::args_os()
        .next()
        .is_some_and(|arg0| Path::new(&arg0).file_name() == Some(OsStr::new("tt")));
    if as_tt && std::io::stderr().is_terminal() {
        eprintln!("note: `tt` is now called `forskap`; the old name will be removed");
    }
}
