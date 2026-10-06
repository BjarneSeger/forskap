//! `forskap sync jobs` — what the daemon's sync worker is doing.
//!
//! The worker runs a few jobs at a time, so slow ones hold the others up.
//! This lists every planned job in the order the worker runs them: the ones
//! in flight, the ones demanded ahead of the schedule, the due ones, then
//! the rest by their next run. Jobs that are done for good (a fetched avatar
//! per project) would drown the rest, so they share one line per kind, and so
//! do the jobs GitLab refuses for good (a project's merge requests switched
//! off): those are no failures to fix, and the daemon asks again once a day.

use anyhow::Result;
use chrono::Utc;
use forskap_api::admin::{GetSyncJobs_Reply, SyncJob, SyncJobStatus, VarlinkClientInterface};

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
pub(super) async fn fetch() -> Result<GetSyncJobs_Reply> {
    let client = client::connect_admin().await?;
    client
        .get_sync_jobs()
        .call()
        .await
        .map_err(|e| friendly("GetSyncJobs", e))
}

fn text(reply: &GetSyncJobs_Reply, all: bool) -> String {
    render(&reply.jobs, reply.paused_until, Utc::now().timestamp(), all)
}

/// One line of the table: a job, or the settled or unavailable jobs of one
/// kind, with what is said beneath it.
struct Row<'a> {
    cells: [String; 4],
    below: Option<Below<'a>>,
}

/// The line beneath a row.
enum Below<'a> {
    /// Why it fails, and how often it did.
    Failure(&'a str, i64),
    /// Why GitLab refuses it, where the daemon still knows.
    Refused(Option<&'a str>),
    /// Several refused jobs share the row.
    RefusedAll,
}

/// A job with nothing left to do and nothing to report: it ran, and is
/// never due again.
fn settled(job: &SyncJob) -> bool {
    job.status == SyncJobStatus::waiting
        && job.last_ok.is_some()
        && job.next_due.is_none()
        && job.last_error.is_none()
}

/// A job GitLab refuses for good, as the daemon says; one too old to say
/// has none.
pub fn unavailable(job: &SyncJob) -> bool {
    job.unavailable == Some(true)
}

/// What jobs share a row, by kind, unless `--all`.
#[derive(PartialEq)]
enum Share {
    Settled,
    Unavailable,
}

/// The key with its ids blanked: `project/7/avatar` is a `project/*/avatar`.
pub fn kind(key: &str) -> String {
    let blank = |part| match part {
        "" => part,
        id if id.bytes().all(|b| b.is_ascii_digit()) => "*",
        _ => part,
    };
    key.split('/').map(blank).collect::<Vec<_>>().join("/")
}

/// The jobs of one kind sharing a row.
struct Group {
    share: Share,
    kind: String,
    row: usize,
    count: usize,
    /// The latest sync of any of them, 0 for none.
    synced: i64,
    /// The soonest next run of any of them.
    next: Option<i64>,
}

/// A row per job, in order; unless `all`, the settled jobs of a kind share
/// the row of the first, under their count and latest sync, and so do the
/// unavailable ones, under their soonest next attempt.
fn rows(jobs: &[SyncJob], now: i64, all: bool) -> Vec<Row<'_>> {
    let ago = |at: i64| format!("{} ago", span(now - at));
    let mut rows: Vec<Row> = Vec::with_capacity(jobs.len());
    let mut groups: Vec<Group> = Vec::new();
    for job in jobs {
        let share = if resting(job) {
            Some(Share::Unavailable)
        } else if settled(job) {
            Some(Share::Settled)
        } else {
            None
        };
        if let Some(share) = share.filter(|_| !all) {
            let (kind, synced) = (kind(&job.key), job.last_ok.unwrap_or(0));
            let group = groups
                .iter_mut()
                .find(|g| g.share == share && g.kind == kind);
            if let Some(group) = group {
                group.count += 1;
                group.synced = group.synced.max(synced);
                group.next = group.next.into_iter().chain(job.next_due).min();
                continue;
            }
            groups.push(Group {
                share,
                kind,
                row: rows.len(),
                count: 1,
                synced,
                next: job.next_due,
            });
        }
        let below = if unavailable(job) {
            Some(Below::Refused(job.last_error.as_deref()))
        } else {
            job.last_error
                .as_deref()
                .map(|e| Below::Failure(e, job.failures))
        };
        rows.push(Row {
            cells: [
                job.key.clone(),
                status(job).to_string(),
                job.last_ok.map_or_else(|| "never".to_string(), ago),
                next(job, now),
            ],
            below,
        });
    }
    for group in groups.into_iter().filter(|g| g.count > 1) {
        let row = &mut rows[group.row];
        row.cells[0] = format!("{} ({})", group.kind, group.count);
        if group.synced > 0 {
            row.cells[2] = ago(group.synced);
        }
        if group.share == Share::Unavailable {
            row.cells[3] = group
                .next
                .map_or_else(|| "-".to_string(), |at| when(at, now));
            row.below = Some(Below::RefusedAll);
        }
    }
    rows
}

