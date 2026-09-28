//! The sync jobs: what each one fetches, how often, and how its rows land in
//! the store.
//!
//! A job runs in two phases. [`fetch`] talks to GitLab and writes nothing;
//! it returns a [`Staged`] commit the worker applies in one batch together
//! with the job's new state. Dropping a fetch mid-flight therefore never
//! leaves partial data behind.

use std::sync::Arc;

use tracing::{info, warn};

use super::model::{Board, Event, Group, Issue, MergeRequest, Project, Resource, RowKey, Timelog};
use super::schedule::{Cadence, JobState, fingerprint};
use super::store::{Commit, RowScope, Stored, View};
use crate::config::Config;
use crate::error::Result;
use crate::gitlab::{GitlabApi, Issuable, Listing};
use crate::write::{Write, WriteOp};

/// View of the open issues assigned to the user.
pub const ASSIGNED_ISSUES: &str = "assigned/issues";
/// View of the open merge requests assigned to the user.
pub const ASSIGNED_MERGE_REQUESTS: &str = "assigned/merge_requests";

/// How far a delta's `updated_after` cursor reaches back before the previous
/// run started, so items updated during that run (or under clock skew) are
/// fetched again instead of missed. Upserts dedupe the overlap.
const DELTA_OVERLAP_SECS: u64 = 300;

/// Every job the planner can schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Job {
    AssignedIssues,
    AssignedMergeRequests,
    /// Timelogs inside `refresh.quick.window_hours`.
    RecentTimelogs,
    /// The user's contribution events: evidence for the tracked population.
    Events,
    ProjectBoards(i64),
    MemberProjects,
    MemberGroups,
    ProjectIssues(i64),
    ProjectMergeRequests(i64),
    AllIssues,
    AllMergeRequests,
    /// Timelogs inside `history.retention_hours`; prunes older ones.
    AllTimelogs,
}

impl Job {
    /// What the foreground views read; refilled first and never delayed.
    pub const FOREGROUND: [Job; 3] = [
        Job::AssignedIssues,
        Job::AssignedMergeRequests,
        Job::RecentTimelogs,
    ];

    /// Stable id: the job state's storage key and the log field.
    pub fn key(&self) -> String {
        match self {
            Self::AssignedIssues => ASSIGNED_ISSUES.into(),
            Self::AssignedMergeRequests => ASSIGNED_MERGE_REQUESTS.into(),
            Self::RecentTimelogs => "timelogs/recent".into(),
            Self::Events => "events".into(),
            Self::ProjectBoards(p) => format!("project/{p}/boards"),
            Self::MemberProjects => "member/projects".into(),
            Self::MemberGroups => "member/groups".into(),
            Self::ProjectIssues(p) => format!("project/{p}/issues"),
            Self::ProjectMergeRequests(p) => format!("project/{p}/merge_requests"),
            Self::AllIssues => "all/issues".into(),
            Self::AllMergeRequests => "all/merge_requests".into(),
            Self::AllTimelogs => "timelogs/all".into(),
        }
    }

    /// Lower runs first when several jobs are due.
    pub fn priority(&self) -> u8 {
        match self {
            Self::AssignedIssues | Self::AssignedMergeRequests | Self::RecentTimelogs => 0,
            Self::Events => 1,
            Self::AllTimelogs => 3,
            _ => 2,
        }
    }

    /// Whether a successful run can change which jobs are planned.
    pub fn feeds_plan(&self) -> bool {
        matches!(
            self,
            Self::AssignedIssues
                | Self::AssignedMergeRequests
                | Self::RecentTimelogs
                | Self::AllTimelogs
                | Self::Events
                | Self::MemberProjects
        )
    }

