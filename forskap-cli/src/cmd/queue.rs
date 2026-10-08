//! `forskap queue` — the writes the daemon queued, and the ones it gave up.
//!
//! A write that hit a network error is queued and retried by the daemon. If
//! GitLab later rejects it, or the retry window expires, the daemon moves it
//! to a dead-letter store. This lists the writes still waiting, each with
//! what it waits for, above those failures, and lets the user retry or
//! dismiss the latter.

use anyhow::Result;
use chrono::{DateTime, Utc};
use forskap_api::{FailedTask, QueuedWrite, VarlinkClientInterface};
use serde::Serialize;

use crate::cli::{OutputFormat, QueueCommand, WatchArgs};
use crate::cmd::sync::jobs::{failure, pause, span};
use crate::friendly::{self, friendly};
use crate::{client, output, refspec, style, watch};

/// The line a write command adds when the daemon queued its write instead
/// of sending it.
pub const NOT_SENT: &str =
    "not sent yet: the daemon sends it once it reaches GitLab (`forskap queue list`)";

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
    let text = |listed: &Listed| render(&listed.listing, listed.no_session.as_deref(), now());
    match watch::interval(watch, format)? {
        Some(every) => watch::run(every, || async { Ok(text(&fetch().await?)) }).await,
        None => {
            let listed = fetch().await?;
            output::emit(format, &listed.listing, |_| out!("{}", text(&listed)))
        }
    }
}

fn now() -> i64 {
    Utc::now().timestamp()
}

/// What `forskap queue list` shows, and its `--output json`: the writes
/// still waiting to be sent and the ones the daemon gave up.
#[derive(Serialize)]
struct Listing {
    /// Oldest first, the order they are sent in. Absent from a daemon too
    /// old to say.
    #[serde(skip_serializing_if = "Option::is_none")]
    queued: Option<Vec<QueuedWrite>>,
    /// Unix seconds a GitLab rate limit holds them all until.
    #[serde(skip_serializing_if = "Option::is_none")]
    paused_until: Option<i64>,
    failures: Vec<FailedTask>,
}

struct Listed {
    listing: Listing,
    /// Why nothing is sent: the daemon has no session. Asked only while
    /// something waits.
    no_session: Option<String>,
}

// Connects per call: a watch has to find a restarted daemon again.
async fn fetch() -> Result<Listed> {
    let client = client::connect_default().await?;
    let (queued, paused_until) = match client.get_queue().call().await {
        Ok(reply) => (Some(reply.writes), reply.paused_until),
        // A daemon before 1.3 lists only what failed.
        Err(e) if friendly::is_method_not_found(&e) => (None, None),
        Err(e) => return Err(friendly("GetQueue", e)),
    };
    let failures = client
        .get_failures()
        .call()
        .await
        .map_err(|e| friendly("GetFailures", e))?
        .failures;
    let waiting = queued.as_ref().is_some_and(|q| !q.is_empty());
    // Whatever fails here is not the listing failing.
    let status = match waiting {
        true => client.get_status().call().await.ok(),
        false => None,
    };
    let no_session = status
        .filter(|s| !s.connected)
        .map(|s| friendly::message_for(s.reason, s.detail.as_deref()));
    let listing = Listing {
        queued,
        paused_until,
        failures,
    };
    Ok(Listed {
        listing,
        no_session,
    })
}

/// The waiting writes above the failed ones. With nothing waiting it is
/// the failures alone, as a daemon too old to list the queue gets them.
fn render(listing: &Listing, no_session: Option<&str>, now: i64) -> String {
    let queued = listing.queued.as_deref().unwrap_or_default();
    if queued.is_empty() {
        if listing.failures.is_empty() {
            let nothing = match listing.queued {
                Some(_) => "nothing queued, no failed writes",
                None => "no failed writes",
            };
            return format!("{}\n", style::muted(nothing));
        }
        return failed(&listing.failures);
    }
    let mut out = String::new();
    let paused = pause(listing.paused_until, now);
    // What holds them all, said once.
    let held = match (no_session, &paused) {
        (Some(why), _) => Some(format!("no GitLab session: {why}")),
        (None, Some(pause)) => Some(pause.clone()),
        (None, None) => None,
    };
    if let Some(held) = held {
        out.push_str(&format!("{}\n\n", style::warning(&held)));
    }
    out.push_str(&format!("{}\n", style::heading("queued")));
    for w in queued {
        let state = waits(w, no_session.is_some(), paused.is_some(), now);
        let state = match w.last_error.is_some() && !w.running {
            true => style::error(&state),
            false => style::muted(&state),
        };
        let since = format!("(queued {} ago)", span(now - w.queued_at));
        out.push_str(&format!(
            "{}  —  {}  {}\n",
            subject(w.id, &w.op, &w.kind, w.iid, &w.detail),
            state,
            style::muted(&since)
        ));
    }
    if !listing.failures.is_empty() {
        out.push_str(&format!("\n{}\n", style::heading("failed")));
        out.push_str(&failed(&listing.failures));
    }
    out
}

