//! The sync jobs: what each one fetches, how often, and how its rows land in
//! the store.
//!
//! A job runs in two phases. [`fetch`] talks to GitLab and writes nothing;
//! it returns a [`Staged`] commit the worker applies in one batch together
//! with the job's new state. Dropping a fetch mid-flight therefore never
//! leaves partial data behind. An avatar's file is written in that second
//! phase too, right before its row.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tracing::{info, warn};

use super::avatars::{self, Avatar, AvatarDir};
use super::model::{
    Board, Epic, Event, Group, Issue, MergeRequest, Project, Resource, RowKey, Timelog,
};
use super::schedule::{Cadence, JobState, UNAVAILABLE_AFTER, fingerprint};
use super::store::{Commit, RowScope, Stored, View};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::gitlab::{GitlabApi, Issuable, Listing, Progress};
use crate::write::{Write, WriteOp};

/// View of the open issues assigned to the user.
pub const ASSIGNED_ISSUES: &str = "assigned/issues";
/// View of the open merge requests assigned to the user.
pub const ASSIGNED_MERGE_REQUESTS: &str = "assigned/merge_requests";
/// View of the issues the user authored, open or closed, updated inside
/// `search.tracked_retention_hours`.
pub const RECENT_AUTHORED_ISSUES: &str = "recent/authored/issues";
/// View of the issues assigned to the user, open or closed, updated inside
/// `search.tracked_retention_hours`.
pub const RECENT_ASSIGNED_ISSUES: &str = "recent/assigned/issues";
/// Every view listing issues. A row one of them names is that view's to
/// keep, whatever the others or a project's cap say.
pub const ISSUE_VIEWS: [&str; 3] = [
    ASSIGNED_ISSUES,
    RECENT_AUTHORED_ISSUES,
    RECENT_ASSIGNED_ISSUES,
];

/// How far a delta's `updated_after` cursor reaches back before the previous
/// run started, so items updated during that run (or under clock skew) are
/// fetched again instead of missed. Upserts dedupe the overlap.
const DELTA_OVERLAP_SECS: u64 = 300;

/// What two fetches in flight must not share: the worker runs one job per
/// lane at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lane {
    /// Everything read from one project.
    Project(i64),
    /// A group's epics.
    Group(i64),
    /// The cross-project issue listings.
    Issues,
    /// The cross-project merge request listings.
    MergeRequests,
    /// Both timelog windows: the recent one lies inside the full one.
    Timelogs,
    /// A job sharing its rows with no other.
    Own(Job),
}

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
    /// A member group's own epics. Only instances with epics (GitLab
    /// Premium) answer it.
    GroupEpics(i64),
    AllIssues,
    AllMergeRequests,
    /// The issues the user authored, any state, updated inside
    /// `search.tracked_retention_hours`.
    RecentAuthoredIssues,
    /// The same for the issues assigned to the user. Not the assigned list
    /// with another filter: that one is bounded by state and not by time,
    /// runs every few minutes and feeds the plan.
    RecentAssignedIssues,
    /// Timelogs inside `history.retention_hours`; prunes older ones.
    AllTimelogs,
    /// A member project's avatar, as a file for the launchers.
    ProjectAvatar(i64),
}

