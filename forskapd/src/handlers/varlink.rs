//! The [`VarlinkInterface`] method implementations plus the write cascade they
//! share. Reads serve the sync store only; see the module docs of
//! [`super`] for the conventions.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use tracing::{debug, info, instrument, warn};

use forskap_api::{
    Call_AssignSelf, Call_ClearCache, Call_ClearFailures, Call_Close, Call_DismissFailure,
    Call_GetAssignedIssues, Call_GetAssignedMergeRequests, Call_GetFailures, Call_GetHistory,
    Call_GetSyncJobs, Call_Login, Call_Logout, Call_PostTime, Call_RecordOpen, Call_RetryFailure,
    Call_Search, Call_UnassignSelf, Call_WhoAmI, FailedTask, Group, HistoryEvent, IssuableKind,
    Issue, MergeRequest, Project, VarlinkInterface,
};

use crate::error::{DormancyReason, Error};
use crate::gitlab::{GitlabClient, Issuable};
use crate::query::{in_group, namespace_of, parse_iid_query, text_matches};
use crate::secrets::{self, Credentials, Token};
use crate::sync::jobs::{ASSIGNED_ISSUES, ASSIGNED_MERGE_REQUESTS};
use crate::sync::model::{self, RowKey};
use crate::sync::store::{RowScope, Stored, SyncStore};
use crate::sync::{Clear, Job};
use crate::usage::{UsageEntry, UsageRecord};
use crate::write::{Write, WriteOp};

use super::{
    ConnState, Handlers, Session, dormant_args, issue_ref_error, looks_like_duration, now_secs,
    wire,
};

/// The kind strings `Search` accepts, matching the `ClearCache` scope style.
const SEARCH_KINDS: [&str; 4] = ["issues", "merge_requests", "projects", "groups"];

/// How long `GetSyncJobs` waits for the worker, which answers between two
/// awaits even with a fetch in flight.
const SYNC_JOBS_TIMEOUT: Duration = Duration::from_secs(5);

/// Per-kind result cap when the caller doesn't pass a `limit`.
const DEFAULT_SEARCH_LIMIT: usize = 50;

/// How long `ClearCache` waits for the foreground views (and a cleared
/// history) to refill before replying anyway; the rest refills in the
/// background.
const CLEAR_REFILL_TIMEOUT: Duration = Duration::from_secs(30);

/// Why a read has nothing to serve: its source never synced.
enum Cold {
    /// Connected; the first sync is still pending.
    Pending,
    Dormant(DormancyReason),
}

/// How [`Handlers::perform_write`] ended.
enum WriteOutcome {
    /// Applied, or queued for the retry worker.
    Accepted,
    NotAuthenticated(DormancyReason),
    Rejected(Error),
}

/// Reply to a read whose source never synced — an honest `NotAuthenticated`
/// while dormant, an empty reply while the first sync is pending — and
/// return; fall through when it has synced. A macro because every method
/// has its own generated call trait.
macro_rules! reply_if_cold {
    ($self:ident, $call:ident, $job:expr, ($($empty:expr),*)) => {
        match $self.cold($job).await {
            Some(Cold::Pending) => return $call.reply($($empty),*),
            Some(Cold::Dormant(r)) => {
                let (reason, detail) = dormant_args(&r);
                return $call.reply_not_authenticated(reason, detail);
            }
            None => {}
        }
    };
}

/// Reply to a write call from its [`WriteOutcome`].
macro_rules! reply_write {
    ($call:expr, $outcome:expr) => {
        match $outcome {
            WriteOutcome::Accepted => $call.reply(),
            WriteOutcome::NotAuthenticated(r) => {
                let (reason, detail) = dormant_args(&r);
                $call.reply_not_authenticated(reason, detail)
            }
            WriteOutcome::Rejected(e) => $call.reply_gitlab_error(e.to_string()),
        }
    };
}

impl Handlers {
    fn store(&self) -> &SyncStore {
        self.sync.store()
    }

    async fn cold(&self, job: Job) -> Option<Cold> {
        if self.sync.has_synced(job) {
            return None;
        }
        Some(match self.gitlab().await {
            Ok(_) => Cold::Pending,
            Err(r) => Cold::Dormant(r),
        })
    }