    pub fn cadence(&self, c: &Config) -> Cadence {
        let full_only = |every| Cadence {
            every,
            full_every: None,
        };
        match self {
            Self::AssignedIssues | Self::AssignedMergeRequests | Self::RecentTimelogs => {
                full_only(c.refresh.quick.interval_secs)
            }
            // Memberships rarely change, and a large one pages for a while.
            Self::ProjectBoards(_)
            | Self::AllTimelogs
            | Self::MemberProjects
            | Self::MemberGroups => full_only(c.refresh.slow.interval_secs),
            Self::ProjectIssues(_)
            | Self::ProjectMergeRequests(_)
            | Self::AllIssues
            | Self::AllMergeRequests => Cadence {
                every: c.search.partial_interval_secs,
                full_every: Some(c.search.full_interval_secs),
            },
            // Events never change once created: after the first run fills
            // the window, deltas are all there is.
            Self::Events => Cadence {
                every: c.search.partial_interval_secs,
                full_every: Some(u64::MAX),
            },
        }
    }

    /// What a success is valid for; a change (schema bump, wider window)
    /// makes the job due at once and full.
    pub fn fingerprint(&self, c: &Config) -> u64 {
        let schema = u64::from(match self {
            Self::AssignedIssues | Self::ProjectIssues(_) | Self::AllIssues => Issue::SCHEMA,
            Self::AssignedMergeRequests
            | Self::ProjectMergeRequests(_)
            | Self::AllMergeRequests => MergeRequest::SCHEMA,
            Self::RecentTimelogs | Self::AllTimelogs => Timelog::SCHEMA,
            Self::Events => Event::SCHEMA,
            Self::ProjectBoards(_) => Board::SCHEMA,
            Self::MemberProjects => Project::SCHEMA,
            Self::MemberGroups => Group::SCHEMA,
        });
        match self {
            Self::ProjectIssues(_) | Self::ProjectMergeRequests(_) => {
                fingerprint(&[schema, c.search.max_items_per_project])
            }
            Self::RecentTimelogs => fingerprint(&[schema, c.refresh.quick.window_hours]),
            Self::AllTimelogs => fingerprint(&[schema, c.history.retention_hours]),
            Self::Events => fingerprint(&[schema, c.search.tracked_retention_hours]),
            _ => fingerprint(&[schema]),
        }
    }

    /// The jobs to rerun so a write shows up, whichever of them are planned.
    pub fn affected_by(write: &Write) -> Vec<Job> {
        let pid = write.project_id;
        match (&write.op, write.kind) {
            (WriteOp::PostTime { .. }, _) => vec![Self::RecentTimelogs],
            (_, Issuable::Issue) => vec![Self::AssignedIssues, Self::ProjectIssues(pid)],
            (_, Issuable::MergeRequest) => {
                vec![Self::AssignedMergeRequests, Self::ProjectMergeRequests(pid)]
            }
        }
    }
}

/// The windows and limits a run needs, snapshotted from the config.
#[derive(Debug, Clone, Copy)]
pub struct Windows {
    pub quick: u64,
    pub retention: u64,
    pub tracked: u64,
    /// Most issues (and most MRs) fetched per project.
    pub project_cap: usize,
}

impl Windows {
    pub fn from_config(c: &Config) -> Self {
        Self {
            quick: c.refresh.quick.window().as_secs(),
            retention: c.history.retention().as_secs(),
            tracked: c.search.tracked_retention().as_secs(),
            project_cap: usize::try_from(c.search.max_items_per_project).unwrap_or(usize::MAX),
        }
    }
}

/// Everything one run needs besides the job itself.
pub struct FetchCtx {
    pub gitlab: Arc<dyn GitlabApi>,
    pub full: bool,
    pub state: JobState,
    /// Unix seconds the run started; becomes `last_ok` on success.
    pub started: u64,
    pub windows: Windows,
}

impl FetchCtx {
    /// `updated_after` for a delta run, `None` for a full one.
    fn updated_after(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        (!self.full).then(|| {
            let cursor = self.state.last_ok.saturating_sub(DELTA_OVERLAP_SECS);
            chrono::DateTime::from_timestamp(cursor as i64, 0).unwrap_or_default()
        })
    }
}

