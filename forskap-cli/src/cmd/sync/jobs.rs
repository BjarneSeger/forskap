//! `forskap sync jobs` — what the daemon's sync worker is doing.
//!
//! The worker runs a few jobs at a time, so slow ones hold the others up.
//! This lists every planned job in the order the worker runs them: the ones
//! in flight, the ones demanded ahead of the schedule, the due ones, then
//! the rest by their next run. Jobs that are done for good (a fetched avatar
//! per project) would drown the rest, so they share one line per kind.

use anyhow::Result;
use chrono::Utc;
use forskap_api::{GetSyncJobs_Reply, SyncJob, SyncJobStatus, VarlinkClientInterface};

use crate::cli::{OutputFormat, WatchArgs};
use crate::friendly::friendly;
use crate::{client, output, style, watch};

pub async fn run(all: bool, format: OutputFormat, watch: WatchArgs) -> Result<()> {
    match watch::interval(watch, format)? {
        Some(every) => watch::run(every, || async { Ok(text(&fetch().await?, all)) }).await,
        None => output::emit(format, &fetch().await?, |reply| {
            out!("{}", text(reply, all))
        }),
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

fn text(reply: &GetSyncJobs_Reply, all: bool) -> String {
    render(&reply.jobs, reply.paused_until, Utc::now().timestamp(), all)
}

/// One line of the table: a job, or the settled jobs of one kind.
struct Row<'a> {
    cells: [String; 4],
    error: Option<(&'a str, i64)>,
}

/// A job with nothing left to do and nothing to report: it ran, and is
/// never due again.
fn settled(job: &SyncJob) -> bool {
    job.status == SyncJobStatus::waiting
        && job.last_ok.is_some()
        && job.next_due.is_none()
        && job.last_error.is_none()
}

/// The key with its ids blanked: `project/7/avatar` is a `project/*/avatar`.
fn kind(key: &str) -> String {
    let blank = |part| match part {
        "" => part,
        id if id.bytes().all(|b| b.is_ascii_digit()) => "*",
        _ => part,
    };
    key.split('/').map(blank).collect::<Vec<_>>().join("/")
}

/// A row per job, in order; unless `all`, the settled jobs of a kind share
/// the row of the first, under their count and latest sync.
fn rows(jobs: &[SyncJob], now: i64, all: bool) -> Vec<Row<'_>> {
    let ago = |at: i64| format!("{} ago", span(now - at));
    let mut rows: Vec<Row> = Vec::with_capacity(jobs.len());
    // Kind, row, count and latest sync of each group.
    let mut groups: Vec<(String, usize, usize, i64)> = Vec::new();
    for job in jobs {
        if !all && settled(job) {
            let (kind, synced) = (kind(&job.key), job.last_ok.unwrap_or(0));
            if let Some(group) = groups.iter_mut().find(|g| g.0 == kind) {
                group.2 += 1;
                group.3 = group.3.max(synced);
                continue;
            }
            groups.push((kind, rows.len(), 1, synced));
        }
        rows.push(Row {
            cells: [
                job.key.clone(),
                status(job).to_string(),
                job.last_ok.map_or_else(|| "never".to_string(), ago),
                next(job, now),
            ],
            error: job.last_error.as_deref().map(|e| (e, job.failures)),
        });
    }
    for (kind, row, count, synced) in groups {
        if count > 1 {
            rows[row].cells[0] = format!("{kind} ({count})");
            rows[row].cells[2] = ago(synced);
        }
    }
    rows
}

/// The jobs as an aligned table, a job's last error on a line of its own.
fn render(jobs: &[SyncJob], paused_until: Option<i64>, now: i64, all: bool) -> String {
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
    let rows = rows(jobs, now, all);
    let header = ["JOB", "STATUS", "LAST SYNC", "NEXT"].map(str::to_string);
    let width = |col: usize| {
        let cells = rows.iter().map(|r| &r.cells).chain([&header]);
        cells.map(|r| r[col].chars().count()).max().unwrap_or(0)
    };
    let (w0, w1, w2) = (width(0), width(1), width(2));
    let line = |[job, status, last, next]: &[String; 4]| {
        let status = style::state(status);
        format!("{job:<w0$}  {status:<w1$}  {last:<w2$}  {next}\n")
    };
    let [job, status, last, next] = &header;
    let header = format!("{job:<w0$}  {status:<w1$}  {last:<w2$}  {next}");
    out.push_str(&format!("{}\n", style::heading(&header)));
    for row in &rows {
        out.push_str(&line(&row.cells));
        if let Some((error, failures)) = row.error {
            let times = match failures {
                0 => String::new(),
                1 => "failed once: ".to_string(),
                n => format!("failed {n} times: "),
            };
            let error = format!("{times}{error}");
            out.push_str(&format!("    {}\n", style::error(&error)));
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
            render(&jobs, Some(NOW + 240), NOW, false),
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
    fn render_colours_the_header_the_statuses_and_the_errors() {
        style::force(true);
        let jobs = [
            job("events", SyncJobStatus::running),
            SyncJob {
                failures: 1,
                last_error: Some("403 Forbidden".to_string()),
                ..job("project/9/boards", SyncJobStatus::backing_off)
            },
        ];
        // The columns line up as they do without the escapes.
        assert_eq!(
            render(&jobs, None, NOW, false),
            "\x1b[1mJOB               STATUS       LAST SYNC  NEXT\x1b[0m\n\
             events            \x1b[32mrunning    \x1b[0m  never      now\n\
             project/9/boards  \x1b[31mbacking off\x1b[0m  never      -\n\
             \x20   \x1b[31mfailed once: 403 Forbidden\x1b[0m\n"
        );
    }

    #[test]
    fn render_leaves_out_a_pause_that_is_over() {
        let jobs = [job("events", SyncJobStatus::due)];
        assert!(render(&jobs, Some(NOW - 1), NOW, false).starts_with("JOB"));
        assert_eq!(render(&[], None, NOW, false), "no sync jobs planned\n");
    }

    #[test]
    fn settled_jobs_of_a_kind_share_a_line() {
        let avatar = |project: i64, ago: i64| SyncJob {
            last_ok: Some(NOW - ago),
            ..job(&format!("project/{project}/avatar"), SyncJobStatus::waiting)
        };
        let jobs = [
            SyncJob {
                last_ok: Some(NOW - 60),
                next_due: Some(NOW + 60),
                ..job("events", SyncJobStatus::waiting)
            },
            avatar(7, 600),
            // Still to fetch, or failing: worth a line of its own.
            job("project/8/avatar", SyncJobStatus::due),
            avatar(9, 120),
            SyncJob {
                next_due: Some(NOW + 3600),
                failures: 1,
                last_error: Some("403 Forbidden".to_string()),
                ..job("project/10/avatar", SyncJobStatus::backing_off)
            },
            avatar(11, 300),
        ];
        assert_eq!(
            render(&jobs, None, NOW, false),
            "\
JOB                   STATUS       LAST SYNC  NEXT
events                waiting      1m ago     in 1m
project/*/avatar (3)  waiting      2m ago     -
project/8/avatar      due          never      now
project/10/avatar     backing off  never      in 1h
    failed once: 403 Forbidden
"
        );
        let every = render(&jobs, None, NOW, true);
        assert_eq!(every.lines().count(), 1 + jobs.len() + 1, "{every}");
        assert!(every.contains("project/11/avatar"), "{every}");

        // One settled job keeps its name.
        let single = render(&jobs[..2], None, NOW, false);
        assert!(single.contains("project/7/avatar "), "{single}");
    }
}