    /// The rows the assigned view `name` lists, minus the items a write took
    /// out of it that the view doesn't reflect yet: closes and unassigns
    /// still in the retry queue, and ones applied after the view's fetch
    /// began.
    fn assigned<R: Stored>(&self, name: &str, kind: Issuable) -> Vec<R> {
        let view = self.store().view(name).unwrap_or_else(|e| {
            warn!(error = %e, view = name, "view read failed, treating as empty");
            None
        });
        let Some(view) = view else {
            return Vec::new();
        };
        let pending = self.queue.pending().unwrap_or_else(|e| {
            warn!(error = %e, "queue scan failed; queued writes not reflected");
            Vec::new()
        });
        let hidden: HashSet<RowKey> = pending
            .into_iter()
            .map(|p| p.write)
            .chain(self.sync.writes_since(view.fetched_at))
            .filter(|w| w.kind == kind && matches!(w.op, WriteOp::Close | WriteOp::UnassignSelf))
            .map(|w| (w.project_id.max(0) as u64, w.iid.max(0) as u64))
            .collect();
        let table = self.store().table::<R>();
        view.keys
            .into_iter()
            .filter(|k| !hidden.contains(k))
            .filter_map(|k| {
                table.get(k).unwrap_or_else(|e| {
                    warn!(error = %e, kind = R::NAME, "row read failed, skipping");
                    None
                })
            })
            .collect()
    }

    /// The user id whose data the store holds, if known.
    fn synced_user(&self) -> Option<i64> {
        self.store()
            .identity()
            .unwrap_or_else(|e| {
                warn!(error = %e, "identity read failed");
                None
            })
            .map(|i| i.user_id)
    }

    /// Every stored row of `R`, empty on a read failure.
    fn all<R: Stored>(&self) -> Vec<R> {
        self.store()
            .table::<R>()
            .scan(RowScope::All)
            .unwrap_or_else(|e| {
                warn!(error = %e, kind = R::NAME, "store read failed, treating as empty");
                Vec::new()
            })
    }

    /// The global numeric id GraphQL embeds in `gid://gitlab/<Kind>/<id>`,
    /// so a queued PostTime can keep its time. `None` when not stored — the
    /// replay then looks it up.
    fn resolve_issuable_id(&self, kind: Issuable, project_id: i64, iid: i64) -> Option<i64> {
        let key = (project_id.max(0) as u64, iid.max(0) as u64);
        let id = match kind {
            Issuable::Issue => self.store().issues.get(key).map(|i| i.map(|i| i.id)),
            Issuable::MergeRequest => self
                .store()
                .merge_requests
                .get(key)
                .map(|m| m.map(|m| m.id)),
        };
        id.ok().flatten()
    }

    /// The open statistics, degraded to empty on a read failure so ranking
    /// merely falls back to recency (the standing cache-error convention).
    fn usage_or_empty(&self) -> UsageRecord {
        self.usage.snapshot().unwrap_or_else(|e| {
            warn!(error = %e, "usage read failed, ranking without open counts");
            UsageRecord::default()
        })
    }

    /// The shared write cascade: try once while connected; queue the write
    /// when GitLab is unreachable or the failure is retryable; otherwise hand
    /// the rejection back.
    async fn perform_write(&self, write: Write) -> WriteOutcome {
        let (kind, project_id, iid, op) =
            (write.kind, write.project_id, write.iid, write.op.name());
        let gitlab = match self.gitlab().await {
            Ok(g) => g,
            Err(DormancyReason::Unreachable { .. }) => {
                info!(
                    project_id,
                    iid,
                    ?kind,
                    op,
                    "GitLab unreachable, queuing write for retry"
                );
                self.defer(write).await;
                return WriteOutcome::Accepted;
            }
            Err(r) => return WriteOutcome::NotAuthenticated(r),
        };
        match write.apply(&*gitlab, None).await {
            Ok(()) => {
                info!(project_id, iid, ?kind, op, "write applied");
                self.sync.note_write(&write);
                self.sync.refresh_soon(&Job::affected_by(&write));
                WriteOutcome::Accepted
            }
            Err(e) if e.is_retryable(write.op.idempotent()) => {
                warn!(error = %e, project_id, iid, op, "write failed transiently, queuing for retry");
                self.defer(write).await;
                WriteOutcome::Accepted
            }
            Err(e) => {
                warn!(error = %e, project_id, iid, op, "write rejected by GitLab");
                WriteOutcome::Rejected(e)
            }
        }
    }

