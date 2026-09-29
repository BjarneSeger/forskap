//! `tt auth status` — show who the daemon is currently authenticated as.

use anyhow::Result;
use gitlab_trackr_api::VarlinkClientInterface;

use crate::cli::OutputFormat;
use crate::friendly::friendly;
use crate::{client, output};

pub async fn run(format: OutputFormat) -> Result<()> {
    let client = client::connect_default().await?;
    let me = client
        .who_am_i()
        .call()
        .await
        .map_err(|e| friendly("WhoAmI", e))?;

    output::emit(
        format,
        &serde_json::json!({ "host": me.host, "user_id": me.user_id }),
        |_| println!("Logged in to {} as user #{}.", me.host, me.user_id),
    )
}
