//! `forskap time history` — the time logged over the last `days` days, newest
//! first, including entries still queued in the daemon.

use anyhow::Result;
use chrono::{DateTime, Utc};
use forskap_api::{IssuableKind, VarlinkClientInterface};

use crate::cli::OutputFormat;
use crate::friendly::friendly;
use crate::{client, output};

pub async fn run(days: u32, format: OutputFormat) -> Result<()> {
    let client = client::connect_default().await?;
    let reply = client
        .get_history(Some(i64::from(days)))
        .call()
        .await
        .map_err(|e| friendly("GetHistory", e))?;

    output::emit(format, &reply.events, |events| {
        for e in events {
            let ts = DateTime::<Utc>::from_timestamp(e.timestamp, 0)
                .map(|d| d.to_rfc3339())
                .unwrap_or_else(|| e.timestamp.to_string());
            let sigil = match e.kind {
                IssuableKind::merge_request => '!',
                IssuableKind::issue => '#',
            };
            outln!(
                "{ts}  {:<8}  {sigil}{:<5}  {:<6}  {}",
                e.source,
                e.iid,
                e.duration,
                e.title
            )?;
        }
        Ok(())
    })
}
