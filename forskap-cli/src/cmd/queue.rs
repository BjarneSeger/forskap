//! `forskap queue` — inspect and manage failed queued writes.
//!
//! A write that hit a network error is queued and retried by the daemon. If
//! GitLab later rejects it, or the retry window expires, the daemon moves it
//! to a dead-letter store. This lists those failures and lets the user retry
//! or dismiss them.

use anyhow::Result;
use chrono::{DateTime, Utc};
use forskap_api::{FailedTask, IssuableKind, VarlinkClientInterface};

use crate::cli::{OutputFormat, QueueCommand, WatchArgs};
use crate::friendly::friendly;
use crate::{client, output, style, watch};

pub async fn run(command: QueueCommand) -> Result<()> {
    match command {
        QueueCommand::List { output, watch } => list(output.output, watch).await,
        QueueCommand::Retry { id } => {
            let client = client::connect_default().await?;
            client
                .retry_failure(id)
                .call()
                .await
                .map_err(|e| friendly("RetryFailure", e))?;
            outln!("re-enqueued failed write {id}")?;
            Ok(())
        }
        QueueCommand::Dismiss { id } => {
            let client = client::connect_default().await?;
            client
                .dismiss_failure(id)
                .call()
                .await
                .map_err(|e| friendly("DismissFailure", e))?;
            outln!("dismissed failed write {id}")?;
            Ok(())
        }
        QueueCommand::Clear => {
            let client = client::connect_default().await?;
            client
                .clear_failures()
                .call()
                .await
                .map_err(|e| friendly("ClearFailures", e))?;
            outln!("cleared all failed writes")?;
            Ok(())
        }
    }
}

async fn list(format: OutputFormat, watch: WatchArgs) -> Result<()> {
    match watch::interval(watch, format)? {
        Some(every) => watch::run(every, || async { Ok(render(&fetch().await?)) }).await,
        None => output::emit(format, &fetch().await?, |failures| {
            out!("{}", render(failures))
        }),
    }
}

// Connects per call: a watch has to find a restarted daemon again.
async fn fetch() -> Result<Vec<FailedTask>> {
    let client = client::connect_default().await?;
    let reply = client
        .get_failures()
        .call()
        .await
        .map_err(|e| friendly("GetFailures", e))?;
    Ok(reply.failures)
}

fn render(failures: &[FailedTask]) -> String {
    if failures.is_empty() {
        return "no failed writes\n".to_string();
    }
    let mut out = String::new();
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
        out.push_str(&format!(
            "[{}] {} {}{}  —  {}  ({})\n",
            f.id,
            f.op,
            style::reference(sigil, f.iid),
            detail,
            style::error(&f.error),
            when
        ));
    }
    out.push_str(
        "\nretry with `forskap queue retry <id>`, drop with `forskap queue dismiss <id>`, \
         or `forskap queue clear`\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_lists_the_failures_and_what_to_do_with_them() {
        let failure = FailedTask {
            id: 3,
            op: "post_time".to_string(),
            kind: IssuableKind::merge_request,
            project_id: 7,
            iid: 42,
            detail: "1h".to_string(),
            error: "403 Forbidden".to_string(),
            queued_at: 1_799_990_000,
            failed_at: 1_800_000_000,
        };
        let text = render(std::slice::from_ref(&failure));
        assert!(
            text.starts_with(
                "[3] post_time !42 (1h)  —  403 Forbidden  (2027-01-15T08:00:00+00:00)\n\nretry"
            ),
            "{text}"
        );
        assert_eq!(render(&[]), "no failed writes\n");

        style::force(true);
        let text = render(&[failure]);
        assert!(
            text.starts_with(
                "[3] post_time \x1b[36m!42\x1b[0m (1h)  —  \x1b[31m403 Forbidden\x1b[0m  (2027"
            ),
            "{text}"
        );
    }
}
