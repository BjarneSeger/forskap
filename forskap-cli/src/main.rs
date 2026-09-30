//! `forskap` — client for the [`forskapd`] varlink daemon.
//!
//! See the modules under [`cmd`], which mirror the command tree. The binary is
//! deliberately thin: all GitLab access goes through the daemon over a unix
//! socket, so `forskap` only handles argument parsing, local state (last-prompt
//! timestamp, last-used issue) and the interactive UI.
//!
//! [`forskapd`]: ../../forskapd/README.md

// A closed stdout must not panic: print with `out!` / `outln!`.
#![warn(clippy::print_stdout)]

use std::ffi::OsStr;
use std::io::IsTerminal;
use std::path::Path;
use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;

// First, so the modules below see `out!` / `outln!`.
#[macro_use]
mod output;

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
mod columns;
mod complete;
mod config;
mod friendly;
mod item;
mod migrate;
mod pick;
mod refspec;
mod state;
mod style;
mod watch;

use cli::{Cli, Command};
use refspec::RefKind;

/// Single-thread tokio flavour: each invocation does at most a few varlink
/// round-trips plus stdin/stdout work, so a multi-thread runtime would just add
/// startup overhead to the hot `forskap tick` path (fires on every shell prompt).
///
/// Built by hand rather than by `#[tokio::main]` because a shell completion
/// request is answered first: its completers are synchronous and bring their
/// own runtime, which can't be started from inside this one.
fn main() -> ExitCode {
    complete::run();
    let outcome = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(anyhow::Error::from)
        .and_then(|runtime| runtime.block_on(run()));
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        // `forskap issue list | head`: the reader has what it wanted.
        Err(e) if e.is::<output::StdoutClosed>() => ExitCode::SUCCESS,
        // The picker already showed the dismissal; exit like a Ctrl-C.
        Err(e) if e.is::<pick::Cancelled>() => ExitCode::from(130),
        Err(e) => {
            eprintln!("Error: {e:?}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    migrate::run();
    let Cli { color, command } = Cli::parse();
    style::init(color);
    if !matches!(command, Command::Tick { .. } | Command::Prompt) {
        note_old_name();
    }
    match command {
        Command::Issue { command } => cmd::item::run(RefKind::Issue, command).await,
        Command::Mr { command } => cmd::item::run(RefKind::Mr, command).await,
        Command::Epic { command } => cmd::epic::run(command).await,
        Command::Search {
            query,
            kinds,
            limit,
            output,
        } => cmd::search::run(query, kinds, limit, output.output).await,
        Command::Activity { window, output } => {
            cmd::activity::run(window.days, output.output).await
        }
        Command::Time { command } => cmd::time::run(command).await,
        Command::Auth { command } => cmd::auth::run(command).await,
        Command::Sync { command } => cmd::sync::run(command).await,
        Command::Queue { command } => cmd::queue::run(command).await,
        Command::Config { command } => cmd::config::run(command),
        #[cfg(target_os = "linux")]
        Command::Integration { command } => cmd::integration::run(command).await,
        Command::Tick { mode } => cmd::time::tick::run(mode).await,
        Command::Prompt => cmd::time::prompt::run().await,
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