/// The jobs as an aligned table, a job's last error on a line of its own.
fn render(jobs: &[SyncJob], paused_until: Option<i64>, now: i64, all: bool) -> String {
    if jobs.is_empty() {
        return format!("{}\n", style::muted("no sync jobs planned"));
    }
    let mut out = String::new();
    if let Some(pause) = pause(paused_until, now) {
        out.push_str(&format!("{}\n\n", style::warning(&pause)));
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
        let (last, next) = (style::muted(last), style::muted(next));
        format!("{job:<w0$}  {status:<w1$}  {last:<w2$}  {next}\n")
    };
    let [job, status, last, next] = &header;
    let header = format!("{job:<w0$}  {status:<w1$}  {last:<w2$}  {next}");
    out.push_str(&format!("{}\n", style::heading(&header)));
    for row in &rows {
        out.push_str(&line(&row.cells));
        match row.below {
            Some(Below::Failure(error, failures)) => {
                let error = failure(failures, Some(error));
                out.push_str(&format!("    {}\n", style::error(&error)));
            }
            Some(Below::Refused(error)) => {
                let why = refused(error);
                out.push_str(&format!("    {}\n", style::muted(&why)));
            }
            Some(Below::RefusedAll) => {
                let why = "refused by GitLab; `--all` lists each";
                out.push_str(&format!("    {}\n", style::muted(why)));
            }
            None => {}
        }
    }
    out
}

/// Why GitLab refuses a job, as far as the daemon still knows: its last
/// error is gone after a restart.
fn refused(error: Option<&str>) -> String {
    match error {
        Some(error) => format!("refused by GitLab: {error}"),
        None => "refused by GitLab".to_string(),
    }
}

/// What the sync is at, on one line, for a command waiting on it:
/// `syncing assigned/issues 12/40, timelogs/all 300/?; 2 waiting`. Empty
/// while nothing runs or waits.
pub(super) fn summary(jobs: &[SyncJob], paused_until: Option<i64>, now: i64) -> String {
    let of = |status| jobs.iter().filter(move |j| j.status == status);
    let running: Vec<String> = of(SyncJobStatus::running)
        .map(|job| match counts(job) {
            Some(counts) => format!("{} {counts}", job.key),
            None => job.key.clone(),
        })
        .collect();
    let waiting = of(SyncJobStatus::demanded).count();
    match (running.is_empty(), waiting) {
        (false, 0) => format!("syncing {}", running.join(", ")),
        (false, n) => format!("syncing {}; {n} waiting", running.join(", ")),
        (true, n) => match pause(paused_until, now) {
            Some(pause) => pause,
            None if n > 0 => format!("{n} waiting to sync"),
            None => String::new(),
        },
    }
}