    /// Queue `write` for the retry worker. A PostTime gets its issuable id
    /// resolved so the replay keeps its time.
    async fn defer(&self, mut write: Write) {
        if let WriteOp::PostTime { issuable_id, .. } = &mut write.op {
            *issuable_id = self.resolve_issuable_id(write.kind, write.project_id, write.iid);
        }
        self.queue.enqueue(write).await;
    }
}

/// Board list labels per project, read at most once per request. `None` for
/// a project whose boards never synced, so its `graph_status` stays empty.
struct BoardLabels<'a> {
    handlers: &'a Handlers,
    by_project: HashMap<i64, Option<Vec<String>>>,
}

impl<'a> BoardLabels<'a> {
    fn new(handlers: &'a Handlers) -> Self {
        Self {
            handlers,
            by_project: HashMap::new(),
        }
    }

    fn of(&mut self, project_id: i64) -> Option<&[String]> {
        let h = self.handlers;
        self.by_project
            .entry(project_id)
            .or_insert_with(|| {
                if !h.sync.has_synced(Job::ProjectBoards(project_id)) {
                    return None;
                }
                let boards = h
                    .store()
                    .boards
                    .scan(RowScope::Prefix(project_id.max(0) as u64))
                    .unwrap_or_else(|e| {
                        warn!(error = %e, project_id, "board read failed");
                        Vec::new()
                    });
                Some(
                    boards
                        .iter()
                        .flat_map(|b| b.labels().map(str::to_string))
                        .collect(),
                )
            })
            .as_deref()
    }

    /// The wire issue for `i`, with its `graph_status` from the board labels.
    fn wire(&mut self, i: model::Issue, open_count: i64, project_avatar: String) -> Issue {
        let labels = self.of(i.project_id).map(<[String]>::to_vec);
        wire::issue(i, labels.as_deref(), open_count, project_avatar)
    }
}

/// Avatar file paths per project, read at most once per request from the
/// rows the sync wrote; the filesystem is never asked.
struct Avatars<'a> {
    handlers: &'a Handlers,
    by_project: HashMap<i64, String>,
}

impl<'a> Avatars<'a> {
    fn new(handlers: &'a Handlers) -> Self {
        Self {
            handlers,
            by_project: HashMap::new(),
        }
    }

    /// The absolute path of the project's avatar, empty when it has none.
    fn of(&mut self, project_id: i64) -> String {
        let h = self.handlers;
        self.by_project
            .entry(project_id)
            .or_insert_with(|| {
                let row = h.store().avatars.get((project_id.max(0) as u64, 0));
                let avatar = row.unwrap_or_else(|e| {
                    warn!(error = %e, project_id, "avatar read failed");
                    None
                });
                avatar
                    .filter(|a| !a.file.is_empty())
                    .map(|a| {
                        let path = h.sync.avatars().path_of(&a.file);
                        path.to_string_lossy().into_owned()
                    })
                    .unwrap_or_default()
            })
            .clone()
    }
}

/// Sort key for `Search` hits: most-opened first, then most recently opened,
/// then most recently updated — so never-opened items keep the old
/// newest-first order among themselves.
fn rank_key(usage: Option<UsageEntry>, updated_at: u64) -> std::cmp::Reverse<(u64, u64, u64)> {
    let u = usage.unwrap_or_default();
    std::cmp::Reverse((u.count, u.last_opened_secs, updated_at))
}

/// `open_count` for the wire, from an optional usage entry.
fn open_count_of(usage: Option<UsageEntry>) -> i64 {
    usage.map_or(0, |u| u.count as i64)
}