/// What a queued write waits for, the first of these that holds: its own
/// attempt, the write ahead of it, a session, the rate limit's pause, its
/// backoff.
fn waits(w: &QueuedWrite, no_session: bool, paused: bool, now: i64) -> String {
    let tried = || failure(w.attempts, w.last_error.as_deref());
    if w.running {
        return "being sent".to_string();
    }
    if w.blocked {
        return "after an earlier write to it".to_string();
    }
    if no_session {
        return "waits for a GitLab session".to_string();
    }
    if paused {
        return "after the pause".to_string();
    }
    match w.next_attempt_at.filter(|&at| at > now) {
        Some(at) => format!("{}; next try in {}", tried(), span(at - now)),
        None if w.last_error.is_some() => format!("{}; next try now", tried()),
        None => "sent next".to_string(),
    }
}

/// `[3] PostTime !42 (1h)`: the id, the write and what it is on.
fn subject(id: i64, op: &str, kind: &forskap_api::IssuableKind, iid: i64, detail: &str) -> String {
    let detail = match detail {
        "" => String::new(),
        detail => format!(" ({detail})"),
    };
    format!(
        "{} {op} {}{detail}",
        style::muted(&format!("[{id}]")),
        style::reference(refspec::wire_sigil(kind), iid)
    )
}

/// The failed writes and what to do with them.
fn failed(failures: &[FailedTask]) -> String {
    let mut out = String::new();
    for f in failures {
        let when = DateTime::<Utc>::from_timestamp(f.failed_at, 0)
            .map(|d| d.to_rfc3339())
            .unwrap_or_else(|| f.failed_at.to_string());
        out.push_str(&format!(
            "{}  —  {}  {}\n",
            subject(f.id, &f.op, &f.kind, f.iid, &f.detail),
            style::error(&f.error),
            style::muted(&format!("({when})"))
        ));
    }
    let hint = "retry with `forskap queue retry <id>`, drop with `forskap queue dismiss <id>`, \
                or `forskap queue clear`";
    out.push_str(&format!("\n{}\n", style::muted(hint)));
    out
}

#[cfg(test)]
mod tests {
    use forskap_api::IssuableKind;

    use super::*;

    const NOW: i64 = 1_800_000_000;

    fn failure_3() -> FailedTask {
        FailedTask {
            id: 3,
            op: "post_time".to_string(),
            kind: IssuableKind::merge_request,
            project_id: 7,
            iid: 42,
            detail: "1h".to_string(),
            error: "403 Forbidden".to_string(),
            queued_at: 1_799_990_000,
            failed_at: 1_800_000_000,
        }
    }

    /// A write queued `ago` seconds before, untried.
    fn queued(id: i64, op: &str, iid: i64, detail: &str, ago: i64) -> QueuedWrite {
        QueuedWrite {
            id,
            op: op.to_string(),
            kind: IssuableKind::work_item,
            project_id: 7,
            iid,
            detail: detail.to_string(),
            queued_at: NOW - ago,
            attempts: 0,
            running: false,
            blocked: false,
            next_attempt_at: None,
            last_error: None,
            expires_at: NOW - ago + 7 * 86_400,
        }
    }

    fn listing(queued: Option<Vec<QueuedWrite>>, failures: Vec<FailedTask>) -> Listing {
        Listing {
            queued,
            paused_until: None,
            failures,
        }
    }

