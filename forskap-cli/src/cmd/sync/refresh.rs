//! `forskap sync refresh` — drop the daemon's caches and re-fetch.
//!
//! Use it after editing something in the GitLab UI when you don't want to wait
//! out the daemon's sync interval. The daemon replies once what it cleared of
//! the assigned lists and the history is re-synced; on a terminal a line on
//! stderr follows that sync until then. Where it replies without that — no
//! session, a rate limit, a failed fetch, its patience run out — a line says
//! what is still missing and why: the cache is cleared, but not fresh.

use std::convert::Infallible;
use std::io::{self, IsTerminal, Write};
use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use clap::ValueEnum;
use forskap_api::admin::{
    CacheScope, GetSyncJobs_Reply, SyncJob, SyncJobStatus, VarlinkClientInterface,
};

use super::jobs;
use crate::cli::RefreshScope;
use crate::client;
use crate::friendly::friendly;
use crate::style;
use crate::watch::{self, ERASE_LINE};

/// How often the sync is asked how far it is. The first look comes after
/// one such wait, so a quick refresh shows nothing.
const FOLLOW_EVERY: Duration = Duration::from_millis(500);

/// The daemon's `ClearCache` scopes behind each CLI scope.
fn wire(scope: RefreshScope) -> &'static [CacheScope] {
    match scope {
        RefreshScope::Assigned => &[CacheScope::assigned],
        RefreshScope::Search => &[CacheScope::search],
        // The daemon syncs the history in three age bands.
        RefreshScope::History => &[CacheScope::quick, CacheScope::slow, CacheScope::stale],
        RefreshScope::Usage => &[CacheScope::usage],
    }
}

pub async fn run(mut scopes: Vec<RefreshScope>) -> Result<()> {
    scopes.dedup();
    // No scope ⇒ `None`, which the daemon reads as "everything synced".
    let scope =
        (!scopes.is_empty()).then(|| scopes.iter().flat_map(|s| wire(*s)).cloned().collect());

    let client = client::connect_admin().await?;
    let started = Utc::now().timestamp();
    let mut clear = client.clear_cache(scope);
    // The follow ends with the call, and takes its line with it.
    let cleared = tokio::select! {
        cleared = clear.call() => cleared,
        never = follow() => match never {},
    };
    let cleared = cleared.map_err(|e| friendly("ClearCache", e))?;

    if scopes.is_empty() {
        outln!("{}", style::success("cache cleared"))?;
    } else {
        let names: Vec<_> = scopes
            .iter()
            .filter_map(|s| s.to_possible_value())
            .map(|v| v.get_name().to_string())
            .collect();
        outln!("{} {}", style::success("cleared:"), names.join(", "))?;
    }
    let pending = cleared.pending.unwrap_or_default();
    if let Some(why) = unfinished(&pending, started).await {
        outln!("{}", style::warning(&why))?;
    }
    Ok(())
}

/// Why the jobs `pending` after a clear begun at `started` have not synced
/// again, from one more look at the sync; `None` where they have meanwhile
/// or none were. Whatever fails here leaves at least their names.
async fn unfinished(pending: &[String], started: i64) -> Option<String> {
    if pending.is_empty() {
        return None;
    }
    let Ok(reply) = jobs::fetch().await else {
        return Some(format!("not synced again yet: {}", pending.join(", ")));
    };
    let on_hold = jobs::on_hold(&reply).await;
    let now = Utc::now().timestamp();
    why_unfinished(pending, started, &reply, on_hold.as_deref(), now)
}