/// Whether an issue/MR matches the search: case-insensitive substring on the
/// title or any label, or an exact `#iid` reference query.
fn search_item_matches(
    needle: &str,
    iid_query: Option<i64>,
    title: &str,
    labels: &[String],
    iid: i64,
) -> bool {
    text_matches(needle, title)
        || labels.iter().any(|l| text_matches(needle, l))
        || iid_query == Some(iid)
}

/// Whether an item from an assigned view still is open and assigned to `me`
/// by its current row: a project sync may have updated the row since the
/// view was fetched. Rows without assignee data aren't second-guessed.
fn still_assigned(state: &str, assignees: &[model::UserRef], me: Option<i64>) -> bool {
    state == "opened"
        && (assignees.is_empty() || me.is_none_or(|me| assignees.iter().any(|a| a.id == me)))
}

/// Whether `web_url` lies in any of `groups` (subgroups included). No filter
/// matches everything.
fn in_groups(groups: &Option<Vec<String>>, web_url: &str) -> bool {
    match groups {
        Some(groups) if !groups.is_empty() => {
            let ns = namespace_of(web_url);
            groups.iter().any(|g| in_group(&ns, g))
        }
        _ => true,
    }
}

#[async_trait::async_trait]
impl VarlinkInterface for Handlers {
    #[instrument(skip(self, call))]
    async fn get_assigned_issues(
        &self,
        call: &mut dyn Call_GetAssignedIssues,
        groups: Option<Vec<String>>,
    ) -> varlink::Result<()> {
        reply_if_cold!(self, call, Job::AssignedIssues, (Vec::new()));

        let me = self.synced_user();
        let mut rows: Vec<model::Issue> = self.assigned(ASSIGNED_ISSUES, Issuable::Issue);
        rows.retain(|i| {
            still_assigned(&i.state, &i.assignees, me) && in_groups(&groups, &i.web_url)
        });
        // Grouped by namespace; GitLab's order within each.
        rows.sort_by_cached_key(|i| namespace_of(&i.web_url));

        let usage = self.usage_or_empty();
        let mut boards = BoardLabels::new(self);
        let mut avatars = Avatars::new(self);
        let issues: Vec<Issue> = rows
            .into_iter()
            .map(|i| {
                let open = open_count_of(usage.get(Issuable::Issue, i.project_id, i.iid));
                let avatar = avatars.of(i.project_id);
                boards.wire(i, open, avatar)
            })
            .collect();
        debug!(count = issues.len(), "serving assigned issues");
        call.reply(issues)
    }

    #[instrument(skip(self, call))]
    async fn get_assigned_merge_requests(
        &self,
        call: &mut dyn Call_GetAssignedMergeRequests,
        groups: Option<Vec<String>>,
    ) -> varlink::Result<()> {
        reply_if_cold!(self, call, Job::AssignedMergeRequests, (Vec::new()));

        let me = self.synced_user();
        let mut rows: Vec<model::MergeRequest> =
            self.assigned(ASSIGNED_MERGE_REQUESTS, Issuable::MergeRequest);
        rows.retain(|m| {
            still_assigned(&m.state, &m.assignees, me) && in_groups(&groups, &m.web_url)
        });
        // The wire type carries no timestamp, so order for the picker here.
        rows.sort_by_key(|m| std::cmp::Reverse(m.updated_at));

        let usage = self.usage_or_empty();
        let mut avatars = Avatars::new(self);
        let mrs: Vec<MergeRequest> = rows
            .into_iter()
            .map(|m| {
                let open = open_count_of(usage.get(Issuable::MergeRequest, m.project_id, m.iid));
                let avatar = avatars.of(m.project_id);
                wire::merge_request(m, open, avatar)
            })
            .collect();
        debug!(count = mrs.len(), "serving assigned merge requests");
        call.reply(mrs)
    }