impl Job {
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
            Self::GroupEpics(g) => format!("group/{g}/epics"),
            Self::AllIssues => "all/issues".into(),
            Self::AllMergeRequests => "all/merge_requests".into(),
            Self::RecentAuthoredIssues => RECENT_AUTHORED_ISSUES.into(),
            Self::RecentAssignedIssues => RECENT_ASSIGNED_ISSUES.into(),
            Self::AllTimelogs => "timelogs/all".into(),
            Self::ProjectAvatar(p) => format!("project/{p}/avatar"),
        }
    }

    /// Lower runs first when several jobs are due.
    pub fn priority(&self) -> u8 {
        match self {
            Self::AssignedIssues | Self::AssignedMergeRequests | Self::RecentTimelogs => 0,
            Self::Events => 1,
            Self::AllTimelogs => 3,
            // Decoration: never ahead of data.
            Self::ProjectAvatar(_) => 4,
            _ => 2,
        }
    }

    /// The lane the job runs in. Jobs in different lanes write disjoint
    /// rows, except a list and a project corpus: both store the project's
    /// listed items, and [`drop_outdated`] keeps the newer one.
    pub fn lane(&self) -> Lane {
        match *self {
            Self::ProjectBoards(p)
            | Self::ProjectIssues(p)
            | Self::ProjectMergeRequests(p)
            | Self::ProjectAvatar(p) => Lane::Project(p),
            Self::GroupEpics(g) => Lane::Group(g),
            // A full `all/*` run reconciles every row, so a list landing
            // mid-fetch would lose what it just stored. And two lists store
            // the same rows with nothing to tell the newer one.
            Self::AssignedIssues
            | Self::AllIssues
            | Self::RecentAuthoredIssues
            | Self::RecentAssignedIssues => Lane::Issues,
            Self::AssignedMergeRequests | Self::AllMergeRequests => Lane::MergeRequests,
            Self::RecentTimelogs | Self::AllTimelogs => Lane::Timelogs,
            Self::Events | Self::MemberProjects | Self::MemberGroups => Lane::Own(*self),
        }
    }

    /// The view the job fills, if it is a view job.
    pub fn view(&self) -> Option<&'static str> {
        match self {
            Self::AssignedIssues => Some(ASSIGNED_ISSUES),
            Self::AssignedMergeRequests => Some(ASSIGNED_MERGE_REQUESTS),
            Self::RecentAuthoredIssues => Some(RECENT_AUTHORED_ISSUES),
            Self::RecentAssignedIssues => Some(RECENT_ASSIGNED_ISSUES),
            _ => None,
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
                | Self::MemberGroups
        )
    }

    /// How many refusals in a row (see [`Self::refused_by`]) make GitLab's
    /// answer final: the job is then unavailable, rests for a day at a time
    /// and fails quietly. `None` for the account-wide listings: GitLab
    /// refusing one of them means something is wrong with the session, which
    /// must stay loud.
    pub fn unavailable_after(&self) -> Option<u32> {
        match self {
            // An instance without epics (no Premium) refuses every group
            // alike: the first refusal says it all.
            Self::GroupEpics(_) => Some(1),
            // A feature switched off in one project, or one the account may
            // not see there.
            Self::ProjectIssues(_)
            | Self::ProjectMergeRequests(_)
            | Self::ProjectBoards(_)
            | Self::ProjectAvatar(_) => Some(UNAVAILABLE_AFTER),
            _ => None,
        }
    }

    /// Whether `e` is GitLab refusing the job's listing: a 403 or a 404 for
    /// a job that can be unavailable at all. The epics, which an instance
    /// may lack altogether, count any rejection, as they always did.
    pub fn refused_by(&self, e: &Error) -> bool {
        match e {
            Error::Rejected { .. } | Error::Gitlab(_) if matches!(self, Self::GroupEpics(_)) => {
                true
            }
            Error::Rejected {
                status: 403 | 404, ..
            } => self.unavailable_after().is_some(),
            _ => false,
        }
    }

    /// Whether the job in `state` is unavailable: GitLab refused it
    /// [`Self::unavailable_after`] times in a row.
    pub fn unavailable(&self, state: &JobState) -> bool {
        self.unavailable_after()
            .is_some_and(|after| state.rejections >= after)
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
            // The recent issue lists are a look back, where a day of lag
            // is fine.
            Self::ProjectBoards(_)
            | Self::AllTimelogs
            | Self::MemberProjects
            | Self::MemberGroups
            | Self::RecentAuthoredIssues
            | Self::RecentAssignedIssues => full_only(c.refresh.slow.interval_secs),
            Self::ProjectIssues(_)
            | Self::ProjectMergeRequests(_)
            | Self::GroupEpics(_)
            | Self::AllIssues
            | Self::AllMergeRequests => Cadence {
                every: c.search.partial_interval_secs,
                full_every: Some(c.search.full_interval_secs),
            },
            // Events never change once created, but GitLab's answer does: a
            // row hidden at walk time, an import landing with an old
            // `created_at`. The full cadence re-walks the window (a few
            // pages) so nothing stays missed.
            Self::Events => Cadence {
                every: c.search.partial_interval_secs,
                full_every: Some(c.search.full_interval_secs),
            },
            // Once per avatar: only a changed fingerprint (its URL) makes
            // the job due again.
            Self::ProjectAvatar(_) => full_only(u64::MAX),
        }
    }

    /// What a success is valid for; a change (schema bump, wider window)
    /// makes the job due at once and full. The worker goes through
    /// [`Plan::fingerprint`](super::planner::Plan::fingerprint), which adds
    /// what the store tells.
    pub fn fingerprint(&self, c: &Config) -> u64 {
        let schema = u64::from(match self {
            Self::AssignedIssues
            | Self::ProjectIssues(_)
            | Self::AllIssues
            | Self::RecentAuthoredIssues
            | Self::RecentAssignedIssues => Issue::SCHEMA,
            Self::AssignedMergeRequests
            | Self::ProjectMergeRequests(_)
            | Self::AllMergeRequests => MergeRequest::SCHEMA,
            Self::RecentTimelogs | Self::AllTimelogs => Timelog::SCHEMA,
            Self::Events => Event::SCHEMA,
            Self::ProjectBoards(_) => Board::SCHEMA,
            Self::MemberProjects => Project::SCHEMA,
            Self::MemberGroups => Group::SCHEMA,
            Self::GroupEpics(_) => Epic::SCHEMA,
            Self::ProjectAvatar(_) => Avatar::SCHEMA,
        });
        match self {
            Self::ProjectIssues(_) | Self::ProjectMergeRequests(_) | Self::GroupEpics(_) => {
                fingerprint(&[schema, c.search.max_items_per_project])
            }
            Self::RecentTimelogs => fingerprint(&[schema, c.refresh.quick.window_hours]),
            Self::AllTimelogs => fingerprint(&[schema, c.history.retention_hours]),
            Self::Events | Self::RecentAuthoredIssues | Self::RecentAssignedIssues => {
                fingerprint(&[schema, c.search.tracked_retention_hours])
            }
            _ => fingerprint(&[schema]),
        }
    }

    /// The jobs to rerun so a write shows up, whichever of them are planned.
    pub fn affected_by(write: &Write) -> Vec<Job> {
        let pid = write.project_id;
        match (&write.op, write.kind) {
            (WriteOp::PostTime { .. }, _) => vec![Self::RecentTimelogs],
            (_, Issuable::Issue) => Self::showing_issues_of(pid).to_vec(),
            (_, Issuable::MergeRequest) => {
                vec![Self::AssignedMergeRequests, Self::ProjectMergeRequests(pid)]
            }
        }
    }

    /// The jobs displaying an issue of `project_id`, to rerun once one was
    /// written or created. The recent lists too: without a corpus nothing
    /// else shows a closed issue as closed before their next daily run.
    pub fn showing_issues_of(project_id: i64) -> [Job; 4] {
        [
            Self::AssignedIssues,
            Self::ProjectIssues(project_id),
            Self::RecentAuthoredIssues,
            Self::RecentAssignedIssues,
        ]
    }

    /// Like [`Self::affected_by`], for a write replayed from the queue: a
    /// PostTime is booked at `queued_at`, and one older than the recent
    /// window only shows in the full history.
    pub fn affected_by_replay(write: &Write, queued_at: u64, c: &Config, now: u64) -> Vec<Job> {
        let mut jobs = Self::affected_by(write);
        let recent_since = now.saturating_sub(c.refresh.quick.window().as_secs());
        if matches!(write.op, WriteOp::PostTime { .. }) && queued_at < recent_since {
            jobs.push(Self::AllTimelogs);
        }
        jobs
    }
}

