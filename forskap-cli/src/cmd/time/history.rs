//! `forskap time history` — the time logged over the last `days` days, newest
//! first, including entries still queued in the daemon.

use anyhow::Result;
use chrono::{DateTime, Utc};
use forskap_api::{HistorySource, VarlinkClientInterface};

use crate::cli::OutputFormat;
use crate::friendly::friendly;
use crate::{client, output, refspec, style};

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
            let sigil = refspec::wire_sigil(&e.kind);
            // A queued entry has its duration as given, a synced one seconds.
            let spent = e
                .time_spent
                .map(super::spent)
                .or_else(|| e.duration.clone());
            outln!(
                "{}  {:<8}  {:<6}  {:<6}  {}",
                style::muted(&ts),
                style::state(source(&e.source)),
                style::reference(sigil, e.iid),
                spent.unwrap_or_default(),
                e.title.as_deref().unwrap_or_default()
            )?;
        }
        Ok(())
    })
}

fn source(source: &HistorySource) -> &'static str {
    match source {
        HistorySource::gitlab => "gitlab",
        HistorySource::queued => "queued",
    }
}