    #[instrument(skip(self, call))]
    async fn search(
        &self,
        call: &mut dyn Call_Search,
        query: String,
        kinds: Option<Vec<String>>,
        limit: Option<i64>,
    ) -> varlink::Result<()> {
        // An empty query is the "frequently opened" view: only issues/MRs with
        // recorded opens, ranked. Projects and groups have no open counts, so
        // they come back empty in that mode.
        let needle = query.trim().to_lowercase();
        let frequent_only = needle.is_empty();
        let limit = match limit {
            None => DEFAULT_SEARCH_LIMIT,
            Some(n) if n > 0 => n as usize,
            Some(n) => return call.reply_gitlab_error(format!("invalid limit: {n}")),
        };
        let kinds = kinds.unwrap_or_default();
        if let Some(bad) = kinds.iter().find(|k| !SEARCH_KINDS.contains(&k.as_str())) {
            return call.reply_gitlab_error(format!(
                "unknown kind {bad:?} (expected one of: {})",
                SEARCH_KINDS.join(", ")
            ));
        }
        let want = |k: &str| kinds.is_empty() || kinds.iter().any(|x| x == k);

        reply_if_cold!(
            self,
            call,
            Job::MemberProjects,
            (Vec::new(), Vec::new(), Vec::new(), Vec::new())
        );

        let iid_query = parse_iid_query(&query);
        let usage = self.usage_or_empty();
        let mut avatars = Avatars::new(self);

        let mut issues: Vec<Issue> = Vec::new();
        if want("issues") {
            let mut hits: Vec<(Option<UsageEntry>, model::Issue)> = self
                .all::<model::Issue>()
                .into_iter()
                .filter(|i| search_item_matches(&needle, iid_query, &i.title, &i.labels, i.iid))
                .map(|i| (usage.get(Issuable::Issue, i.project_id, i.iid), i))
                .filter(|(u, _)| !frequent_only || u.is_some())
                .collect();
            hits.sort_by_key(|(u, i)| rank_key(*u, i.updated_at));
            hits.truncate(limit);
            let mut boards = BoardLabels::new(self);
            issues = hits
                .into_iter()
                .map(|(u, i)| {
                    let avatar = avatars.of(i.project_id);
                    boards.wire(i, open_count_of(u), avatar)
                })
                .collect();
        }

        let mut merge_requests: Vec<MergeRequest> = Vec::new();
        if want("merge_requests") {
            let mut hits: Vec<(Option<UsageEntry>, model::MergeRequest)> = self
                .all::<model::MergeRequest>()
                .into_iter()
                .filter(|m| search_item_matches(&needle, iid_query, &m.title, &m.labels, m.iid))
                .map(|m| (usage.get(Issuable::MergeRequest, m.project_id, m.iid), m))
                .filter(|(u, _)| !frequent_only || u.is_some())
                .collect();
            hits.sort_by_key(|(u, m)| rank_key(*u, m.updated_at));
            hits.truncate(limit);
            merge_requests = hits
                .into_iter()
                .map(|(u, m)| {
                    let avatar = avatars.of(m.project_id);
                    wire::merge_request(m, open_count_of(u), avatar)
                })
                .collect();
        }

        let mut projects: Vec<Project> = Vec::new();
        if want("projects") && !frequent_only {
            let mut hits = self.all::<model::Project>();
            hits.retain(|p| {
                text_matches(&needle, &p.name) || text_matches(&needle, &p.path_with_namespace)
            });
            hits.sort_by(|a, b| a.path_with_namespace.cmp(&b.path_with_namespace));
            hits.truncate(limit);
            projects = hits
                .into_iter()
                .map(|p| {
                    let avatar = avatars.of(p.id);
                    wire::project(p, avatar)
                })
                .collect();
        }

        let mut groups: Vec<Group> = Vec::new();
        if want("groups") && !frequent_only {
            let mut hits = self.all::<model::Group>();
            hits.retain(|g| text_matches(&needle, &g.name) || text_matches(&needle, &g.full_path));
            hits.sort_by(|a, b| a.full_path.cmp(&b.full_path));
            hits.truncate(limit);
            groups = hits.into_iter().map(wire::group).collect();
        }

        debug!(
            issues = issues.len(),
            merge_requests = merge_requests.len(),
            projects = projects.len(),
            groups = groups.len(),
            "serving search results"
        );
        call.reply(issues, merge_requests, projects, groups)
    }

