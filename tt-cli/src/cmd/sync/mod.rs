//! `tt sync` — the daemon's background sync.

mod refresh;

use anyhow::Result;

use crate::cli::SyncCommand;

pub async fn run(command: SyncCommand) -> Result<()> {
    match command {
        SyncCommand::Refresh { scopes } => refresh::run(scopes).await,
    }
}