/// The rate-limit pause, while it lasts.
pub fn pause(paused_until: Option<i64>, now: i64) -> Option<String> {
    let until = paused_until.filter(|&until| until > now)?;
    Some(format!(
        "paused by a GitLab rate limit for another {}",
        span(until - now)
    ))
}

/// How a job has been failing: `failed 2 times: 403 Forbidden`. The error
/// is gone after a daemon restart, the count is not.
pub fn failure(failures: i64, error: Option<&str>) -> String {
    let times = match failures {
        0 => None,
        1 => Some("failed once".to_string()),
        n => Some(format!("failed {n} times")),
    };
    match (times, error) {
        (Some(times), Some(error)) => format!("{times}: {error}"),
        (Some(times), None) => times,
        (None, Some(error)) => error.to_string(),
        (None, None) => "failing".to_string(),
    }
}

/// An unavailable job that isn't being tried again right now.
fn resting(job: &SyncJob) -> bool {
    let trying = matches!(job.status, SyncJobStatus::running | SyncJobStatus::demanded);
    unavailable(job) && !trying
}

fn status(job: &SyncJob) -> &'static str {
    if resting(job) {
        return "unavailable";
    }
    match job.status {
        SyncJobStatus::running => "running",
        SyncJobStatus::demanded => "demanded",
        SyncJobStatus::due => "due",
        SyncJobStatus::waiting => "waiting",
        SyncJobStatus::backing_off => "backing off",
    }
}

/// How far a running job is: `400/1000`, or `400/?` where GitLab announced
/// no total. Nothing before its first row, or from a daemon too old to say.
fn counts(job: &SyncJob) -> Option<String> {
    let fetched = job.fetched.unwrap_or(0);
    match job.expected.filter(|&expected| expected > 0) {
        // A total can fall short of the rows (GitLab counted before they
        // changed): never more than all of them.
        Some(expected) => Some(format!("{}/{expected}", fetched.min(expected))),
        None if fetched > 0 => Some(format!("{fetched}/?")),
        None => None,
    }
}

/// When the job runs next, or for how long it has been running and how far
/// it is.
fn next(job: &SyncJob, now: i64) -> String {
    match (&job.status, job.next_due) {
        (SyncJobStatus::running, _) => {
            let mut cell = match job.running_since {
                Some(since) => format!("for {}", span(now - since)),
                None => "now".to_string(),
            };
            if let Some(counts) = counts(job) {
                cell.push_str(&format!(" · {counts}"));
            }
            if job.full == Some(true) {
                cell.push_str(" (full)");
            }
            cell
        }
        (SyncJobStatus::demanded, _) => "next".to_string(),
        (SyncJobStatus::due, _) => "now".to_string(),
        (_, Some(at)) => when(at, now),
        // Nothing left to do until what it syncs changes (an avatar).
        (_, None) => "-".to_string(),
    }
}

/// A time to come relative to `now`: `in 3h 5m`, or `now` once it passed.
fn when(at: i64, now: i64) -> String {
    if at > now {
        format!("in {}", span(at - now))
    } else {
        "now".to_string()
    }
}