    #[instrument(skip(self, call))]
    async fn clear_cache(
        &self,
        call: &mut dyn Call_ClearCache,
        scope: Option<Vec<String>>,
    ) -> varlink::Result<()> {
        let scopes = scope.unwrap_or_default();
        let all = scopes.is_empty();
        let want = |s: &str| all || scopes.iter().any(|x| x == s);

        let now = now_secs();
        let (quick_start, retention_start) = {
            let c = self.config.read().unwrap();
            (
                now.saturating_sub(c.refresh.quick.window().as_secs()),
                now.saturating_sub(c.history.retention().as_secs()),
            )
        };
        let mut clears = Vec::new();
        if all {
            clears.push(Clear::Everything);
        } else {
            if want("issues") {
                clears.push(Clear::Assigned);
            }
            if want("search") {
                clears.push(Clear::Corpus);
            }
            for (band, from, until) in [
                ("quick", quick_start, u64::MAX),
                ("slow", retention_start, quick_start),
                ("stale", 0, retention_start),
            ] {
                if want(band) {
                    clears.push(Clear::Timelogs { from, until });
                }
            }
        }
        let mut refill: Vec<Job> = clears.iter().flat_map(|c| c.refill()).copied().collect();
        refill.sort_unstable();
        refill.dedup();
        // Queued together, so no scheduled run slips in between.
        let cleared: Vec<_> = clears.into_iter().map(|c| self.sync.clear(c)).collect();
        let refilled = (!refill.is_empty()).then(|| self.sync.refresh_now(&refill));
        for c in cleared {
            c.await;
        }

        // Open statistics are user data, not a cache: only an explicit scope
        // clears them, never the "everything" default.
        if scopes.iter().any(|s| s == "usage") {
            if let Err(e) = self.usage.clear() {
                warn!("usage stats clear failed: {e}");
            } else {
                info!("usage stats cleared");
            }
        }

        // Resolves at once while dormant: the worker drops demands then.
        if let Some(refilled) = refilled
            && tokio::time::timeout(CLEAR_REFILL_TIMEOUT, refilled)
                .await
                .is_err()
        {
            warn!("refill still running; replying before it lands");
        }
        call.reply()
    }

    #[instrument(skip(self, call))]
    async fn post_time(
        &self,
        call: &mut dyn Call_PostTime,
        project_id: i64,
        iid: i64,
        kind: IssuableKind,
        duration: String,
        summary: Option<String>,
    ) -> varlink::Result<()> {
        if let Some(msg) = issue_ref_error(project_id, iid) {
            return call.reply_gitlab_error(msg);
        }
        if !looks_like_duration(&duration) {
            return call.reply_gitlab_error(format!("invalid duration: {duration:?}"));
        }
        let write = Write {
            kind: wire::internal_kind(&kind),
            project_id,
            iid,
            op: WriteOp::PostTime {
                duration,
                summary,
                issuable_id: None,
            },
        };
        reply_write!(call, self.perform_write(write).await)
    }

    #[instrument(skip(self, call))]
    async fn get_history(
        &self,
        call: &mut dyn Call_GetHistory,
        days: Option<i64>,
    ) -> varlink::Result<()> {
        let now = now_secs();
        let days = days.unwrap_or(7).max(0) as u64;
        let cutoff = now.saturating_sub(days.saturating_mul(86_400));

        let mut events: Vec<HistoryEvent> = Vec::new();
        match self.queue.pending() {
            Ok(pending) => {
                for p in pending {
                    let Write {
                        kind,
                        project_id,
                        iid,
                        op:
                            WriteOp::PostTime {
                                duration, summary, ..
                            },
                    } = p.write
                    else {
                        continue;
                    };
                    let key = (project_id.max(0) as u64, iid.max(0) as u64);
                    let (title, web_url) = match kind {
                        Issuable::Issue => self
                            .store()
                            .issues
                            .get(key)
                            .ok()
                            .flatten()
                            .map(|i| (i.title, i.web_url)),
                        Issuable::MergeRequest => self
                            .store()
                            .merge_requests
                            .get(key)
                            .ok()
                            .flatten()
                            .map(|m| (m.title, m.web_url)),
                    }
                    .unwrap_or_default();
                    events.push(HistoryEvent {
                        timestamp: p.queued_at_secs as i64,
                        source: "queued".to_string(),
                        kind: wire::kind(kind),
                        project_id,
                        iid,
                        title,
                        web_url,
                        duration,
                        summary: summary.unwrap_or_default(),
                    });
                }
            }
            Err(e) => warn!(error = %e, "queue scan failed; queued events omitted"),
        }

        match self.store().timelogs.scan(RowScope::Since(cutoff)) {
            // Stored oldest first; reply newest first.
            Ok(logs) => events.extend(logs.into_iter().rev().map(wire::timelog)),
            Err(e) => warn!(error = %e, "history read failed; returning queued only"),
        }
        call.reply(events)
    }