type Apply = Box<dyn FnOnce(&mut Commit<'_>) -> Result<usize> + Send>;

/// A fetched run's writes, applied by the worker in one batch. Yields the
/// number of rows fetched, for the log.
pub struct Staged(Apply);

impl Staged {
    fn new(f: impl FnOnce(&mut Commit<'_>) -> Result<usize> + Send + 'static) -> Self {
        Self(Box::new(f))
    }

    pub fn apply(self, commit: &mut Commit<'_>) -> Result<usize> {
        (self.0)(commit)
    }
}

/// Fetch one run of `job`.
pub async fn fetch(job: Job, ctx: FetchCtx) -> Result<Staged> {
    let gitlab = &*ctx.gitlab;
    match job {
        Job::AssignedIssues => {
            view::<Issue>(
                gitlab,
                Listing::AssignedIssues,
                ASSIGNED_ISSUES,
                ctx.started,
            )
            .await
        }
        Job::AssignedMergeRequests => {
            view::<MergeRequest>(
                gitlab,
                Listing::AssignedMergeRequests,
                ASSIGNED_MERGE_REQUESTS,
                ctx.started,
            )
            .await
        }
        Job::ProjectIssues(project_id) => {
            let listing = Listing::ProjectIssues {
                project_id,
                updated_after: ctx.updated_after(),
            };
            project_rows::<Issue>(&ctx, listing, project_id).await
        }
        Job::ProjectMergeRequests(project_id) => {
            let listing = Listing::ProjectMergeRequests {
                project_id,
                updated_after: ctx.updated_after(),
            };
            project_rows::<MergeRequest>(&ctx, listing, project_id).await
        }
        Job::AllIssues => {
            let listing = Listing::AllIssues {
                updated_after: ctx.updated_after(),
            };
            rows::<Issue>(gitlab, listing, ctx.full.then_some(RowScope::All)).await
        }
        Job::AllMergeRequests => {
            let listing = Listing::AllMergeRequests {
                updated_after: ctx.updated_after(),
            };
            rows::<MergeRequest>(gitlab, listing, ctx.full.then_some(RowScope::All)).await
        }
        Job::MemberProjects => {
            rows::<Project>(gitlab, Listing::MemberProjects, Some(RowScope::All)).await
        }
        Job::MemberGroups => {
            rows::<Group>(gitlab, Listing::MemberGroups, Some(RowScope::All)).await
        }
        Job::ProjectBoards(project_id) => {
            let fetched = fetch_rows(
                gitlab,
                &Listing::ProjectBoards { project_id },
                None,
                |b: &mut Board| b.project_id = project_id,
            )
            .await?;
            let scope = RowScope::Prefix(project_id.max(0) as u64);
            Ok(Staged::new(move |c| {
                c.upsert(&fetched)?;
                c.reconcile(scope, &fetched)?;
                Ok(fetched.len())
            }))
        }
        Job::Events => events(&ctx).await,
        Job::RecentTimelogs => timelogs(&ctx, ctx.windows.quick, false).await,
        Job::AllTimelogs => timelogs(&ctx, ctx.windows.retention, true).await,
    }
}

/// One project's issues or MRs, newest first and capped: a huge project
/// keeps only its most recently updated items, and a full run's reconcile
/// drops the rest.
async fn project_rows<R: Stored>(
    ctx: &FetchCtx,
    listing: Listing,
    project_id: i64,
) -> Result<Staged> {
    let cap = ctx.windows.project_cap;
    let fetched: Vec<R> = fetch_rows(&*ctx.gitlab, &listing, Some(cap), |_| {}).await?;
    if fetched.len() >= cap {
        info!(
            project_id,
            kind = R::NAME,
            cap,
            "project exceeds search.max_items_per_project; keeping the most recently updated"
        );
    }
    let reconcile = ctx
        .full
        .then_some(RowScope::Prefix(project_id.max(0) as u64));
    Ok(Staged::new(move |c| {
        c.upsert(&fetched)?;
        if let Some(scope) = reconcile {
            c.reconcile(scope, &fetched)?;
        }
        Ok(fetched.len())
    }))
}

/// Fetch `listing` as `R` rows; a full run (`reconcile = Some`) also drops
/// stored rows in that scope the listing no longer returns.
async fn rows<R: Stored>(
    gitlab: &dyn GitlabApi,
    listing: Listing,
    reconcile: Option<RowScope>,
) -> Result<Staged> {
    let fetched: Vec<R> = fetch_rows(gitlab, &listing, None, |_| {}).await?;
    Ok(Staged::new(move |c| {
        c.upsert(&fetched)?;
        if let Some(scope) = reconcile {
            c.reconcile(scope, &fetched)?;
        }
        Ok(fetched.len())
    }))
}

/// Fetch `listing` into the rows plus the view `name` listing their keys.
async fn view<R: Stored>(
    gitlab: &dyn GitlabApi,
    listing: Listing,
    name: &'static str,
    started: u64,
) -> Result<Staged> {
    let fetched: Vec<R> = fetch_rows(gitlab, &listing, None, |_| {}).await?;
    Ok(Staged::new(move |c| {
        c.upsert(&fetched)?;
        let keys: Vec<RowKey> = fetched.iter().map(Resource::key).collect();
        c.set_view(
            name,
            &View {
                keys,
                fetched_at: started,
            },
        )?;
        Ok(fetched.len())
    }))
}

/// Events inside the tracked window: the whole window on a full run, else
/// since the day before the last run (GitLab's `after` is a date and
/// exclusive). Older rows are pruned.
async fn events(ctx: &FetchCtx) -> Result<Staged> {
    let window_start = ctx.started.saturating_sub(ctx.windows.tracked);
    let from = if ctx.full {
        window_start
    } else {
        ctx.state.last_ok.max(window_start)
    };
    let after = chrono::DateTime::from_timestamp(from as i64, 0)
        .unwrap_or_default()
        .date_naive()
        .pred_opt();
    let fetched: Vec<Event> = fetch_rows(
        &*ctx.gitlab,
        &Listing::Events { after },
        None,
        |_: &mut Event| {},
    )
    .await?
    .into_iter()
    .filter(|e| e.created_at >= window_start)
    .collect();
    Ok(Staged::new(move |c| {
        c.upsert(&fetched)?;
        c.remove_where::<Event>(RowScope::Before(window_start), |_| false)?;
        Ok(fetched.len())
    }))
}

/// Timelogs spent inside the last `window` seconds, reconciled so deletions
/// in GitLab disappear here too; `prune` also drops everything older.
async fn timelogs(ctx: &FetchCtx, window: u64, prune: bool) -> Result<Staged> {
    let since = ctx.started.saturating_sub(window);
    let since_dt = chrono::DateTime::from_timestamp(since as i64, 0).unwrap_or_default();
    let fetched: Vec<Timelog> = ctx
        .gitlab
        .list_timelogs(since_dt)
        .await?
        .into_iter()
        .filter(|t| t.is_valid() && t.spent_at >= since)
        .collect();
    Ok(Staged::new(move |c| {
        c.upsert(&fetched)?;
        c.reconcile(RowScope::Since(since), &fetched)?;
        if prune {
            c.remove_where::<Timelog>(RowScope::Before(since), |_| false)?;
        }
        Ok(fetched.len())
    }))
}

/// Fetch a listing and deserialize each row as `R`, `stamp`ing in fields the
/// response lacks. Rows that don't parse or validate are skipped: one bad
/// row must not cost the whole listing.
pub async fn fetch_rows<R: Resource>(
    gitlab: &dyn GitlabApi,
    listing: &Listing,
    limit: Option<usize>,
    stamp: impl Fn(&mut R),
) -> Result<Vec<R>> {
    let raw = gitlab.list(listing, limit).await?;
    let total = raw.len();
    let rows: Vec<R> = raw
        .into_iter()
        .filter_map(|v| serde_json::from_value::<R>(v).ok())
        .map(|mut r| {
            stamp(&mut r);
            r
        })
        .filter(Resource::is_valid)
        .collect();
    if rows.len() < total {
        warn!(
            kind = R::NAME,
            dropped = total - rows.len(),
            "skipped malformed rows from GitLab"
        );
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::store::SyncStore;
    use crate::testing::{FakeGitlab, event_json, issue_json};
    use serde_json::json;

    const DAY: u64 = 86_400;
    /// 2026-07-01T10:00:00Z.
    const NOW: u64 = 1_782_900_000;

    fn store() -> (SyncStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = fjall::Database::builder(dir.path().join("db"))
            .open()
            .unwrap();
        (SyncStore::open(&db).unwrap(), dir)
    }

    fn ctx(fake: &Arc<FakeGitlab>, full: bool, last_ok: u64) -> FetchCtx {
        FetchCtx {
            gitlab: Arc::clone(fake) as Arc<dyn GitlabApi>,
            full,
            state: JobState {
                last_ok,
                ..Default::default()
            },
            started: NOW,
            windows: Windows {
                quick: DAY,
                retention: 90 * DAY,
                tracked: 30 * DAY,
                project_cap: 2,
            },
        }
    }

    async fn run(store: &SyncStore, job: Job, ctx: FetchCtx) -> usize {
        let staged = fetch(job, ctx).await.unwrap();
        let mut c = store.begin();
        let rows = staged.apply(&mut c).unwrap();
        c.commit().unwrap();
        rows
    }

    #[tokio::test]
    async fn view_job_stores_rows_and_their_order() {
        let (s, _d) = store();
        let fake = Arc::new(FakeGitlab::default());
        fake.serve(
            "issues",
            vec![
                issue_json(2, 5, "b"),
                issue_json(1, 9, "a"),
                json!({"id": 0}),
            ],
        );
        assert_eq!(run(&s, Job::AssignedIssues, ctx(&fake, true, 0)).await, 2);

        let view = s.view(ASSIGNED_ISSUES).unwrap().unwrap();
        assert_eq!(
            view.keys,
            [(2, 5), (1, 9)],
            "GitLab's order, malformed row skipped"
        );
        assert_eq!(view.fetched_at, NOW);
        assert_eq!(s.issues.get((1, 9)).unwrap().unwrap().title, "a");
    }

    #[tokio::test]
    async fn project_job_deltas_upsert_and_full_runs_reconcile() {
        let (s, _d) = store();
        let fake = Arc::new(FakeGitlab::default());
        fake.serve(
            "projects/7/issues",
            vec![issue_json(7, 1, "one"), issue_json(7, 2, "two")],
        );
        run(&s, Job::ProjectIssues(7), ctx(&fake, true, 0)).await;

        // A delta that sees only #1 must not drop #2.
        fake.serve("projects/7/issues", vec![issue_json(7, 1, "one v2")]);
        run(&s, Job::ProjectIssues(7), ctx(&fake, false, NOW - 3600)).await;
        assert_eq!(s.issues.keys(RowScope::All).unwrap(), [(7, 1), (7, 2)]);
        assert_eq!(s.issues.get((7, 1)).unwrap().unwrap().title, "one v2");

        // A full run that no longer sees #2 drops it.
        run(&s, Job::ProjectIssues(7), ctx(&fake, true, NOW - 3600)).await;
        assert_eq!(s.issues.keys(RowScope::All).unwrap(), [(7, 1)]);

        let calls = fake.calls_to("projects/7/issues");
        assert_eq!(
            calls[1],
            Listing::ProjectIssues {
                project_id: 7,
                updated_after: chrono::DateTime::from_timestamp((NOW - 3600 - 300) as i64, 0),
            },
            "the delta cursor overlaps the previous run"
        );
        assert!(matches!(
            calls[2],
            Listing::ProjectIssues {
                updated_after: None,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn a_large_project_keeps_only_its_newest_items() {
        let (s, _d) = store();
        let fake = Arc::new(FakeGitlab::default());
        // GitLab returns newest first; the test cap is 2.
        fake.serve(
            "projects/7/issues",
            vec![
                issue_json(7, 3, "new"),
                issue_json(7, 2, "mid"),
                issue_json(7, 1, "old"),
            ],
        );
        run(&s, Job::ProjectIssues(7), ctx(&fake, true, 0)).await;
        assert_eq!(s.issues.keys(RowScope::All).unwrap(), [(7, 2), (7, 3)]);
        assert_eq!(fake.limits_to("projects/7/issues"), [Some(2)]);
    }

    #[tokio::test]
    async fn boards_get_their_project_stamped() {
        let (s, _d) = store();
        let fake = Arc::new(FakeGitlab::default());
        fake.serve(
            "projects/7/boards",
            vec![json!({"id": 3, "lists": [{"label": {"name": "Doing"}}]})],
        );
        run(&s, Job::ProjectBoards(7), ctx(&fake, true, 0)).await;
        let boards = s.boards.scan(RowScope::Prefix(7)).unwrap();
        assert_eq!(boards.len(), 1);
        assert_eq!(boards[0].labels().collect::<Vec<_>>(), ["Doing"]);
    }

    #[tokio::test]
    async fn events_fetch_the_window_then_deltas_and_prune_old_rows() {
        let (s, _d) = store();
        let fake = Arc::new(FakeGitlab::default());
        fake.serve(
            "events",
            vec![
                event_json(1, 7, "pushed to", NOW - DAY),
                event_json(2, 8, "opened", NOW - 40 * DAY),
            ],
        );
        run(&s, Job::Events, ctx(&fake, true, 0)).await;
        let kept: Vec<i64> = s
            .events
            .scan(RowScope::All)
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(kept, [1], "outside the 30-day window");

        run(&s, Job::Events, ctx(&fake, false, NOW - 3600)).await;
        let day = |secs: u64| {
            chrono::DateTime::from_timestamp(secs as i64, 0)
                .unwrap()
                .date_naive()
                .pred_opt()
        };
        assert_eq!(
            fake.calls_to("events"),
            [
                Listing::Events {
                    after: day(NOW - 30 * DAY)
                },
                Listing::Events {
                    after: day(NOW - 3600)
                },
            ],
            "`after` is exclusive, so each fetch starts the day before"
        );
    }

    #[tokio::test]
    async fn timelog_jobs_reconcile_their_window() {
        let (s, _d) = store();
        let fake = Arc::new(FakeGitlab::default());
        let log = |id, spent_at| Timelog {
            id,
            spent_at,
            iid: 1,
            project_id: 7,
            ..Default::default()
        };
        fake.serve_timelogs(vec![
            log(1, NOW - 100),
            log(2, NOW - 2 * DAY),
            log(3, NOW - 100 * DAY),
        ]);
        run(&s, Job::AllTimelogs, ctx(&fake, true, 0)).await;
        let ids = |s: &SyncStore| -> Vec<u64> {
            s.timelogs
                .scan(RowScope::All)
                .unwrap()
                .into_iter()
                .map(|t| t.id)
                .collect()
        };
        assert_eq!(
            ids(&s),
            [2, 1],
            "beyond retention is dropped; key order is spent_at"
        );

        // #1 was deleted in GitLab: the recent window drops it, the older #2
        // is outside that window and stays.
        fake.serve_timelogs(vec![log(2, NOW - 2 * DAY)]);
        run(&s, Job::RecentTimelogs, ctx(&fake, true, 0)).await;
        assert_eq!(ids(&s), [2]);
        assert_eq!(
            fake.timelog_calls()[1],
            chrono::DateTime::from_timestamp((NOW - DAY) as i64, 0).unwrap()
        );
    }

    #[test]
    fn writes_rerun_the_jobs_that_show_them() {
        let w = |kind, op| Write {
            kind,
            project_id: 7,
            iid: 1,
            op,
        };
        assert_eq!(
            Job::affected_by(&w(Issuable::Issue, WriteOp::Close)),
            [Job::AssignedIssues, Job::ProjectIssues(7)]
        );
        assert_eq!(
            Job::affected_by(&w(Issuable::MergeRequest, WriteOp::AssignSelf)),
            [Job::AssignedMergeRequests, Job::ProjectMergeRequests(7)]
        );
        let post = WriteOp::PostTime {
            duration: "1h".into(),
            summary: None,
            issuable_id: None,
        };
        assert_eq!(
            Job::affected_by(&w(Issuable::Issue, post)),
            [Job::RecentTimelogs]
        );
    }
}
