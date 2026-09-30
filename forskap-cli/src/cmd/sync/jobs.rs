//! `forskap sync jobs` — what the daemon's sync worker is doing.
//!
//! The worker runs one job at a time, so a slow one holds the others up.
//! This lists every planned job in the order the worker runs them: the one
//! in flight, the ones demanded ahead of the schedule, the due ones, then
//! the rest by their next run.

use anyhow::Result;
use chrono::Utc;
use forskap_api::{GetSyncJobs_Reply, SyncJob, SyncJobStatus, VarlinkClientInterface};

use crate::cli::{OutputFormat, WatchArgs};
use crate::friendly::friendly;
use crate::{client, output, watch};

pub async fn run(format: OutputFormat, watch: WatchArgs) -> Result<()> {
    match watch::interval(watch, format)? {
        Some(every) => watch::run(every, || async { Ok(text(&fetch().await?)) }).await,
        None => output::emit(format, &fetch().await?, |reply| out!("{}", text(reply))),
    }
}

// Connects per call: a watch has to find a restarted daemon again.
async fn fetch() -> Result<GetSyncJobs_Reply> {
    let client = client::connect_default().await?;
    client
        .get_sync_jobs()
        .call()
        .await
        .map_err(|e| friendly("GetSyncJobs", e))
}

fn text(reply: &GetSyncJobs_Reply) -> String {
    render(&reply.jobs, reply.paused_until, Utc::now().timestamp())
}

/// The jobs as an aligned table, a job's last error on a line of its own.
fn render(jobs: &[SyncJob], paused_until: Option<i64>, now: i64) -> String {
    if jobs.is_empty() {
        return "no sync jobs planned\n".to_string();
    }
    let mut out = String::new();
    if let Some(until) = paused_until.filter(|&until| until > now) {
        out.push_str(&format!(
            "paused by a GitLab rate limit for another {}\n\n",
            span(until - now)
        ));
    }
    let rows: Vec<[String; 4]> = jobs
        .iter()
        .map(|j| {
            [
                j.key.clone(),
                status(j).to_string(),
                j.last_ok.map_or_else(
                    || "never".to_string(),
                    |at| format!("{} ago", span(now - at)),
                ),
                next(j, now),
            ]
        })
        .collect();
    let header = ["JOB", "STATUS", "LAST SYNC", "NEXT"].map(str::to_string);
    let width = |col: usize| {
        let cells = rows.iter().chain([&header]).map(|r| r[col].chars().count());
        cells.max().unwrap_or(0)
    };
    let (w0, w1, w2) = (width(0), width(1), width(2));
    let line = |[job, status, last, next]: &[String; 4]| {
        format!("{job:<w0$}  {status:<w1$}  {last:<w2$}  {next}\n")
    };
    out.push_str(&line(&header));
    for (row, job) in rows.iter().zip(jobs) {
        out.push_str(&line(row));
        if let Some(error) = &job.last_error {
            let times = match job.failures {
                0 => String::new(),
                1 => "failed once: ".to_string(),
                n => format!("failed {n} times: "),
            };
            out.push_str(&format!("    {times}{error}\n"));
        }
    }
    out
}

fn status(job: &SyncJob) -> &'static str {
    match job.status {
        SyncJobStatus::running => "running",
        SyncJobStatus::demanded => "demanded",
        SyncJobStatus::due => "due",
        SyncJobStatus::waiting => "waiting",
        SyncJobStatus::backing_off => "backing off",
    }
}

/// When the job runs next, or for how long it has been running.
fn next(job: &SyncJob, now: i64) -> String {
    match (&job.status, job.next_due) {
        (SyncJobStatus::running, _) => match job.running_since {
            Some(since) => format!("for {}", span(now - since)),
            None => "now".to_string(),
        },
        (SyncJobStatus::demanded, _) => "next".to_string(),
        (SyncJobStatus::due, _) => "now".to_string(),
        (_, Some(at)) if at > now => format!("in {}", span(at - now)),
        (_, Some(_)) => "now".to_string(),
        // Nothing left to do until what it syncs changes (an avatar).
        (_, None) => "-".to_string(),
    }
}

/// A span of seconds in its two largest units: `45s`, `12m`, `3h 5m`, `2d 4h`.
fn span(secs: i64) -> String {
    let secs = secs.max(0);
    let (days, hours, mins) = (secs / 86_400, secs % 86_400 / 3600, secs % 3600 / 60);
    match (days, hours, mins) {
        (0, 0, 0) => format!("{secs}s"),
        (0, 0, m) => format!("{m}m"),
        (0, h, 0) => format!("{h}h"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, 0, _) => format!("{d}d"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    fn job(key: &str, status: SyncJobStatus) -> SyncJob {
        SyncJob {
            key: key.to_string(),
            status,
            last_ok: None,
            next_due: None,
            running_since: None,
            failures: 0,
            last_error: None,
        }
    }

    #[test]
    fn span_keeps_the_two_largest_units() {
        for (secs, text) in [
            (-5, "0s"),
            (45, "45s"),
            (60, "1m"),
            (3599, "59m"),
            (3600, "1h"),
            (11_100, "3h 5m"),
            (86_400, "1d"),
            (187_200, "2d 4h"),
        ] {
            assert_eq!(span(secs), text);
        }
    }

    #[test]
    fn render_lines_the_jobs_up_with_relative_times() {
        let jobs = [
            SyncJob {
                last_ok: Some(NOW - 120),
                running_since: Some(NOW - 3),
                ..job("project/7/issues", SyncJobStatus::running)
            },
            job("member/groups", SyncJobStatus::demanded),
            SyncJob {
                last_ok: Some(NOW - 300),
                next_due: Some(NOW - 10),
                ..job("assigned/issues", SyncJobStatus::due)
            },
            SyncJob {
                last_ok: Some(NOW - 7200),
                next_due: Some(NOW + 480),
                ..job("events", SyncJobStatus::waiting)
            },
            SyncJob {
                next_due: Some(NOW + 3720),
                failures: 2,
                last_error: Some("403 Forbidden".to_string()),
                ..job("project/9/boards", SyncJobStatus::backing_off)
            },
            SyncJob {
                last_ok: Some(NOW - 90_000),
                ..job("project/7/avatar", SyncJobStatus::waiting)
            },
        ];
        assert_eq!(
            render(&jobs, Some(NOW + 240), NOW),
            "\
paused by a GitLab rate limit for another 4m

JOB               STATUS       LAST SYNC  NEXT
project/7/issues  running      2m ago     for 3s
member/groups     demanded     never      next
assigned/issues   due          5m ago     now
events            waiting      2h ago     in 8m
project/9/boards  backing off  never      in 1h 2m
    failed 2 times: 403 Forbidden
project/7/avatar  waiting      1d 1h ago  -
"
        );
    }

    #[test]
    fn render_leaves_out_a_pause_that_is_over() {
        let jobs = [job("events", SyncJobStatus::due)];
        assert!(render(&jobs, Some(NOW - 1), NOW).starts_with("JOB"));
        assert_eq!(render(&[], None, NOW), "no sync jobs planned\n");
    }
}