    /// With nothing waiting the view is the one it always was, as it is for
    /// a daemon too old to list its queue.
    #[test]
    fn render_lists_the_failures_and_what_to_do_with_them() {
        for queued in [None, Some(Vec::new())] {
            let text = render(&listing(queued, vec![failure_3()]), None, NOW);
            assert!(
                text.starts_with(
                    "[3] post_time !42 (1h)  —  403 Forbidden  (2027-01-15T08:00:00+00:00)\n\nretry"
                ),
                "{text}"
            );
        }
        assert_eq!(
            render(&listing(None, Vec::new()), None, NOW),
            "no failed writes\n"
        );
        assert_eq!(
            render(&listing(Some(Vec::new()), Vec::new()), None, NOW),
            "nothing queued, no failed writes\n"
        );

        style::force(true);
        let text = render(&listing(None, vec![failure_3()]), None, NOW);
        assert!(
            text.starts_with(
                "\x1b[2m[3]\x1b[0m post_time \x1b[36m!42\x1b[0m (1h)  —  \x1b[31m403 Forbidden\x1b[0m  \
                 \x1b[2m(2027-01-15T08:00:00+00:00)\x1b[0m\n\n\x1b[2mretry"
            ),
            "{text}"
        );
        style::force(false);
    }

    /// Each waiting write says what it waits for: its attempt, the write
    /// ahead of it, its backoff, or just its turn.
    #[test]
    fn render_lists_what_waits_above_what_failed() {
        let waiting = vec![
            QueuedWrite {
                attempts: 2,
                running: true,
                last_error: Some("network error: reset".to_string()),
                ..queued(11, "Close", 9, "", 600)
            },
            queued(12, "PostTime", 42, "1h", 180),
            QueuedWrite {
                blocked: true,
                ..queued(13, "Close", 42, "", 180)
            },
            QueuedWrite {
                kind: IssuableKind::merge_request,
                attempts: 3,
                next_attempt_at: Some(NOW + 240),
                last_error: Some("502 Bad Gateway".to_string()),
                ..queued(14, "PostTime", 7, "30m", 3600)
            },
        ];
        assert_eq!(
            render(&listing(Some(waiting), vec![failure_3()]), None, NOW),
            "\
queued
[11] Close #9  —  being sent  (queued 10m ago)
[12] PostTime #42 (1h)  —  sent next  (queued 3m ago)
[13] Close #42  —  after an earlier write to it  (queued 3m ago)
[14] PostTime !7 (30m)  —  failed 3 times: 502 Bad Gateway; next try in 4m  (queued 1h ago)

failed
[3] post_time !42 (1h)  —  403 Forbidden  (2027-01-15T08:00:00+00:00)

retry with `forskap queue retry <id>`, drop with `forskap queue dismiss <id>`, or `forskap queue clear`
"
        );
    }

    /// What holds every write is said once, above them: no session and why,
    /// or the rate limit's pause.
    #[test]
    fn render_says_once_what_holds_every_write() {
        let waiting = || {
            Some(vec![
                queued(12, "PostTime", 42, "1h", 180),
                QueuedWrite {
                    blocked: true,
                    ..queued(13, "Close", 42, "", 180)
                },
            ])
        };
        let why = "Logged out. Run `forskap auth login` to authenticate.";
        assert_eq!(
            render(&listing(waiting(), Vec::new()), Some(why), NOW),
            "\
no GitLab session: Logged out. Run `forskap auth login` to authenticate.

queued
[12] PostTime #42 (1h)  —  waits for a GitLab session  (queued 3m ago)
[13] Close #42  —  after an earlier write to it  (queued 3m ago)
"
        );
        let paused = Listing {
            paused_until: Some(NOW + 90),
            ..listing(waiting(), Vec::new())
        };
        assert_eq!(
            render(&paused, None, NOW),
            "\
paused by a GitLab rate limit for another 1m

queued
[12] PostTime #42 (1h)  —  after the pause  (queued 3m ago)
[13] Close #42  —  after an earlier write to it  (queued 3m ago)
"
        );
    }

    /// `--output json`: one object, the failures under the key they are, the
    /// waiting writes beside them where the daemon lists them.
    #[test]
    fn the_structured_output_is_one_object() {
        let both = listing(
            Some(vec![queued(12, "PostTime", 42, "1h", 180)]),
            vec![failure_3()],
        );
        let json = serde_json::to_value(&both).unwrap();
        assert_eq!(json["queued"][0]["id"], 12);
        assert_eq!(json["queued"][0]["blocked"], false);
        assert_eq!(json["failures"][0]["id"], 3);
        assert!(json.get("paused_until").is_none(), "{json}");

        let old = serde_json::to_value(listing(None, Vec::new())).unwrap();
        assert_eq!(old, serde_json::json!({"failures": []}));
    }
}
