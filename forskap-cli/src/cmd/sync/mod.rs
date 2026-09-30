//! `forskap sync` — the daemon's background sync.

mod jobs;
mod refresh;

use anyhow::Result;

use crate::cli::SyncCommand;

pub async fn run(command: SyncCommand) -> Result<()> {
    match command {
        SyncCommand::Refresh { scopes } => refresh::run(scopes).await,
        SyncCommand::Jobs { all, output, watch } => jobs::run(all, output.output, watch).await,
    }
}