/// [`unfinished`], from what the sync said: `reply`, and `on_hold` where it
/// has no session.
fn why_unfinished(
    pending: &[String],
    started: i64,
    reply: &GetSyncJobs_Reply,
    on_hold: Option<&str>,
    now: i64,
) -> Option<String> {
    let left: Vec<SyncJob> = reply
        .jobs
        .iter()
        .filter(|j| pending.contains(&j.key))
        // Finished between the two replies.
        .filter(|j| j.last_ok.is_none_or(|at| at < started))
        .cloned()
        .collect();
    if left.is_empty() {
        return None;
    }
    if let Some(why) = on_hold {
        return Some(format!("not synced again: {why}"));
    }
    if let Some(pause) = jobs::pause(reply.paused_until, now) {
        return Some(format!("not synced again yet: {pause}"));
    }
    let failed: Vec<String> = left
        .iter()
        .filter(|j| j.status != SyncJobStatus::running && j.last_error.is_some())
        .map(|j| {
            let how = jobs::failure(j.failures, j.last_error.as_deref());
            format!("{} {how}", j.key)
        })
        .collect();
    if !failed.is_empty() {
        return Some(format!("not synced again: {}", failed.join("; ")));
    }
    Some(match jobs::summary(&left, None, now) {
        at if at.starts_with("syncing") => format!("still {at}"),
        at if !at.is_empty() => format!("not synced again yet: {at}"),
        _ => {
            let keys: Vec<&str> = left.iter().map(|j| j.key.as_str()).collect();
            format!("not synced again yet: {}", keys.join(", "))
        }
    })
}

/// Show what the sync is at while the daemon holds its reply back. On its
/// own connection: the daemon answers one call at a time on each. Whatever
/// fails here is not the refresh failing, and stays silent.
async fn follow() -> Infallible {
    if !io::stderr().is_terminal() {
        std::future::pending::<()>().await;
    }
    let mut line = Line::default();
    loop {
        tokio::time::sleep(FOLLOW_EVERY).await;
        if let Ok(reply) = jobs::fetch().await {
            let now = Utc::now().timestamp();
            line.paint(&jobs::summary(&reply.jobs, reply.paused_until, now));
        }
    }
}

/// A status line on stderr, redrawn in place and gone when dropped.
#[derive(Default)]
struct Line {
    shown: bool,
}

impl Line {
    fn paint(&mut self, text: &str) {
        let cols = terminal_size::terminal_size_of(io::stderr()).map(|(w, _)| w.0.into());
        write(&frame(text, cols));
        self.shown = true;
    }
}

impl Drop for Line {
    fn drop(&mut self) {
        if self.shown {
            write(&frame("", None));
        }
    }
}

/// `text` drawn over the line the cursor is on, which it stays on.
fn frame(text: &str, cols: Option<usize>) -> String {
    let mut frame = format!("\r{ERASE_LINE}");
    // One column short: a line reaching the last one would wrap.
    let room = cols.map_or(usize::MAX, |cols| cols.saturating_sub(1));
    watch::cut(&mut frame, text, room);
    frame
}

// Not `eprint!`, which panics on a closed stderr.
fn write(frame: &str) {
    let mut stderr = io::stderr();
    let _ = stderr.write_all(frame.as_bytes());
    let _ = stderr.flush();
}

#[cfg(test)]
mod tests {
    use forskap_api::admin::SyncJobHold;

    use super::*;

    const NOW: i64 = 1_800_000_000;
    /// When the clear began.
    const STARTED: i64 = NOW - 31;

    /// A job never synced since the clear.
    fn job(key: &str, status: SyncJobStatus) -> SyncJob {
        SyncJob {
            key: key.to_string(),
            status,
            last_ok: None,
            next_due: None,
            running_since: None,
            failures: 0,
            last_error: None,
            unavailable: Some(false),
            full: None,
            fetched: None,
            expected: None,
            held_by: None,
            behind: None,
        }
    }

    fn sync(jobs: Vec<SyncJob>, paused_until: Option<i64>, connected: bool) -> GetSyncJobs_Reply {
        GetSyncJobs_Reply {
            jobs,
            paused_until,
            connected: Some(connected),
        }
    }

    fn pending() -> Vec<String> {
        ["assigned/issues", "timelogs/all"]
            .map(str::to_string)
            .into()
    }

    #[test]
    fn a_refresh_without_a_session_says_so() {
        let held = |key| SyncJob {
            held_by: Some(SyncJobHold::session),
            ..job(key, SyncJobStatus::due)
        };
        let reply = sync(
            vec![held("assigned/issues"), held("timelogs/all")],
            None,
            false,
        );
        let on_hold = "no GitLab session: Logged out. Run `forskap auth login` to authenticate.";
        assert_eq!(
            why_unfinished(&pending(), STARTED, &reply, Some(on_hold), NOW).unwrap(),
            "not synced again: no GitLab session: Logged out. \
             Run `forskap auth login` to authenticate."
        );
    }

