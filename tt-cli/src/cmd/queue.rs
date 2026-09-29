//! `tt queue` — inspect and manage failed queued writes.
//!
//! A write that hit a network error is queued and retried by the daemon. If
//! GitLab later rejects it, or the retry window expires, the daemon moves it
//! to a dead-letter store. This lists those failures and lets the user retry
//! or dismiss them.

use anyhow::Result;
use chrono::{DateTime, Utc};
use gitlab_trackr_api::{IssuableKind, VarlinkClient, VarlinkClientInterface};

use crate::cli::{OutputFormat, QueueCommand};
use crate::friendly::friendly;
use crate::{client, output};

pub async fn run(command: QueueCommand) -> Result<()> {
    let client = client::connect_default().await?;

    match command {
        QueueCommand::List { output } => list(&client, output.output).await,
        QueueCommand::Retry { id } => {
            client
                .retry_failure(id)
                .call()
                .await
                .map_err(|e| friendly("RetryFailure", e))?;
            println!("re-enqueued failed write {id}");
            Ok(())
        }
        QueueCommand::Dismiss { id } => {
            client
                .dismiss_failure(id)
                .call()
                .await
                .map_err(|e| friendly("DismissFailure", e))?;
            println!("dismissed failed write {id}");
            Ok(())
        }
        QueueCommand::Clear => {
            client
                .clear_failures()
                .call()
                .await
                .map_err(|e| friendly("ClearFailures", e))?;
            println!("cleared all failed writes");
            Ok(())
        }
    }
}

async fn list(client: &VarlinkClient, format: OutputFormat) -> Result<()> {
    let reply = client
        .get_failures()
        .call()
        .await
        .map_err(|e| friendly("GetFailures", e))?;

    output::emit(format, &reply.failures, |failures| {
        if failures.is_empty() {
            println!("no failed writes");
            return;
        }
        for f in failures {
            let when = DateTime::<Utc>::from_timestamp(f.failed_at, 0)
                .map(|d| d.to_rfc3339())
                .unwrap_or_else(|| f.failed_at.to_string());
            let detail = if f.detail.is_empty() {
                String::new()
            } else {
                format!(" ({})", f.detail)
            };
            let sigil = match f.kind {
                IssuableKind::merge_request => '!',
                IssuableKind::issue => '#',
            };
            println!(
                "[{}] {} {sigil}{}{}  —  {}  ({})",
                f.id, f.op, f.iid, detail, f.error, when
            );
        }
        println!(
            "\nretry with `tt queue retry <id>`, drop with `tt queue dismiss <id>`, \
             or `tt queue clear`"
        );
    })
}
