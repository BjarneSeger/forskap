//! `tt integration` — desktop integrations.

pub mod search_provider;

use anyhow::Result;

use crate::cli::IntegrationCommand;

pub async fn run(command: IntegrationCommand) -> Result<()> {
    match command {
        IntegrationCommand::SearchProvider { command } => search_provider::run(command).await,
    }
}