    /// Status, not GitLab data: served whatever the session is.
    #[instrument(skip(self, call))]
    async fn get_sync_jobs(&self, call: &mut dyn Call_GetSyncJobs) -> varlink::Result<()> {
        let snapshot = match tokio::time::timeout(SYNC_JOBS_TIMEOUT, self.sync.jobs()).await {
            Ok(s) => s,
            Err(_) => {
                warn!("the sync worker didn't report its jobs in time; returning empty");
                Default::default()
            }
        };
        call.reply(
            snapshot.jobs.into_iter().map(wire::sync_job).collect(),
            snapshot.paused_until.map(|at| at as i64),
        )
    }

    #[instrument(skip(self, call))]
    async fn get_failures(&self, call: &mut dyn Call_GetFailures) -> varlink::Result<()> {
        let failures = match self.queue.failures() {
            Ok(f) => f,
            Err(e) => {
                warn!(error = %e, "dead-letter read failed; returning empty");
                Vec::new()
            }
        };
        let out = failures
            .into_iter()
            .map(|f| FailedTask {
                id: f.id as i64,
                op: f.op_kind.to_string(),
                kind: wire::kind(f.kind),
                project_id: f.project_id,
                iid: f.iid,
                detail: f.detail,
                error: f.error,
                queued_at: f.queued_at_secs as i64,
                failed_at: f.failed_at_secs as i64,
            })
            .collect();
        call.reply(out)
    }

    #[instrument(skip(self, call))]
    async fn retry_failure(
        &self,
        call: &mut dyn Call_RetryFailure,
        id: i64,
    ) -> varlink::Result<()> {
        match self.queue.retry_failure(id as u64).await {
            Ok(true) => {
                info!(id, "re-enqueued dead-letter task");
                call.reply()
            }
            Ok(false) => call.reply_gitlab_error(format!("no failed task with id {id}")),
            Err(e) => {
                warn!(error = %e, id, "retry_failure failed");
                call.reply_gitlab_error(e.to_string())
            }
        }
    }

    #[instrument(skip(self, call))]
    async fn dismiss_failure(
        &self,
        call: &mut dyn Call_DismissFailure,
        id: i64,
    ) -> varlink::Result<()> {
        match self.queue.dismiss_failure(id as u64) {
            Ok(true) => {
                info!(id, "dismissed dead-letter task");
                call.reply()
            }
            Ok(false) => call.reply_gitlab_error(format!("no failed task with id {id}")),
            Err(e) => {
                warn!(error = %e, id, "dismiss_failure failed");
                call.reply_gitlab_error(e.to_string())
            }
        }
    }

    #[instrument(skip(self, call))]
    async fn clear_failures(&self, call: &mut dyn Call_ClearFailures) -> varlink::Result<()> {
        if let Err(e) = self.queue.clear_failures() {
            warn!(error = %e, "clear_failures failed");
            return call.reply_gitlab_error(e.to_string());
        }
        info!("cleared dead-letter queue");
        call.reply()
    }

    #[instrument(skip(self, call))]
    async fn record_open(
        &self,
        call: &mut dyn Call_RecordOpen,
        project_id: i64,
        iid: i64,
        kind: IssuableKind,
    ) -> varlink::Result<()> {
        if let Some(msg) = issue_ref_error(project_id, iid) {
            return call.reply_gitlab_error(msg);
        }
        // Local bookkeeping only — no GitLab, so it works while dormant.
        let retention_secs = self.config.read().unwrap().usage.retention().as_secs();
        let now = now_secs();
        let key = crate::usage::usage_key(wire::internal_kind(&kind), project_id, iid);
        if let Err(e) = self
            .usage
            .record(&key, now, now.saturating_sub(retention_secs))
        {
            warn!(error = %e, key, "record_open failed");
            return call.reply_gitlab_error(e.to_string());
        }
        debug!(key, "recorded open");
        call.reply()
    }