    #[test]
    fn a_refresh_into_a_rate_limit_says_until_when() {
        let limited = SyncJob {
            last_error: Some("GitLab unavailable (429): slow down".to_string()),
            held_by: Some(SyncJobHold::rate_limit),
            ..job("assigned/issues", SyncJobStatus::demanded)
        };
        let reply = sync(vec![limited], Some(NOW + 240), true);
        assert_eq!(
            why_unfinished(&pending(), STARTED, &reply, None, NOW).unwrap(),
            "not synced again yet: paused by a GitLab rate limit for another 4m"
        );
    }

    #[test]
    fn a_refresh_whose_fetch_failed_names_the_job_and_the_failure() {
        let failed = SyncJob {
            next_due: Some(NOW + 60),
            failures: 1,
            last_error: Some("GitLab error: 500".to_string()),
            ..job("assigned/issues", SyncJobStatus::backing_off)
        };
        let done = SyncJob {
            last_ok: Some(NOW - 20),
            ..job("timelogs/all", SyncJobStatus::waiting)
        };
        let reply = sync(vec![failed, done], None, true);
        assert_eq!(
            why_unfinished(&pending(), STARTED, &reply, None, NOW).unwrap(),
            "not synced again: assigned/issues failed once: GitLab error: 500"
        );
    }

    /// The daemon waits half a minute at most; a longer refill goes on.
    #[test]
    fn a_refresh_that_outlasts_the_daemons_wait_says_how_far_it_is() {
        let running = SyncJob {
            running_since: Some(NOW - 30),
            fetched: Some(120),
            expected: Some(400),
            ..job("timelogs/recent", SyncJobStatus::running)
        };
        let behind = SyncJob {
            held_by: Some(SyncJobHold::lane),
            behind: Some("timelogs/recent".to_string()),
            ..job("timelogs/all", SyncJobStatus::demanded)
        };
        let reply = sync(vec![running.clone(), behind.clone()], None, true);
        let pending: Vec<String> = ["timelogs/recent", "timelogs/all"]
            .map(str::to_string)
            .into();
        assert_eq!(
            why_unfinished(&pending, STARTED, &reply, None, NOW).unwrap(),
            "still syncing timelogs/recent 120/400; 1 waiting behind timelogs/recent"
        );
        // Only what the clear waited for counts: another job running says
        // nothing about it.
        let reply = sync(vec![running, behind], None, true);
        assert_eq!(
            why_unfinished(&["timelogs/all".to_string()], STARTED, &reply, None, NOW).unwrap(),
            "not synced again yet: 1 waiting to sync"
        );
    }

    /// What landed between the daemon's reply and the look is no news.
    #[test]
    fn a_refresh_that_finished_meanwhile_says_nothing() {
        let done = |key| SyncJob {
            last_ok: Some(NOW - 5),
            ..job(key, SyncJobStatus::waiting)
        };
        let reply = sync(
            vec![done("assigned/issues"), done("timelogs/all")],
            None,
            true,
        );
        assert_eq!(why_unfinished(&pending(), STARTED, &reply, None, NOW), None);
        // Synced, but before the clear: still to come.
        let stale = SyncJob {
            last_ok: Some(STARTED - 60),
            ..job("assigned/issues", SyncJobStatus::due)
        };
        let reply = sync(vec![stale, done("timelogs/all")], None, true);
        assert_eq!(
            why_unfinished(&pending(), STARTED, &reply, None, NOW).unwrap(),
            "not synced again yet: assigned/issues"
        );
    }

    #[test]
    fn a_frame_replaces_the_line_and_stays_on_it() {
        assert_eq!(frame("syncing events", None), "\r\x1b[Ksyncing events");
        assert_eq!(frame("syncing events", Some(8)), "\r\x1b[Ksyncing");
        assert_eq!(frame("", Some(80)), "\r\x1b[K");
    }
}