/// The windows and limits a run needs, snapshotted from the config.
#[derive(Debug, Clone, Copy)]
pub struct Windows {
    pub quick: u64,
    pub retention: u64,
    pub tracked: u64,
    /// Most issues (and most MRs) fetched per project, and most epics per
    /// group.
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
    pub avatars: AvatarDir,
    /// How far the run is, for the worker to report while it is in flight.
    pub progress: Arc<Progress>,
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
    match job {
        Job::AssignedIssues => {
            let listing = Listing::AssignedIssues;
            view::<Issue>(&ctx, listing, ASSIGNED_ISSUES, Job::ProjectIssues).await
        }
        Job::AssignedMergeRequests => {
            let (listing, name) = (Listing::AssignedMergeRequests, ASSIGNED_MERGE_REQUESTS);
            view::<MergeRequest>(&ctx, listing, name, Job::ProjectMergeRequests).await
        }
        Job::ProjectIssues(project_id) => {
            let listing = |updated_after| Listing::ProjectIssues {
                project_id,
                updated_after,
            };
            capped_rows::<Issue>(&ctx, listing, project_id, &ISSUE_VIEWS).await
        }
        Job::ProjectMergeRequests(project_id) => {
            let listing = |updated_after| Listing::ProjectMergeRequests {
                project_id,
                updated_after,
            };
            let views = &[ASSIGNED_MERGE_REQUESTS];
            capped_rows::<MergeRequest>(&ctx, listing, project_id, views).await
        }
        Job::GroupEpics(group_id) => {
            let listing = |updated_after| Listing::GroupEpics {
                group_id,
                updated_after,
            };
            capped_rows::<Epic>(&ctx, listing, group_id, &[]).await
        }
        Job::AllIssues => {
            let listing = Listing::AllIssues {
                updated_after: ctx.updated_after(),
            };
            rows::<Issue>(&ctx, listing, ctx.full.then_some(RowScope::All)).await
        }
        Job::AllMergeRequests => {
            let listing = Listing::AllMergeRequests {
                updated_after: ctx.updated_after(),
            };
            rows::<MergeRequest>(&ctx, listing, ctx.full.then_some(RowScope::All)).await
        }
        Job::RecentAuthoredIssues => {
            let listing = |updated_after| Listing::RecentAuthoredIssues { updated_after };
            recent_issues(&ctx, listing, RECENT_AUTHORED_ISSUES).await
        }
        Job::RecentAssignedIssues => {
            let listing = |updated_after| Listing::RecentAssignedIssues { updated_after };
            recent_issues(&ctx, listing, RECENT_ASSIGNED_ISSUES).await
        }
        Job::MemberProjects => {
            rows::<Project>(&ctx, Listing::MemberProjects, Some(RowScope::All)).await
        }
        Job::MemberGroups => rows::<Group>(&ctx, Listing::MemberGroups, Some(RowScope::All)).await,
        Job::ProjectBoards(project_id) => {
            let fetched = fetch_rows(
                &ctx,
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
        Job::ProjectAvatar(project_id) => avatar(&ctx, project_id).await,
    }
}

/// A project's avatar as a local file, named by its bytes: the job runs
/// whenever GitLab's avatar URL changes, which its `?v=<updated_at>` does on
/// any update of the project, but only a new image gets a new path. A row
/// is stored either way: without a file when GitLab has no image a launcher
/// could show, so the job rests until the URL changes.
async fn avatar(ctx: &FetchCtx, project_id: i64) -> Result<Staged> {
    let image = ctx
        .gitlab
        .project_avatar(project_id)
        .await?
        .and_then(|bytes| {
            let file = avatars::file_name(project_id, &bytes);
            if file.is_none() {
                warn!(
                    project_id,
                    bytes = bytes.len(),
                    "skipping a project avatar: too large or not a known image format"
                );
            }
            Some((file?, bytes))
        });
    let dir = ctx.avatars.clone();
    Ok(Staged::new(move |c| {
        let file = match image {
            Some((file, bytes)) => {
                dir.write(&file, &bytes)?;
                file
            }
            None => String::new(),
        };
        c.upsert(&[Avatar { project_id, file }])?;
        Ok(1)
    }))
}

/// The issues or MRs of one project, or the epics of one group (`owner`),
/// newest first and capped: a huge one keeps only its most recently updated
/// items, and a full run's reconcile drops the rest, except for items one
/// of `views` lists.
async fn capped_rows<R: Stored + Dated>(
    ctx: &FetchCtx,
    listing: impl Fn(Option<chrono::DateTime<chrono::Utc>>) -> Listing,
    owner: i64,
    views: &'static [&'static str],
) -> Result<Staged> {
    let cap = ctx.windows.project_cap;
    let mut fetched: Vec<R> =
        fetch_rows(ctx, &listing(ctx.updated_after()), Some(cap), |_| {}).await?;
    // A delta that fills the cap holds the newest `cap` items, just like a
    // full run, so it can reconcile too.
    let whole = ctx.full || fetched.len() >= cap;
    if fetched.len() >= cap {
        info!(
            owner,
            kind = R::NAME,
            cap,
            "more items than search.max_items_per_project; keeping the most recently updated"
        );
    }
    if whole {
        // Offset pages over `updated_at` skip an item updated mid-walk: it
        // jumps to a page already read. A delta from the walk's start finds
        // it before the reconcile would drop it.
        let since = ctx.started.saturating_sub(DELTA_OVERLAP_SECS);
        let late: Vec<R> = fetch_rows(
            ctx,
            &listing(chrono::DateTime::from_timestamp(since as i64, 0)),
            Some(cap),
            |_| {},
        )
        .await?;
        let seen: HashSet<RowKey> = late.iter().map(Resource::key).collect();
        fetched.retain(|r| !seen.contains(&r.key()));
        fetched.extend(late);
    }
    let prefix = owner.max(0) as u64;
    let started = ctx.started;
    Ok(Staged::new(move |c| {
        let rows = fetched.len();
        let mut keep: HashSet<RowKey> = fetched.iter().map(Resource::key).collect();
        for view in views {
            let listed = c.view(view)?.unwrap_or_default();
            let named: HashSet<RowKey> =
                listed.keys.into_iter().filter(|k| k.0 == prefix).collect();
            // The list was fetched after this run started and landed first.
            if listed.fetched_at >= started {
                drop_outdated(c, &mut fetched, |k| Ok(named.contains(&k)))?;
            }
            // A listed item older than the cap is still on the list.
            keep.extend(named);
        }
        c.upsert(&fetched)?;
        if whole {
            c.remove_where::<R>(RowScope::Prefix(prefix), |k| keep.contains(&k))?;
        }
        Ok(rows)
    }))
}

/// A row with GitLab's `updated_at`, to tell two fetched versions apart.
trait Dated {
    fn updated_at(&self) -> u64;
}

macro_rules! dated {
    ($($ty:ty),*) => {$(
        impl Dated for $ty {
            fn updated_at(&self) -> u64 {
                self.updated_at
            }
        }
    )*};
}

dated!(Issue, MergeRequest, Epic);

/// Take out of `fetched` the rows stored in a newer version, among those
/// `raced` names: the ones a fetch that started later may have stored while
/// this one was in flight. Commits land in completion order, so without
/// this the older read would win.
fn drop_outdated<R: Stored + Dated>(
    c: &Commit<'_>,
    fetched: &mut Vec<R>,
    mut raced: impl FnMut(RowKey) -> Result<bool>,
) -> Result<()> {
    let mut current = Vec::with_capacity(fetched.len());
    for row in fetched.drain(..) {
        let key = row.key();
        let outdated = raced(key)?
            && c.get::<R>(key)?
                .is_some_and(|stored| stored.updated_at() > row.updated_at());
        if !outdated {
            current.push(row);
        }
    }
    *fetched = current;
    Ok(())
}

/// Fetch `listing` as `R` rows; a full run (`reconcile = Some`) also drops
/// stored rows in that scope the listing no longer returns.
async fn rows<R: Stored>(
    ctx: &FetchCtx,
    listing: Listing,
    reconcile: Option<RowScope>,
) -> Result<Staged> {
    let fetched: Vec<R> = fetch_rows(ctx, &listing, None, |_| {}).await?;
    Ok(Staged::new(move |c| {
        c.upsert(&fetched)?;
        if let Some(scope) = reconcile {
            c.reconcile(scope, &fetched)?;
        }
        Ok(fetched.len())
    }))
}

/// Fetch `listing` into the rows plus the view `name` listing their keys.
/// `corpus` is the job that stores a project's rows of the same kind.
async fn view<R: Stored + Dated>(
    ctx: &FetchCtx,
    listing: Listing,
    name: &'static str,
    corpus: fn(i64) -> Job,
) -> Result<Staged> {
    let mut fetched: Vec<R> = fetch_rows(ctx, &listing, None, |_| {}).await?;
    let started = ctx.started;
    Ok(Staged::new(move |c| {
        let keys: Vec<RowKey> = fetched.iter().map(Resource::key).collect();
        // Projects whose corpus run started after this fetch and landed
        // first.
        let mut later: HashMap<u64, bool> = HashMap::new();
        drop_outdated(c, &mut fetched, |key| {
            if let Some(&raced) = later.get(&key.0) {
                return Ok(raced);
            }
            let raced = c.job_state(&corpus(key.0 as i64).key())?.last_ok >= started;
            later.insert(key.0, raced);
            Ok(raced)
        })?;
        c.upsert(&fetched)?;
        let rows = keys.len();
        c.set_view(
            name,
            &View {
                keys,
                fetched_at: started,
            },
        )?;
        Ok(rows)
    }))
}

/// The user's issues of one role in any state, updated inside the tracked
/// window, as the view `name`. Every run fetches the whole window and
/// replaces the view, so an issue that aged out of the window leaves it.
async fn recent_issues(
    ctx: &FetchCtx,
    listing: fn(chrono::DateTime<chrono::Utc>) -> Listing,
    name: &'static str,
) -> Result<Staged> {
    let since = ctx.started.saturating_sub(ctx.windows.tracked);
    let since = chrono::DateTime::from_timestamp(since as i64, 0).unwrap_or_default();
    view::<Issue>(ctx, listing(since), name, Job::ProjectIssues).await
}

/// Events inside the tracked window: the whole window on a full run, else
/// since the last run. Older rows are pruned.
async fn events(ctx: &FetchCtx) -> Result<Staged> {
    let window_start = ctx.started.saturating_sub(ctx.windows.tracked);
    let from = if ctx.full {
        window_start
    } else {
        ctx.state.last_ok.max(window_start)
    };
    // `after` is an exclusive date in the instance's time zone: two days
    // back cover any zone, and the upserts dedupe the overlap.
    let after = chrono::DateTime::from_timestamp(from.saturating_sub(86_400) as i64, 0)
        .unwrap_or_default()
        .date_naive()
        .pred_opt();
    let fetched: Vec<Event> = fetch_rows(ctx, &Listing::Events { after }, None, |_: &mut Event| {})
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
    let (fetched, unreadable): (Vec<Timelog>, Vec<Timelog>) = ctx
        .gitlab
        .list_timelogs(since_dt, &ctx.progress)
        .await?
        .into_iter()
        .filter(|t| t.id > 0 && t.spent_at >= since)
        .partition(Resource::is_valid);
    // Time logged on an issue the user can no longer read still counts:
    // keep what was stored for it.
    let keep: HashSet<RowKey> = fetched
        .iter()
        .chain(&unreadable)
        .map(Resource::key)
        .collect();
    Ok(Staged::new(move |c| {
        c.upsert(&fetched)?;
        c.remove_where::<Timelog>(RowScope::Since(since), |k| keep.contains(&k))?;
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
    ctx: &FetchCtx,
    listing: &Listing,
    limit: Option<usize>,
    stamp: impl Fn(&mut R),
) -> Result<Vec<R>> {
    let raw = ctx.gitlab.list(listing, limit, &ctx.progress).await?;
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
    use crate::testing::{
        FakeErr, FakeGitlab, PNG, RECENT_ASSIGNED_PATH, RECENT_AUTHORED_PATH, epic_json,
        event_json, issue_json,
    };
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
        ctx_in(fake, full, last_ok, std::env::temp_dir())
    }

    /// [`ctx`] writing avatars into `avatars`.
    fn ctx_in(
        fake: &Arc<FakeGitlab>,
        full: bool,
        last_ok: u64,
        avatars: impl Into<std::path::PathBuf>,
    ) -> FetchCtx {
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
            avatars: AvatarDir::new(avatars),
            progress: Default::default(),
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

    /// A recent list asks for the whole tracked window on every run, in any
    /// state, and the view is what that run returned.
    #[tokio::test]
    async fn a_recent_list_fetches_its_window_and_replaces_the_view() {
        let (s, _d) = store();
        let fake = Arc::new(FakeGitlab::default());
        let mut closed = issue_json(8, 4, "done");
        closed["state"] = json!("closed");
        fake.serve(
            RECENT_AUTHORED_PATH,
            vec![issue_json(7, 1, "one"), closed.clone()],
        );
        let job = Job::RecentAuthoredIssues;
        assert_eq!(run(&s, job, ctx(&fake, true, 0)).await, 2);

        let view = s.view(RECENT_AUTHORED_ISSUES).unwrap().unwrap();
        assert_eq!(view.keys, [(7, 1), (8, 4)]);
        assert_eq!(view.fetched_at, NOW);
        assert_eq!(s.issues.get((8, 4)).unwrap().unwrap().state, "closed");

        // #1 aged out of the window. No delta: the cursor stays the window's.
        fake.serve(RECENT_AUTHORED_PATH, vec![closed]);
        run(&s, job, ctx(&fake, false, NOW - 3600)).await;
        let view = s.view(RECENT_AUTHORED_ISSUES).unwrap().unwrap();
        assert_eq!(view.keys, [(8, 4)]);
        let updated_after = chrono::DateTime::from_timestamp((NOW - 30 * DAY) as i64, 0).unwrap();
        assert_eq!(
            fake.calls_to(RECENT_AUTHORED_PATH),
            [
                Listing::RecentAuthoredIssues { updated_after },
                Listing::RecentAuthoredIssues { updated_after },
            ]
        );

        // The other role is its own listing and its own view.
        run(&s, Job::RecentAssignedIssues, ctx(&fake, true, 0)).await;
        assert_eq!(
            fake.calls_to(RECENT_ASSIGNED_PATH),
            [Listing::RecentAssignedIssues { updated_after }]
        );
        let assigned = s.view(RECENT_ASSIGNED_ISSUES).unwrap().unwrap();
        assert!(assigned.keys.is_empty());
        assert!(s.view(ASSIGNED_ISSUES).unwrap().is_none());
    }

    /// A day of lag is fine for a look back, and a wider window is a
    /// different list.
    #[test]
    fn recent_lists_run_daily_and_follow_the_tracked_window() {
        let cfg = crate::config::defaults();
        let mut wider = crate::config::defaults();
        wider.search.tracked_retention_hours *= 2;
        for job in [Job::RecentAuthoredIssues, Job::RecentAssignedIssues] {
            let cadence = job.cadence(&cfg);
            assert_eq!(cadence.every, cfg.refresh.slow.interval_secs);
            assert_eq!(cadence.full_every, None);
            assert_ne!(job.fingerprint(&cfg), job.fingerprint(&wider));
            assert!(!job.feeds_plan());
        }
    }

    /// GitLab's answer to `/events` changes though events don't, so the
    /// window is re-walked on the full cadence; a wider window is a new walk.
    #[test]
    fn events_rewalk_the_window_on_the_full_cadence() {
        let cfg = crate::config::defaults();
        let mut wider = crate::config::defaults();
        wider.search.tracked_retention_hours *= 2;
        let cadence = Job::Events.cadence(&cfg);
        assert_eq!(cadence.every, cfg.search.partial_interval_secs);
        assert_eq!(cadence.full_every, Some(cfg.search.full_interval_secs));
        assert_ne!(
            Job::Events.fingerprint(&cfg),
            Job::Events.fingerprint(&wider)
        );
    }

    /// Issue #1 of project 7 as GitLab showed it at `hour` o'clock.
    fn issue_at(hour: u32, title: &str) -> serde_json::Value {
        let mut issue = issue_json(7, 1, title);
        issue["updated_at"] = json!(format!("2026-07-01T{hour:02}:00:00Z"));
        issue
    }

    /// Commit `staged` as the worker does, with the job's new state.
    fn land(s: &SyncStore, job: Job, staged: Staged, started: u64) {
        let mut c = s.begin();
        staged.apply(&mut c).unwrap();
        let state = JobState {
            last_ok: started,
            ..Default::default()
        };
        c.set_job(&job.key(), &state).unwrap();
        c.commit().unwrap();
    }

    fn title(s: &SyncStore) -> String {
        s.issues.get((7, 1)).unwrap().unwrap().title
    }

    /// Fetches land in the order they finish. A corpus run that started
    /// before the list but lands after it must not bring the old row back.
    #[tokio::test]
    async fn a_corpus_run_landing_after_a_later_list_keeps_the_newer_row() {
        let (s, _d) = store();
        let fake = Arc::new(FakeGitlab::default());
        fake.serve(
            "projects/7/issues",
            vec![issue_at(9, "stale"), issue_json(7, 2, "two")],
        );
        fake.serve("issues", vec![issue_at(11, "fresh")]);
        let corpus = fetch(Job::ProjectIssues(7), ctx(&fake, true, 0))
            .await
            .unwrap();
        let mut later = ctx(&fake, true, 0);
        later.started = NOW + 10;
        let list = fetch(Job::AssignedIssues, later).await.unwrap();

        land(&s, Job::AssignedIssues, list, NOW + 10);
        land(&s, Job::ProjectIssues(7), corpus, NOW);
        assert_eq!(title(&s), "fresh");
        assert_eq!(s.issues.keys(RowScope::All).unwrap(), [(7, 1), (7, 2)]);
    }

    /// The recent lists race a corpus run like the assigned one.
    #[tokio::test]
    async fn a_corpus_run_landing_after_a_later_recent_list_keeps_the_newer_row() {
        let (s, _d) = store();
        let fake = Arc::new(FakeGitlab::default());
        fake.serve(
            "projects/7/issues",
            vec![issue_at(9, "stale"), issue_json(7, 2, "two")],
        );
        fake.serve(RECENT_ASSIGNED_PATH, vec![issue_at(11, "fresh")]);
        let corpus = fetch(Job::ProjectIssues(7), ctx(&fake, true, 0))
            .await
            .unwrap();
        let mut later = ctx(&fake, true, 0);
        later.started = NOW + 10;
        let list = fetch(Job::RecentAssignedIssues, later).await.unwrap();

        land(&s, Job::RecentAssignedIssues, list, NOW + 10);
        land(&s, Job::ProjectIssues(7), corpus, NOW);
        assert_eq!(title(&s), "fresh");
        assert_eq!(s.issues.keys(RowScope::All).unwrap(), [(7, 1), (7, 2)]);

        // And the list that started first and lands last.
        fake.serve(RECENT_ASSIGNED_PATH, vec![issue_at(10, "older")]);
        let mut first = ctx(&fake, true, 0);
        first.started = NOW + 20;
        let list = fetch(Job::RecentAssignedIssues, first).await.unwrap();
        fake.serve("projects/7/issues", vec![issue_at(12, "newest")]);
        let mut later = ctx(&fake, true, 0);
        later.started = NOW + 30;
        let corpus = fetch(Job::ProjectIssues(7), later).await.unwrap();
        land(&s, Job::ProjectIssues(7), corpus, NOW + 30);
        land(&s, Job::RecentAssignedIssues, list, NOW + 20);
        assert_eq!(title(&s), "newest");
    }

    /// The other way round: the list started first and lands last.
    #[tokio::test]
    async fn a_list_landing_after_a_later_corpus_run_keeps_the_newer_row() {
        let (s, _d) = store();
        let fake = Arc::new(FakeGitlab::default());
        fake.serve("issues", vec![issue_at(9, "stale")]);
        fake.serve("projects/7/issues", vec![issue_at(11, "fresh")]);
        let list = fetch(Job::AssignedIssues, ctx(&fake, true, 0))
            .await
            .unwrap();
        let mut later = ctx(&fake, true, 0);
        later.started = NOW + 10;
        let corpus = fetch(Job::ProjectIssues(7), later).await.unwrap();

        land(&s, Job::ProjectIssues(7), corpus, NOW + 10);
        land(&s, Job::AssignedIssues, list, NOW);
        assert_eq!(title(&s), "fresh");
        let view = s.view(ASSIGNED_ISSUES).unwrap().unwrap();
        assert_eq!(view.keys, [(7, 1)], "the list itself still lands");
    }

    /// Without such a race the fetched row always wins, whatever its
    /// `updated_at`: GitLab's timestamps need not be monotonic.
    #[tokio::test]
    async fn fetches_in_sequence_overwrite_whatever_is_stored() {
        let (s, _d) = store();
        let fake = Arc::new(FakeGitlab::default());
        fake.serve("projects/7/issues", vec![issue_at(11, "corpus")]);
        fake.serve("issues", vec![issue_at(9, "list")]);
        let mut earlier = ctx(&fake, true, 0);
        earlier.started = NOW - 100;
        let corpus = fetch(Job::ProjectIssues(7), earlier).await.unwrap();
        land(&s, Job::ProjectIssues(7), corpus, NOW - 100);

        let list = fetch(Job::AssignedIssues, ctx(&fake, true, 0))
            .await
            .unwrap();
        land(&s, Job::AssignedIssues, list, NOW);
        assert_eq!(title(&s), "list");

        fake.serve("projects/7/issues", vec![issue_at(8, "corpus again")]);
        let mut after = ctx(&fake, true, 0);
        after.started = NOW + 100;
        let corpus = fetch(Job::ProjectIssues(7), after).await.unwrap();
        land(&s, Job::ProjectIssues(7), corpus, NOW + 100);
        assert_eq!(title(&s), "corpus again");
    }

    #[test]
    fn jobs_reading_the_same_rows_share_a_lane() {
        let project = [
            Job::ProjectBoards(7),
            Job::ProjectIssues(7),
            Job::ProjectMergeRequests(7),
            Job::ProjectAvatar(7),
        ];
        assert!(project.iter().all(|j| j.lane() == Lane::Project(7)));
        assert_ne!(Job::ProjectIssues(8).lane(), Lane::Project(7));
        assert_eq!(Job::RecentTimelogs.lane(), Job::AllTimelogs.lane());
        assert_eq!(Job::AssignedIssues.lane(), Job::AllIssues.lane());
        // Two lists store the same rows, and nothing tells the newer one.
        for recent in [Job::RecentAuthoredIssues, Job::RecentAssignedIssues] {
            assert_eq!(recent.lane(), Job::AssignedIssues.lane());
            assert_ne!(recent.lane(), Job::ProjectIssues(7).lane());
        }
        assert_eq!(
            Job::AssignedMergeRequests.lane(),
            Job::AllMergeRequests.lane()
        );
        // The lists overlap with a project's corpus only in rows, which
        // `drop_outdated` settles: they don't wait for each other.
        assert_ne!(Job::AssignedIssues.lane(), Job::ProjectIssues(7).lane());
        let alone = [Job::Events, Job::MemberProjects, Job::MemberGroups];
        assert!(alone.iter().all(|j| j.lane() == Lane::Own(*j)));
        assert_eq!(Job::GroupEpics(3).lane(), Lane::Group(3));
    }

    /// Only the per-project and per-group listings can be unavailable, and
    /// only a 403 or 404 refuses them; the epics any rejection, from the
    /// first one.
    #[test]
    fn which_jobs_can_be_unavailable_and_what_refuses_them() {
        let refused = |status| FakeErr::RejectedWith(status).error();
        let per_project = [
            Job::ProjectIssues(7),
            Job::ProjectMergeRequests(7),
            Job::ProjectBoards(7),
            Job::ProjectAvatar(7),
        ];
        for job in per_project {
            assert_eq!(job.unavailable_after(), Some(3), "{job:?}");
            assert!(job.refused_by(&refused(403)), "{job:?}");
            assert!(job.refused_by(&refused(404)), "{job:?}");
            for other in [
                refused(400),
                refused(422),
                Error::Gitlab("unreadable page".into()),
                FakeErr::Transient.error(),
                FakeErr::Throttled(429).error(),
                FakeErr::Throttled(503).error(),
                FakeErr::Unauthorized.error(),
            ] {
                assert!(!job.refused_by(&other), "{job:?}: {other}");
            }
        }

        let epics = Job::GroupEpics(3);
        assert_eq!(epics.unavailable_after(), Some(1));
        for rejection in [
            refused(403),
            refused(404),
            refused(400),
            Error::Gitlab("x".into()),
        ] {
            assert!(epics.refused_by(&rejection), "{rejection}");
        }
        assert!(!epics.refused_by(&FakeErr::Throttled(503).error()));
        assert!(!epics.refused_by(&FakeErr::Transient.error()));

        let account_wide = [
            Job::AssignedIssues,
            Job::AssignedMergeRequests,
            Job::RecentTimelogs,
            Job::AllTimelogs,
            Job::Events,
            Job::MemberProjects,
            Job::MemberGroups,
            Job::AllIssues,
            Job::AllMergeRequests,
            Job::RecentAuthoredIssues,
            Job::RecentAssignedIssues,
        ];
        let forever = JobState {
            rejections: u32::MAX,
            ..JobState::default()
        };
        for job in account_wide {
            assert_eq!(job.unavailable_after(), None, "{job:?}");
            assert!(!job.refused_by(&refused(403)), "{job:?}");
            assert!(!job.unavailable(&forever), "{job:?}");
        }

        let after = |rejections| JobState {
            rejections,
            ..JobState::default()
        };
        assert!(!Job::ProjectBoards(7).unavailable(&after(2)));
        assert!(Job::ProjectBoards(7).unavailable(&after(3)));
        assert!(!epics.unavailable(&after(0)));
        assert!(epics.unavailable(&after(1)));
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

        let after = |secs: u64| chrono::DateTime::from_timestamp(secs as i64, 0);
        let cursors: Vec<_> = fake
            .calls_to("projects/7/issues")
            .into_iter()
            .map(|l| match l {
                Listing::ProjectIssues { updated_after, .. } => updated_after,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            cursors,
            [
                None,
                after(NOW - 300),
                after(NOW - 3600 - 300),
                None,
                after(NOW - 300),
            ],
            "a full walk is followed by a delta from its start; a delta's \
             cursor overlaps the previous run"
        );
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
        assert_eq!(fake.limits_to("projects/7/issues"), [Some(2), Some(2)]);
    }

    /// Stored #1..#3 in project 7, #1 in the assigned view; the test cap is 2.
    fn seed_three(s: &SyncStore) {
        let mut c = s.begin();
        let rows: Vec<Issue> = (1..=3)
            .map(|iid| serde_json::from_value(issue_json(7, iid, "")).unwrap())
            .collect();
        c.upsert(&rows).unwrap();
        c.set_view(
            ASSIGNED_ISSUES,
            &View {
                keys: vec![(7, 1)],
                fetched_at: NOW,
            },
        )
        .unwrap();
        c.commit().unwrap();
    }

    #[tokio::test]
    async fn the_cap_never_drops_an_assigned_item() {
        let (s, _d) = store();
        seed_three(&s);
        let fake = Arc::new(FakeGitlab::default());
        fake.serve(
            "projects/7/issues",
            vec![issue_json(7, 3, "new"), issue_json(7, 2, "mid")],
        );
        run(&s, Job::ProjectIssues(7), ctx(&fake, true, 0)).await;
        assert_eq!(
            s.issues.keys(RowScope::All).unwrap(),
            [(7, 1), (7, 2), (7, 3)]
        );
    }

    /// A closed issue older than the cap stays for the recent list naming
    /// it, as an assigned one does.
    #[tokio::test]
    async fn the_cap_never_drops_a_recently_listed_item() {
        let (s, _d) = store();
        seed_three(&s);
        let mut c = s.begin();
        c.remove_view(ASSIGNED_ISSUES);
        let listed = View {
            keys: vec![(7, 1), (8, 1)],
            fetched_at: NOW - 100,
        };
        c.set_view(RECENT_AUTHORED_ISSUES, &listed).unwrap();
        c.commit().unwrap();
        let fake = Arc::new(FakeGitlab::default());
        fake.serve(
            "projects/7/issues",
            vec![issue_json(7, 3, "new"), issue_json(7, 2, "mid")],
        );
        run(&s, Job::ProjectIssues(7), ctx(&fake, true, 0)).await;
        assert_eq!(
            s.issues.keys(RowScope::All).unwrap(),
            [(7, 1), (7, 2), (7, 3)]
        );
    }

    #[tokio::test]
    async fn a_delta_that_fills_the_cap_reconciles_like_a_full_run() {
        let (s, _d) = store();
        seed_three(&s);
        let mut c = s.begin();
        c.remove_view(ASSIGNED_ISSUES);
        c.commit().unwrap();
        let fake = Arc::new(FakeGitlab::default());
        fake.serve(
            "projects/7/issues",
            vec![issue_json(7, 3, "new"), issue_json(7, 2, "mid")],
        );
        run(&s, Job::ProjectIssues(7), ctx(&fake, false, NOW - 3600)).await;
        assert_eq!(s.issues.keys(RowScope::All).unwrap(), [(7, 2), (7, 3)]);
    }

    /// #1 was updated while the walk was on page 2: it moved to page 1,
    /// already read, and only the follow-up delta sees it.
    #[tokio::test]
    async fn an_item_updated_mid_walk_survives_the_reconcile() {
        let (s, _d) = store();
        seed_three(&s);
        let mut c = s.begin();
        c.remove_view(ASSIGNED_ISSUES);
        c.commit().unwrap();
        let fake = Arc::new(FakeGitlab::default());
        fake.serve_next("projects/7/issues", vec![issue_json(7, 3, "")]);
        fake.serve("projects/7/issues", vec![issue_json(7, 1, "moved")]);
        run(&s, Job::ProjectIssues(7), ctx(&fake, true, 0)).await;
        assert_eq!(s.issues.keys(RowScope::All).unwrap(), [(7, 1), (7, 3)]);
        assert_eq!(s.issues.get((7, 1)).unwrap().unwrap().title, "moved");
    }

    /// Epics go through the capped fetch of the project jobs, keyed by their
    /// group and with no assigned view to spare rows from the reconcile.
    #[tokio::test]
    async fn a_groups_epics_are_capped_and_reconciled() {
        let (s, _d) = store();
        let fake = Arc::new(FakeGitlab::default());
        fake.serve(
            "groups/3/epics",
            vec![
                epic_json(3, 3, "new"),
                epic_json(3, 2, "mid"),
                epic_json(3, 1, "old"),
            ],
        );
        run(&s, Job::GroupEpics(3), ctx(&fake, true, 0)).await;
        assert_eq!(s.epics.keys(RowScope::All).unwrap(), [(3, 2), (3, 3)]);
        assert_eq!(fake.limits_to("groups/3/epics"), [Some(2), Some(2)]);

        // A delta that sees only &3 keeps &2; the next full run drops it.
        fake.serve("groups/3/epics", vec![epic_json(3, 3, "new v2")]);
        run(&s, Job::GroupEpics(3), ctx(&fake, false, NOW - 3600)).await;
        assert_eq!(s.epics.keys(RowScope::All).unwrap(), [(3, 2), (3, 3)]);
        assert_eq!(s.epics.get((3, 3)).unwrap().unwrap().title, "new v2");
        run(&s, Job::GroupEpics(3), ctx(&fake, true, NOW - 3600)).await;
        assert_eq!(s.epics.keys(RowScope::All).unwrap(), [(3, 3)]);
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
        let two_days_before = |secs: u64| {
            chrono::DateTime::from_timestamp(secs as i64, 0)
                .unwrap()
                .date_naive()
                .checked_sub_days(chrono::Days::new(2))
        };
        assert_eq!(
            fake.calls_to("events"),
            [
                Listing::Events {
                    after: two_days_before(NOW - 30 * DAY)
                },
                Listing::Events {
                    after: two_days_before(NOW - 3600)
                },
            ],
            "`after` is exclusive and zoned, so each fetch starts two days before"
        );

        // GitLab's answer changes though events don't: a row it hid at walk
        // time shows up, one it showed is gone. The full re-walk adds the
        // first and keeps the second.
        fake.serve(
            "events",
            vec![event_json(3, 9, "commented on", NOW - 20 * DAY)],
        );
        run(&s, Job::Events, ctx(&fake, true, NOW - 3600)).await;
        let mut kept: Vec<i64> = s
            .events
            .scan(RowScope::All)
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        kept.sort_unstable();
        assert_eq!(
            kept,
            [1, 3],
            "a re-walk backfills and drops nothing inside the window"
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

        // #2's issue became unreadable: it is still listed, without details,
        // and keeps its stored row.
        let hidden = Timelog {
            iid: 0,
            title: String::new(),
            ..log(2, NOW - 2 * DAY)
        };
        fake.serve_timelogs(vec![hidden]);
        run(&s, Job::AllTimelogs, ctx(&fake, true, 0)).await;
        assert_eq!(ids(&s), [2]);
        assert_eq!(
            fake.timelog_calls()[1],
            chrono::DateTime::from_timestamp((NOW - DAY) as i64, 0).unwrap()
        );
    }

    #[tokio::test]
    async fn an_avatar_lands_as_a_file_named_by_its_bytes_in_its_row() {
        let (s, d) = store();
        let dir = d.path().join("avatars");
        let fake = Arc::new(FakeGitlab::default());
        fake.serve_avatar(7, PNG);
        run(&s, Job::ProjectAvatar(7), ctx_in(&fake, true, 0, &dir)).await;

        let row = s.avatars.get((7, 0)).unwrap().unwrap();
        assert_eq!(Some(&row.file), avatars::file_name(7, PNG).as_ref());
        assert_eq!(std::fs::read(dir.join(&row.file)).unwrap(), PNG);
        assert_eq!(fake.avatar_calls(), [7]);

        // Another run (a new `?v=`) names the same image the same; a new
        // image gets a new name next to the old file, which the worker
        // removes once the row landed.
        run(&s, Job::ProjectAvatar(7), ctx_in(&fake, true, 0, &dir)).await;
        assert_eq!(s.avatars.get((7, 0)).unwrap().unwrap(), row);
        fake.serve_avatar(7, b"GIF89a");
        run(&s, Job::ProjectAvatar(7), ctx_in(&fake, true, 0, &dir)).await;
        let new = s.avatars.get((7, 0)).unwrap().unwrap().file;
        assert!(new.starts_with("7-") && new.ends_with(".gif"), "{new}");
        assert_eq!(std::fs::read(dir.join(&new)).unwrap(), b"GIF89a");
        assert!(dir.join(&row.file).is_file());
    }

    /// No avatar (a 404), an oversized one and an HTML error page all leave
    /// a row without a file: a success, so the job doesn't run again.
    #[tokio::test]
    async fn an_unusable_avatar_leaves_a_row_without_a_file() {
        let (s, d) = store();
        let dir = d.path().join("avatars");
        let fake = Arc::new(FakeGitlab::default());
        let mut huge = PNG.to_vec();
        huge.resize(avatars::MAX_BYTES + 1, 0);
        fake.serve_avatar(8, &huge);
        fake.serve_avatar(9, b"<html>Sign in</html>");
        for project in [7, 8, 9] {
            let job = Job::ProjectAvatar(project);
            run(&s, job, ctx_in(&fake, true, 0, &dir)).await;
            let row = s.avatars.get((project as u64, 0)).unwrap().unwrap();
            assert_eq!(row.file, "", "project {project}");
        }
        assert!(!dir.exists(), "nothing was written");
    }

    #[tokio::test]
    async fn a_failed_avatar_download_stores_nothing() {
        let (s, d) = store();
        let fake = Arc::new(FakeGitlab::default());
        fake.fail_next("projects/7/avatar", FakeErr::Rejected);
        let failed = fetch(Job::ProjectAvatar(7), ctx_in(&fake, true, 0, d.path())).await;
        assert!(failed.is_err());
        assert!(s.avatars.get((7, 0)).unwrap().is_none());
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
            [
                Job::AssignedIssues,
                Job::ProjectIssues(7),
                Job::RecentAuthoredIssues,
                Job::RecentAssignedIssues,
            ]
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
            Job::affected_by(&w(Issuable::Issue, post.clone())),
            [Job::RecentTimelogs]
        );

        // Replayed, it is booked when it was queued: past the recent window
        // only the full history shows it.
        let cfg = crate::config::defaults();
        let replayed = |queued_at| {
            Job::affected_by_replay(&w(Issuable::Issue, post.clone()), queued_at, &cfg, NOW)
        };
        assert_eq!(replayed(NOW - 3600), [Job::RecentTimelogs]);
        assert_eq!(
            replayed(NOW - 2 * DAY),
            [Job::RecentTimelogs, Job::AllTimelogs]
        );
    }
}