/// A span of seconds in its two largest units: `45s`, `12m`, `3h 5m`, `2d 4h`.
pub fn span(secs: i64) -> String {
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
            // What a daemon too old to say sends.
            unavailable: None,
            full: None,
            fetched: None,
            expected: None,
        }
    }

    /// A job GitLab refuses for good, resting until `next_in` from now.
    fn refused_job(key: &str, next_in: i64) -> SyncJob {
        SyncJob {
            next_due: Some(NOW + next_in),
            failures: 3,
            last_error: Some("GitLab error: 403 Forbidden".to_string()),
            unavailable: Some(true),
            ..job(key, SyncJobStatus::waiting)
        }
    }

    /// The owner's account: boards and merge requests switched off in a
    /// few projects, next to a job that fails for real.
    fn refusals() -> Vec<SyncJob> {
        vec![
            SyncJob {
                last_ok: Some(NOW - 60),
                next_due: Some(NOW + 60),
                unavailable: Some(false),
                ..job("events", SyncJobStatus::waiting)
            },
            refused_job("project/61/boards", 80_000),
            refused_job("project/60/merge_requests", 86_000),
            refused_job("project/62/boards", 70_000),
            SyncJob {
                next_due: Some(NOW + 3600),
                failures: 1,
                last_error: Some("GitLab error: 403 Forbidden".to_string()),
                unavailable: Some(false),
                ..job("project/9/issues", SyncJobStatus::backing_off)
            },
        ]
    }

    fn running(key: &str, secs: i64, fetched: i64, expected: Option<i64>) -> SyncJob {
        SyncJob {
            running_since: Some(NOW - secs),
            fetched: Some(fetched),
            expected,
            ..job(key, SyncJobStatus::running)
        }
    }

    /// A running job says how far it is: of GitLab's total, of `?` without
    /// one, and nothing before its first row.
    #[test]
    fn a_running_job_shows_its_rows_and_whether_it_runs_full() {
        let jobs = [
            SyncJob {
                full: Some(true),
                ..running("project/42/issues", 8, 400, Some(1000))
            },
            SyncJob {
                last_ok: Some(NOW - 7200),
                full: Some(false),
                ..running("all/issues", 3, 120, None)
            },
            // A total that fell short of the rows.
            running("events", 2, 1003, Some(1000)),
            SyncJob {
                full: Some(true),
                ..running("project/42/merge_requests", 1, 0, None)
            },
            running("project/42/avatar", 1, 0, None),
            // A daemon too old to say.
            SyncJob {
                running_since: Some(NOW - 3),
                ..job("assigned/issues", SyncJobStatus::running)
            },
            SyncJob {
                last_ok: Some(NOW - 300),
                ..job("timelogs/recent", SyncJobStatus::demanded)
            },
        ];
        assert_eq!(
            render(&jobs, None, NOW, false),
            "JOB                        STATUS    LAST SYNC  NEXT
project/42/issues          running   never      for 8s · 400/1000 (full)
all/issues                 running   2h ago     for 3s · 120/?
events                     running   never      for 2s · 1000/1000
project/42/merge_requests  running   never      for 1s (full)
project/42/avatar          running   never      for 1s
assigned/issues            running   never      for 3s
timelogs/recent            demanded  5m ago     next
"
        );
    }

    #[test]
    fn the_summary_names_what_runs_and_counts_what_waits() {
        let waiting = || job("timelogs/all", SyncJobStatus::demanded);
        let jobs = [
            running("assigned/issues", 2, 12, Some(40)),
            running("timelogs/recent", 2, 300, None),
            running("events", 1, 0, None),
            waiting(),
            waiting(),
            job("all/issues", SyncJobStatus::due),
        ];
        assert_eq!(
            summary(&jobs, None, NOW),
            "syncing assigned/issues 12/40, timelogs/recent 300/?, events; 2 waiting"
        );
        assert_eq!(
            summary(&jobs[..1], None, NOW),
            "syncing assigned/issues 12/40"
        );
        assert_eq!(summary(&jobs[3..], None, NOW), "2 waiting to sync");
        assert_eq!(
            summary(&jobs[3..], Some(NOW + 90), NOW),
            "paused by a GitLab rate limit for another 1m"
        );
        assert_eq!(summary(&jobs[5..], None, NOW), "");
    }

    /// Refused jobs are no failures: they say `unavailable` and why, without
    /// a count, and share a line per kind like the settled ones, under their
    /// soonest next attempt.
    #[test]
    fn unavailable_jobs_say_so_and_share_a_line_per_kind() {
        assert_eq!(
            render(&refusals(), None, NOW, false),
            "\
JOB                        STATUS       LAST SYNC  NEXT
events                     waiting      1m ago     in 1m
project/*/boards (2)       unavailable  never      in 19h 26m
    refused by GitLab; `--all` lists each
project/60/merge_requests  unavailable  never      in 23h 53m
    refused by GitLab: GitLab error: 403 Forbidden
project/9/issues           backing off  never      in 1h
    failed once: GitLab error: 403 Forbidden
"
        );
    }

    #[test]
    fn all_lists_each_unavailable_job_with_its_reason() {
        let every = render(&refusals(), None, NOW, true);
        assert!(
            every.contains(
                "\
project/61/boards          unavailable  never      in 22h 13m
    refused by GitLab: GitLab error: 403 Forbidden
"
            ),
            "{every}"
        );
        assert!(
            every.contains("project/62/boards          unavailable  never      in 19h 26m\n"),
            "{every}"
        );
        assert!(!every.contains("--all"), "{every}");
        assert!(!every.contains("failed 3 times"), "{every}");
        // A header, five jobs and a line beneath each but the events.
        assert_eq!(every.lines().count(), 1 + 5 + 4, "{every}");
    }

    #[test]
    fn an_unavailable_job_tried_again_or_after_a_restart_keeps_a_line() {
        let jobs = [
            // Demanded: being tried again, so it isn't resting.
            SyncJob {
                status: SyncJobStatus::running,
                running_since: Some(NOW - 2),
                ..refused_job("project/61/boards", 0)
            },
            // The daemon restarted: no error kept.
            SyncJob {
                last_error: None,
                ..refused_job("project/62/boards", 3600)
            },
        ];
        assert_eq!(
            render(&jobs, None, NOW, false),
            "\
JOB                STATUS       LAST SYNC  NEXT
project/61/boards  running      never      for 2s
    refused by GitLab: GitLab error: 403 Forbidden
project/62/boards  unavailable  never      in 1h
    refused by GitLab
"
        );
    }

    /// A daemon too old to say sends no `unavailable`: its refused jobs show
    /// as the failures it reports them as.
    #[test]
    fn without_the_field_refused_jobs_fail_as_before() {
        let old: Vec<SyncJob> = refusals()
            .into_iter()
            .map(|j| SyncJob {
                unavailable: None,
                status: if j.failures > 0 {
                    SyncJobStatus::backing_off
                } else {
                    j.status
                },
                ..j
            })
            .collect();
        let text = render(&old, None, NOW, false);
        assert!(!text.contains("unavailable"), "{text}");
        assert!(!text.contains("refused"), "{text}");
        assert_eq!(text.matches("failed 3 times").count(), 3, "{text}");
        assert!(text.contains("project/62/boards"), "{text}");
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
    fn failure_counts_the_runs_and_keeps_the_error_when_there_is_one() {
        assert_eq!(
            failure(1, Some("403 Forbidden")),
            "failed once: 403 Forbidden"
        );
        assert_eq!(
            failure(8, Some("403 Forbidden")),
            "failed 8 times: 403 Forbidden"
        );
        // After a daemon restart: the count without the error.
        assert_eq!(failure(3, None), "failed 3 times");
        assert_eq!(failure(0, Some("403 Forbidden")), "403 Forbidden");
        assert_eq!(failure(0, None), "failing");
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
             events            \x1b[32mrunning    \x1b[0m  \x1b[2mnever    \x1b[0m  \x1b[2mnow\x1b[0m\n\
             project/9/boards  \x1b[31mbacking off\x1b[0m  \x1b[2mnever    \x1b[0m  \x1b[2m-\x1b[0m\n\
             \x20   \x1b[31mfailed once: 403 Forbidden\x1b[0m\n"
        );
        // A rate-limit pause is a warning.
        let paused = render(&jobs, Some(NOW + 240), NOW, false);
        assert!(
            paused.starts_with("\x1b[33mpaused by a GitLab rate limit for another 4m\x1b[0m\n\n"),
            "{paused}"
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
