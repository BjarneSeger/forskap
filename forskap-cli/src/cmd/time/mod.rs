//! `forskap time` — time tracking. [`tick`] and [`prompt`] are also reachable as
//! the hidden top-level `forskap tick` / `forskap prompt` that installed hooks call.

mod history;
mod hook;
mod log;
pub mod prompt;
pub mod tick;

use anyhow::Result;

use crate::cli::TimeCommand;

pub async fn run(command: TimeCommand) -> Result<()> {
    match command {
        TimeCommand::Log {
            reference,
            duration,
            mr,
            project,
            summary,
        } => log::run(&reference, duration, mr, project.project, summary).await,
        TimeCommand::Prompt => prompt::run().await,
        TimeCommand::History { window, output } => history::run(window.days, output.output).await,
        TimeCommand::Hook { shell } => hook::run(shell),
    }
}