    #[instrument(skip(self, call))]
    async fn close(
        &self,
        call: &mut dyn Call_Close,
        project_id: i64,
        iid: i64,
        kind: IssuableKind,
    ) -> varlink::Result<()> {
        if let Some(msg) = issue_ref_error(project_id, iid) {
            return call.reply_gitlab_error(msg);
        }
        let write = Write {
            kind: wire::internal_kind(&kind),
            project_id,
            iid,
            op: WriteOp::Close,
        };
        reply_write!(call, self.perform_write(write).await)
    }

    #[instrument(skip(self, call))]
    async fn assign_self(
        &self,
        call: &mut dyn Call_AssignSelf,
        project_id: i64,
        iid: i64,
        kind: IssuableKind,
    ) -> varlink::Result<()> {
        if let Some(msg) = issue_ref_error(project_id, iid) {
            return call.reply_gitlab_error(msg);
        }
        let write = Write {
            kind: wire::internal_kind(&kind),
            project_id,
            iid,
            op: WriteOp::AssignSelf,
        };
        reply_write!(call, self.perform_write(write).await)
    }

    #[instrument(skip(self, call))]
    async fn unassign_self(
        &self,
        call: &mut dyn Call_UnassignSelf,
        project_id: i64,
        iid: i64,
        kind: IssuableKind,
    ) -> varlink::Result<()> {
        if let Some(msg) = issue_ref_error(project_id, iid) {
            return call.reply_gitlab_error(msg);
        }
        let write = Write {
            kind: wire::internal_kind(&kind),
            project_id,
            iid,
            op: WriteOp::UnassignSelf,
        };
        reply_write!(call, self.perform_write(write).await)
    }

    #[instrument(skip(self, call, token))]
    async fn login(
        &self,
        call: &mut dyn Call_Login,
        host: String,
        token: String,
    ) -> varlink::Result<()> {
        let token = Token::new(token);
        let client = match GitlabClient::connect_with_retry(&host, &token).await {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, host, "Login: connecting to GitLab failed");
                return call.reply_gitlab_error(e.to_string());
            }
        };
        let creds = Credentials {
            host: host.clone(),
            token,
        };
        if let Err(e) = secrets::store(&creds).await {
            warn!(error = %e, "Login: keychain write failed");
            return call.reply_gitlab_error(format!("keychain write failed: {e}"));
        }
        let session = Session::from_client(client);
        info!(host, user_id = session.user_id, "logged in");
        *self.session.write().await = ConnState::Connected(session);
        self.queue.drain_waker().notify_one();
        self.sync.logged_in();
        self.rotation.reevaluate();
        call.reply()
    }

    #[instrument(skip(self, call))]
    async fn logout(&self, call: &mut dyn Call_Logout) -> varlink::Result<()> {
        *self.session.write().await = ConnState::Dormant(DormancyReason::LoggedOut);
        self.rotation.reevaluate();
        if let Err(e) = secrets::delete().await {
            warn!(error = %e, "Logout: keychain delete failed");
            return call.reply_gitlab_error(format!("keychain delete failed: {e}"));
        }
        info!("logged out");
        call.reply()
    }

    #[instrument(skip(self, call))]
    async fn who_am_i(&self, call: &mut dyn Call_WhoAmI) -> varlink::Result<()> {
        match self.current_session().await {
            Ok(s) => {
                let auth = self.config.read().unwrap().auth;
                let (token_expires_at, token_rotates) = self.rotation.report(&s.gitlab, &auth);
                call.reply(s.host, s.user_id, token_expires_at, token_rotates)
            }
            Err(e) => {
                let (reason, detail) = dormant_args(&e);
                call.reply_not_authenticated(reason, detail)
            }
        }
    }
}
