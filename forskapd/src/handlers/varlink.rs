//! The [`VarlinkInterface`] method implementations plus the write cascade they
//! share. Reads serve the sync store only; see the module docs of
//! [`super`] for the conventions. `CreateWorkItem` is the one write outside
//! the cascade: it has nothing to be replayed by.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use tracing::{debug, info, instrument, warn};

use forskap_api::{
    ActivityEvent, CacheScope, Call_AssignSelf, Call_ClearCache, Call_ClearFailures, Call_Close,
    Call_CreateWorkItem, Call_DismissFailure, Call_GetActivity, Call_GetAssignedMergeRequests,
    Call_GetAssignedWorkItems, Call_GetFailures, Call_GetHistory, Call_GetStatus, Call_GetSyncJobs,
    Call_ListWorkItems, Call_Login, Call_Logout, Call_PostTime, Call_RecordOpen, Call_RetryFailure,
    Call_Search, Call_UnassignSelf, Call_WhoAmI, FailedTask, Group, HistoryEvent, HistorySource,
    IssuableKind, MergeRequest, Project, SearchKind, SearchScope, VarlinkInterface, WorkItem,
    WorkItemRef, WorkItemRole, WorkItemState,
};

use crate::error::{DormancyReason, Error};
use crate::gitlab::{GitlabApi, GitlabClient, Issuable, NewIssue};
use crate::query::{in_group, namespace_of, parse_epic_query, parse_iid_query, text_matches};
use crate::secrets::{Credentials, Token};
use crate::sync::jobs::{
    ASSIGNED_ISSUES, ASSIGNED_MERGE_REQUESTS, RECENT_ASSIGNED_ISSUES, RECENT_AUTHORED_ISSUES,
};
use crate::sync::model::{self, RowKey};
use crate::sync::store::{Identity, RowScope, Stored, SyncStore, View};
use crate::sync::{Clear, Job};
use crate::usage::{UsageEntry, UsageRecord};
use crate::write::{Write, WriteOp};

use super::{
    ConnState, Handlers, Invalid, Session, dormant_args, issue_ref_error, looks_like_duration,
    new_issue_error, now_secs, open_key, parent_epic, reply_failed, wire,
};

/// How long `GetSyncJobs` waits for the worker, which answers between two
/// awaits even with a fetch in flight.
const SYNC_JOBS_TIMEOUT: Duration = Duration::from_secs(5);

/// How long `CreateWorkItem` waits for the worker to store the new issue before
/// replying anyway. The worker stores it between two awaits; past this the
/// issue shows up with the next sync instead.
const LAND_TIMEOUT: Duration = Duration::from_secs(5);

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
    /// Neither applied nor queued: GitLab refused it, or it may have landed
    /// (a non-idempotent write on a 5xx).
    Failed(Error),
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
            WriteOutcome::Failed(e) => reply_failed($call, &e, e.to_string()),
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

    /// The view `name`; `None` when it was never stored or can't be read.
    fn view(&self, name: &str) -> Option<View> {
        self.store().view(name).unwrap_or_else(|e| {
            warn!(error = %e, view = name, "view read failed, treating as empty");
            None
        })
    }

    /// The stored row at `key`, `None` on a read failure too.
    fn row<R: Stored>(&self, key: RowKey) -> Option<R> {
        self.store().table::<R>().get(key).unwrap_or_else(|e| {
            warn!(error = %e, kind = R::NAME, "row read failed, skipping");
            None
        })
    }

    /// The writes to items of `kind` that `view` doesn't reflect yet: the
    /// ones still in the retry queue, and the ones applied after its fetch
    /// began.
    fn unreflected(&self, view: &View, kind: Issuable) -> Vec<Write> {
        let pending = self.queue.pending().unwrap_or_else(|e| {
            warn!(error = %e, "queue scan failed; queued writes not reflected");
            Vec::new()
        });
        pending
            .into_iter()
            .map(|p| p.write)
            .chain(self.sync.writes_since(view.fetched_at))
            .filter(|w| w.kind == kind)
            .collect()
    }

    /// The rows the assigned view `name` lists, minus the items a write took
    /// out of it that the view doesn't reflect yet (see
    /// [`Self::unreflected`]): closes and unassigns.
    fn assigned<R: Stored>(&self, name: &str, kind: Issuable) -> Vec<R> {
        let Some(view) = self.view(name) else {
            return Vec::new();
        };
        let hidden: HashSet<RowKey> = self
            .unreflected(&view, kind)
            .iter()
            .filter(|w| matches!(w.op, WriteOp::Close | WriteOp::UnassignSelf))
            .map(written_key)
            .collect();
        view.keys
            .into_iter()
            .filter(|k| !hidden.contains(k))
            .filter_map(|k| self.row(k))
            .collect()
    }

    /// The issues the recent view of `role` lists, corrected for the writes
    /// it doesn't reflect yet (see [`Self::unreflected`]): a closed issue
    /// reads closed, and one the user unassigned from, or whose current row
    /// names other assignees, is no longer listed as assigned.
    fn recent_issues(&self, role: &WorkItemRole) -> Vec<model::Issue> {
        let (_, name) = recent_source(role);
        let Some(view) = self.view(name) else {
            return Vec::new();
        };
        let by_assignment = *role == WorkItemRole::assignee;
        let writes = self.unreflected(&view, Issuable::Issue);
        let written = |op: WriteOp| -> HashSet<RowKey> {
            let writes = writes.iter().filter(|w| w.op == op);
            writes.map(written_key).collect()
        };
        let (closed, unassigned) = (written(WriteOp::Close), written(WriteOp::UnassignSelf));
        let me = self.synced_user();
        view.keys
            .into_iter()
            .filter(|k| !(by_assignment && unassigned.contains(k)))
            .filter_map(|k| Some((k, self.row::<model::Issue>(k)?)))
            .filter(|(_, i)| !by_assignment || assigned_to(&i.assignees, me))
            .map(|(k, mut i)| {
                if closed.contains(&k) {
                    i.state = wire::issue_state(&WorkItemState::closed).into();
                }
                i
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
    /// the failure back.
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
                warn!(error = %e, project_id, iid, op, "write failed, not queued");
                WriteOutcome::Failed(e)
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

    /// The legacy id of the epic `iid` of the group, which GitLab's REST
    /// create takes: the stored row's, else GitLab's.
    async fn legacy_epic_id(
        &self,
        gitlab: &dyn GitlabApi,
        group_id: i64,
        iid: i64,
    ) -> Result<i64, Error> {
        if let Some(epic) = self.row::<model::Epic>((group_id as u64, iid as u64)) {
            return Ok(epic.id);
        }
        let epic: model::Epic = serde_json::from_value(gitlab.epic(group_id, iid).await?)
            .map_err(|e| Error::Gitlab(format!("GitLab's answer is no epic: {e}")))?;
        match epic.id {
            id if id > 0 => Ok(id),
            _ => Err(Error::Gitlab("GitLab's answer carries no epic id".into())),
        }
    }
}

/// Board list labels per project, read at most once per request. `None` for
/// a project whose boards never synced, so its `board_column` is absent.
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

    /// The work item for `i`, with its `board_column` from the board labels
    /// and its parent's link from `epics`.
    fn wire(
        &mut self,
        i: model::Issue,
        open_count: i64,
        project: wire::ProjectInfo,
        epics: &mut EpicLinks,
    ) -> WorkItem {
        let labels = self.of(i.project_id).map(<[String]>::to_vec);
        let epic_url = epics.of(&i);
        wire::issue(i, labels.as_deref(), open_count, project, epic_url)
    }
}

/// The links of the stored epics the issues name as their parent, each read
/// at most once per request; `None` without a row.
struct EpicLinks<'a> {
    handlers: &'a Handlers,
    urls: HashMap<RowKey, Option<String>>,
}

impl<'a> EpicLinks<'a> {
    fn new(handlers: &'a Handlers) -> Self {
        Self {
            handlers,
            urls: HashMap::new(),
        }
    }

    fn of(&mut self, issue: &model::Issue) -> Option<String> {
        let epic = issue
            .epic
            .as_ref()
            .filter(|e| e.group_id > 0 && e.iid > 0)?;
        let key = (epic.group_id as u64, epic.iid as u64);
        let h = self.handlers;
        self.urls
            .entry(key)
            .or_insert_with(|| h.row::<model::Epic>(key).map(|e| e.web_url))
            .clone()
    }
}

/// A stored row `Search` found as a work item.
enum WorkItemRow {
    Issue(model::Issue),
    Epic(model::Epic),
}

impl WorkItemRow {
    fn updated_at(&self) -> u64 {
        match self {
            Self::Issue(i) => i.updated_at,
            Self::Epic(e) => e.updated_at,
        }
    }
}

/// What the items show of their project: its path and its avatar file, each
/// read at most once per request from the rows the sync wrote; the
/// filesystem is never asked.
struct Projects<'a> {
    handlers: &'a Handlers,
    avatars: HashMap<i64, Option<String>>,
    paths: HashMap<i64, Option<String>>,
}

impl<'a> Projects<'a> {
    fn new(handlers: &'a Handlers) -> Self {
        Self {
            handlers,
            avatars: HashMap::new(),
            paths: HashMap::new(),
        }
    }

    fn of(&mut self, project_id: i64) -> wire::ProjectInfo {
        let h = self.handlers;
        let path = self.paths.entry(project_id).or_insert_with(|| {
            let row = h.store().projects.get((project_id.max(0) as u64, 0));
            row.unwrap_or_else(|e| {
                warn!(error = %e, project_id, "project read failed");
                None
            })
            .map(|p| p.path_with_namespace)
        });
        wire::ProjectInfo {
            path: path.clone(),
            avatar: self.avatar(project_id),
        }
    }

    /// The absolute path of the project's avatar, `None` without one.
    fn avatar(&mut self, project_id: i64) -> Option<String> {
        let h = self.handlers;
        self.avatars
            .entry(project_id)
            .or_insert_with(|| {
                let row = h.store().avatars.get((project_id.max(0) as u64, 0));
                let avatar = row.unwrap_or_else(|e| {
                    warn!(error = %e, project_id, "avatar read failed");
                    None
                });
                avatar.filter(|a| !a.file.is_empty()).map(|a| {
                    let path = h.sync.avatars().path_of(&a.file);
                    path.to_string_lossy().into_owned()
                })
            })
            .clone()
    }
}

/// The full path of the epics' groups, each read at most once per request;
/// `None` without a row.
struct Groups<'a> {
    handlers: &'a Handlers,
    paths: HashMap<i64, Option<String>>,
}

impl<'a> Groups<'a> {
    fn new(handlers: &'a Handlers) -> Self {
        Self {
            handlers,
            paths: HashMap::new(),
        }
    }

    fn path_of(&mut self, group_id: i64) -> Option<String> {
        let h = self.handlers;
        self.paths
            .entry(group_id)
            .or_insert_with(|| {
                let row = h.store().groups.get((group_id.max(0) as u64, 0));
                row.unwrap_or_else(|e| {
                    warn!(error = %e, group_id, "group read failed");
                    None
                })
                .map(|g| g.full_path)
            })
            .clone()
    }
}

/// A search scope with at least one criterion: an item passes when it is in
/// any listed project or any listed group (subgroups included).
struct Scope {
    projects: Vec<i64>,
    groups: Vec<String>,
}

impl Scope {
    /// `None` when `scope` names nothing, which means no filter at all.
    fn new(scope: Option<SearchScope>) -> Option<Self> {
        let scope = scope?;
        let projects = scope.projects.unwrap_or_default();
        let groups = scope.groups.unwrap_or_default();
        (!projects.is_empty() || !groups.is_empty()).then_some(Self { projects, groups })
    }

    fn project(&self, id: i64) -> bool {
        self.projects.contains(&id)
    }

    fn group(&self, namespace: &str) -> bool {
        self.groups.iter().any(|g| in_group(namespace, g))
    }

    /// An issue or merge request: by its project, else by its URL's namespace.
    fn item(&self, project_id: i64, web_url: &str) -> bool {
        self.project(project_id) || (!self.groups.is_empty() && self.group(&namespace_of(web_url)))
    }
}

/// Where contribution events happened: their project and the link of the
/// issue or merge request they are about, each read at most once per
/// request. Unknown ones stay `None`.
struct Places<'a> {
    handlers: &'a Handlers,
    projects: HashMap<i64, Option<model::Project>>,
    items: HashMap<(bool, RowKey), Option<String>>,
}

impl<'a> Places<'a> {
    fn new(handlers: &'a Handlers) -> Self {
        Self {
            handlers,
            projects: HashMap::new(),
            items: HashMap::new(),
        }
    }

    /// The `web_url` of the stored issue or merge request `e` is about.
    fn item_url(&mut self, e: &model::Event) -> Option<String> {
        let (kind, iid) = e.target();
        let is_mr = match kind {
            // Work items share the issues' numbers.
            "Issue" | "WorkItem" => false,
            "MergeRequest" => true,
            _ => return None,
        };
        if e.project_id <= 0 || iid <= 0 {
            return None;
        }
        let key = (e.project_id as u64, iid as u64);
        let store = self.handlers.store();
        self.items
            .entry((is_mr, key))
            .or_insert_with(|| {
                let url = if is_mr {
                    store.merge_requests.get(key).map(|m| m.map(|m| m.web_url))
                } else {
                    store.issues.get(key).map(|i| i.map(|i| i.web_url))
                };
                url.unwrap_or_else(|e| {
                    warn!(error = %e, "item read failed; activity event left unlinked");
                    None
                })
            })
            .clone()
    }

    /// The wire event for `e`.
    fn wire(&mut self, e: model::Event) -> ActivityEvent {
        let item_url = self.item_url(&e);
        let store = self.handlers.store();
        let project = self.projects.entry(e.project_id).or_insert_with(|| {
            let row = store.projects.get((e.project_id.max(0) as u64, 0));
            row.unwrap_or_else(|err| {
                warn!(error = %err, project_id = e.project_id, "project read failed");
                None
            })
        });
        wire::activity(e, project.as_ref(), item_url)
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

/// Whether a work item or MR matches the search: case-insensitive
/// substring on the title or any label, or an exact reference query (`#iid`,
/// for an epic `&iid`).
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
/// view was fetched.
fn still_assigned(state: &str, assignees: &[model::UserRef], me: Option<i64>) -> bool {
    state == "opened" && assigned_to(assignees, me)
}

/// Whether `me` is among `assignees`. Rows without assignee data aren't
/// second-guessed.
fn assigned_to(assignees: &[model::UserRef], me: Option<i64>) -> bool {
    assignees.is_empty() || me.is_none_or(|me| assignees.iter().any(|a| a.id == me))
}

/// The row a write went to.
fn written_key(write: &Write) -> RowKey {
    (write.project_id.max(0) as u64, write.iid.max(0) as u64)
}

/// The job syncing the recent issues of `role`, and the view it fills.
fn recent_source(role: &WorkItemRole) -> (Job, &'static str) {
    match role {
        WorkItemRole::author => (Job::RecentAuthoredIssues, RECENT_AUTHORED_ISSUES),
        WorkItemRole::assignee => (Job::RecentAssignedIssues, RECENT_ASSIGNED_ISSUES),
    }
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
    async fn get_assigned_work_items(
        &self,
        call: &mut dyn Call_GetAssignedWorkItems,
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
        let mut projects = Projects::new(self);
        let mut epics = EpicLinks::new(self);
        let work_items: Vec<WorkItem> = rows
            .into_iter()
            .map(|i| {
                let open = open_count_of(usage.get(Issuable::Issue, i.project_id, i.iid));
                let project = projects.of(i.project_id);
                boards.wire(i, open, project, &mut epics)
            })
            .collect();
        debug!(count = work_items.len(), "serving assigned work items");
        call.reply(work_items)
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
        // Newest-updated first, as the interface promises: the picker shows
        // the reply in its order.
        rows.sort_by_key(|m| std::cmp::Reverse(m.updated_at));

        let usage = self.usage_or_empty();
        let mut projects = Projects::new(self);
        let mrs: Vec<MergeRequest> = rows
            .into_iter()
            .map(|m| {
                let open = open_count_of(usage.get(Issuable::MergeRequest, m.project_id, m.iid));
                let project = projects.of(m.project_id);
                wire::merge_request(m, open, project)
            })
            .collect();
        debug!(count = mrs.len(), "serving assigned merge requests");
        call.reply(mrs)
    }

    #[instrument(skip(self, call))]
    async fn list_work_items(
        &self,
        call: &mut dyn Call_ListWorkItems,
        role: Option<WorkItemRole>,
        updated_after: Option<i64>,
        states: Option<Vec<WorkItemState>>,
    ) -> varlink::Result<()> {
        let roles = match role {
            Some(role) => vec![role],
            None => vec![WorkItemRole::author, WorkItemRole::assignee],
        };
        for role in &roles {
            reply_if_cold!(self, call, recent_source(role).0, (Vec::new()));
        }
        let since = updated_after.map_or(0, |t| t.max(0) as u64);
        let states: Vec<&str> = states.iter().flatten().map(wire::issue_state).collect();

        // Each role's view with its own corrections, then every issue once.
        let mut seen = HashSet::new();
        let mut rows: Vec<model::Issue> = roles
            .iter()
            .flat_map(|role| self.recent_issues(role))
            .filter(|i| seen.insert((i.project_id, i.iid)))
            .filter(|i| i.updated_at >= since)
            .filter(|i| states.is_empty() || states.contains(&i.state.as_str()))
            .collect();
        // Newest-updated first; the project and number settle a tie.
        rows.sort_by_key(|i| (std::cmp::Reverse(i.updated_at), i.project_id, i.iid));

        let usage = self.usage_or_empty();
        let mut boards = BoardLabels::new(self);
        let mut projects = Projects::new(self);
        let mut epics = EpicLinks::new(self);
        let work_items: Vec<WorkItem> = rows
            .into_iter()
            .map(|i| {
                let open = open_count_of(usage.get(Issuable::Issue, i.project_id, i.iid));
                let project = projects.of(i.project_id);
                boards.wire(i, open, project, &mut epics)
            })
            .collect();
        debug!(count = work_items.len(), "serving recent work items");
        call.reply(work_items)
    }

    #[instrument(skip(self, call))]
    async fn search(
        &self,
        call: &mut dyn Call_Search,
        query: String,
        kinds: Option<Vec<SearchKind>>,
        limit: Option<i64>,
        scope: Option<SearchScope>,
        types: Option<Vec<String>>,
        exclude_types: Option<Vec<String>>,
    ) -> varlink::Result<()> {
        // An empty query is the "frequently opened" view: only work items and
        // MRs with recorded opens, ranked. Projects and groups have no open
        // counts, so they come back empty in that mode.
        let needle = query.trim().to_lowercase();
        let scope = Scope::new(scope);
        let frequent_only = needle.is_empty();
        let limit = match limit {
            None => DEFAULT_SEARCH_LIMIT,
            Some(n) if n > 0 => n as usize,
            Some(n) => return Invalid::new("limit", format!("invalid limit: {n}")).reply(call),
        };
        let kinds = kinds.unwrap_or_default();
        let want = |k: SearchKind| kinds.is_empty() || kinds.contains(&k);
        let types = types.unwrap_or_default();
        let excluded = exclude_types.unwrap_or_default();
        let listed = |list: &[String], t: &str| list.iter().any(|l| l.eq_ignore_ascii_case(t));
        let typed = |t: &str| (types.is_empty() || listed(&types, t)) && !listed(&excluded, t);

        reply_if_cold!(
            self,
            call,
            Job::MemberProjects,
            (Vec::new(), Vec::new(), Vec::new(), Vec::new())
        );

        let iid_query = parse_iid_query(&query);
        let usage = self.usage_or_empty();
        let mut project_info = Projects::new(self);

        // Issues and epics share one ranking and one limit.
        let mut work_items: Vec<WorkItem> = Vec::new();
        if want(SearchKind::work_items) {
            let epic_query = parse_epic_query(&query);
            let mut group_info = Groups::new(self);
            // Not read at all when only epics are asked for.
            let only_epics =
                !types.is_empty() && types.iter().all(|t| t.eq_ignore_ascii_case(wire::EPIC));
            let issues = if only_epics {
                Vec::new()
            } else {
                self.all::<model::Issue>()
            };
            let issues = issues
                .into_iter()
                .filter(|i| typed(i.work_item_type()))
                .filter(|i| search_item_matches(&needle, iid_query, &i.title, &i.labels, i.iid))
                .filter(|i| {
                    scope
                        .as_ref()
                        .is_none_or(|s| s.item(i.project_id, &i.web_url))
                })
                .map(|i| {
                    let u = usage.get(Issuable::Issue, i.project_id, i.iid);
                    (u, WorkItemRow::Issue(i))
                });
            let epics = if typed(wire::EPIC) {
                self.all::<model::Epic>()
            } else {
                Vec::new()
            };
            let epics = epics
                .into_iter()
                .filter(|e| search_item_matches(&needle, epic_query, &e.title, &e.labels, e.iid))
                .filter(|e| {
                    scope.as_ref().is_none_or(|s| {
                        s.group(&wire::group_path(
                            group_info.path_of(e.group_id),
                            &e.web_url,
                        ))
                    })
                })
                .map(|e| (usage.get_epic(e.group_id, e.iid), WorkItemRow::Epic(e)));
            let mut hits: Vec<(Option<UsageEntry>, WorkItemRow)> = issues
                .chain(epics)
                .filter(|(u, _)| !frequent_only || u.is_some())
                .collect();
            hits.sort_by_key(|(u, row)| rank_key(*u, row.updated_at()));
            hits.truncate(limit);
            let mut boards = BoardLabels::new(self);
            let mut epic_links = EpicLinks::new(self);
            work_items = hits
                .into_iter()
                .map(|(u, row)| match row {
                    WorkItemRow::Issue(i) => {
                        let project = project_info.of(i.project_id);
                        boards.wire(i, open_count_of(u), project, &mut epic_links)
                    }
                    WorkItemRow::Epic(e) => {
                        let group_path = group_info.path_of(e.group_id);
                        wire::epic(e, open_count_of(u), group_path)
                    }
                })
                .collect();
        }

        let mut merge_requests: Vec<MergeRequest> = Vec::new();
        if want(SearchKind::merge_requests) {
            let mut hits: Vec<(Option<UsageEntry>, model::MergeRequest)> = self
                .all::<model::MergeRequest>()
                .into_iter()
                .filter(|m| search_item_matches(&needle, iid_query, &m.title, &m.labels, m.iid))
                .filter(|m| {
                    scope
                        .as_ref()
                        .is_none_or(|s| s.item(m.project_id, &m.web_url))
                })
                .map(|m| (usage.get(Issuable::MergeRequest, m.project_id, m.iid), m))
                .filter(|(u, _)| !frequent_only || u.is_some())
                .collect();
            hits.sort_by_key(|(u, m)| rank_key(*u, m.updated_at));
            hits.truncate(limit);
            merge_requests = hits
                .into_iter()
                .map(|(u, m)| {
                    let project = project_info.of(m.project_id);
                    wire::merge_request(m, open_count_of(u), project)
                })
                .collect();
        }

        let mut projects: Vec<Project> = Vec::new();
        if want(SearchKind::projects) && !frequent_only {
            let mut hits = self.all::<model::Project>();
            hits.retain(|p| {
                (text_matches(&needle, &p.name) || text_matches(&needle, &p.path_with_namespace))
                    && scope
                        .as_ref()
                        .is_none_or(|s| s.project(p.id) || s.group(&p.path_with_namespace))
            });
            hits.sort_by(|a, b| a.path_with_namespace.cmp(&b.path_with_namespace));
            hits.truncate(limit);
            projects = hits
                .into_iter()
                .map(|p| {
                    let avatar = project_info.avatar(p.id);
                    wire::project(p, avatar)
                })
                .collect();
        }

        let mut groups: Vec<Group> = Vec::new();
        if want(SearchKind::groups) && !frequent_only {
            let mut hits = self.all::<model::Group>();
            hits.retain(|g| {
                (text_matches(&needle, &g.name) || text_matches(&needle, &g.full_path))
                    && scope.as_ref().is_none_or(|s| s.group(&g.full_path))
            });
            hits.sort_by(|a, b| a.full_path.cmp(&b.full_path));
            hits.truncate(limit);
            groups = hits.into_iter().map(wire::group).collect();
        }

        debug!(
            work_items = work_items.len(),
            merge_requests = merge_requests.len(),
            projects = projects.len(),
            groups = groups.len(),
            "serving search results"
        );
        call.reply(work_items, merge_requests, projects, groups)
    }

    #[instrument(skip(self, call))]
    async fn clear_cache(
        &self,
        call: &mut dyn Call_ClearCache,
        scope: Option<Vec<CacheScope>>,
    ) -> varlink::Result<()> {
        let scopes = scope.unwrap_or_default();

        let now = now_secs();
        let (quick_start, retention_start) = {
            let c = self.config.read().unwrap();
            (
                now.saturating_sub(c.refresh.quick.window().as_secs()),
                now.saturating_sub(c.history.retention().as_secs()),
            )
        };
        let timelogs = |from, until| Some(Clear::Timelogs { from, until });
        let mut clears = Vec::new();
        // Open statistics are user data, not a cache: only an explicit scope
        // clears them, never the "everything" default.
        let mut usage = false;
        if scopes.is_empty() {
            clears.push(Clear::Everything);
        }
        for scope in &scopes {
            let clear = match scope {
                CacheScope::assigned => Some(Clear::Assigned),
                CacheScope::search => Some(Clear::Corpus),
                CacheScope::quick => timelogs(quick_start, u64::MAX),
                CacheScope::slow => timelogs(retention_start, quick_start),
                CacheScope::stale => timelogs(0, retention_start),
                CacheScope::usage => {
                    usage = true;
                    None
                }
            };
            clears.extend(clear.filter(|c| !clears.contains(c)));
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

        if usage {
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
        if let Some(invalid) = issue_ref_error(project_id, iid) {
            return invalid.reply(call);
        }
        if !looks_like_duration(&duration) {
            let message = format!("invalid duration: {duration:?}");
            return Invalid::new("duration", message).reply(call);
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
                    .unzip();
                    events.push(HistoryEvent {
                        timestamp: p.queued_at_secs as i64,
                        source: HistorySource::queued,
                        kind: wire::kind(kind),
                        project_id,
                        iid,
                        title: title.and_then(wire::some),
                        web_url: web_url.and_then(wire::some),
                        time_spent: None,
                        duration: Some(duration),
                        summary: summary.and_then(wire::some),
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

    /// Never an error: a client asks it first, whatever the session is.
    #[instrument(skip(self, call))]
    async fn get_status(&self, call: &mut dyn Call_GetStatus) -> varlink::Result<()> {
        let api_version = forskap_api::API_VERSION.to_string();
        let daemon_version = env!("CARGO_PKG_VERSION").to_string();
        match self.current_session().await {
            Ok(s) => call.reply(
                api_version,
                daemon_version,
                true,
                None,
                None,
                Some(s.host),
                Some(s.username),
                Some(s.user_id),
            ),
            Err(e) => {
                let (reason, detail) = dormant_args(&e);
                call.reply(
                    api_version,
                    daemon_version,
                    false,
                    reason,
                    detail,
                    None,
                    None,
                    None,
                )
            }
        }
    }

    #[instrument(skip(self, call))]
    async fn get_activity(
        &self,
        call: &mut dyn Call_GetActivity,
        days: Option<i64>,
    ) -> varlink::Result<()> {
        reply_if_cold!(self, call, Job::Events, (Vec::new()));

        let days = days.unwrap_or(7).max(0) as u64;
        let cutoff = now_secs().saturating_sub(days.saturating_mul(86_400));
        let rows = self
            .store()
            .events
            .scan(RowScope::Since(cutoff))
            .unwrap_or_else(|e| {
                warn!(error = %e, "activity read failed; returning empty");
                Vec::new()
            });
        let mut places = Places::new(self);
        // Stored oldest first; reply newest first.
        let events: Vec<ActivityEvent> = rows.into_iter().rev().map(|e| places.wire(e)).collect();
        debug!(count = events.len(), "serving activity");
        call.reply(events)
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
            Ok(false) => call.reply_not_found(format!("no failed task with id {id}")),
            Err(e) => {
                warn!(error = %e, id, "retry_failure failed");
                reply_failed(call, &e, e.to_string())
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
            Ok(false) => call.reply_not_found(format!("no failed task with id {id}")),
            Err(e) => {
                warn!(error = %e, id, "dismiss_failure failed");
                reply_failed(call, &e, e.to_string())
            }
        }
    }

    #[instrument(skip(self, call))]
    async fn clear_failures(&self, call: &mut dyn Call_ClearFailures) -> varlink::Result<()> {
        if let Err(e) = self.queue.clear_failures() {
            warn!(error = %e, "clear_failures failed");
            return reply_failed(call, &e, e.to_string());
        }
        info!("cleared dead-letter queue");
        call.reply()
    }

    #[instrument(skip(self, call))]
    async fn record_open(
        &self,
        call: &mut dyn Call_RecordOpen,
        kind: IssuableKind,
        iid: i64,
        project_id: Option<i64>,
        group_id: Option<i64>,
    ) -> varlink::Result<()> {
        let key = match open_key(&kind, iid, project_id, group_id) {
            Ok(key) => key,
            Err(invalid) => return invalid.reply(call),
        };
        // Local bookkeeping only — no GitLab, so it works while dormant.
        let retention_secs = self.config.read().unwrap().usage.retention().as_secs();
        let now = now_secs();
        if let Err(e) = self
            .usage
            .record(&key, now, now.saturating_sub(retention_secs))
        {
            warn!(error = %e, key, "record_open failed");
            return reply_failed(call, &e, e.to_string());
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
        if let Some(invalid) = issue_ref_error(project_id, iid) {
            return invalid.reply(call);
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
        if let Some(invalid) = issue_ref_error(project_id, iid) {
            return invalid.reply(call);
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
        if let Some(invalid) = issue_ref_error(project_id, iid) {
            return invalid.reply(call);
        }
        let write = Write {
            kind: wire::internal_kind(&kind),
            project_id,
            iid,
            op: WriteOp::UnassignSelf,
        };
        reply_write!(call, self.perform_write(write).await)
    }

    /// Direct, never queued: a create has no target to address a replay by
    /// and no idempotency key, so repeating one that may have landed would
    /// file the issue twice.
    #[instrument(skip(self, call, description))]
    async fn create_work_item(
        &self,
        call: &mut dyn Call_CreateWorkItem,
        project_id: i64,
        title: String,
        description: Option<String>,
        labels: Option<Vec<String>>,
        assign_self: Option<bool>,
        parent: Option<WorkItemRef>,
    ) -> varlink::Result<()> {
        let labels = labels.unwrap_or_default();
        if let Some(invalid) = new_issue_error(project_id, &title, &labels) {
            return invalid.reply(call);
        }
        let parent = match parent.as_ref().map(parent_epic).transpose() {
            Ok(parent) => parent,
            Err(invalid) => return invalid.reply(call),
        };
        // Whatever keeps the session away: nothing is deferred.
        let session = match self.current_session().await {
            Ok(s) => s,
            Err(r) => {
                let (reason, detail) = dormant_args(&r);
                return call.reply_not_authenticated(reason, detail);
            }
        };
        // Resolved before the create, so a failed lookup creates nothing.
        let epic_id = match parent {
            None => None,
            Some((group_id, iid)) => {
                match self.legacy_epic_id(&*session.gitlab, group_id, iid).await {
                    Ok(id) => Some(id),
                    Err(e) => {
                        warn!(error = %e, group_id, iid, "looking up the parent epic failed");
                        let message =
                            format!("looking up the parent epic &{iid} of group {group_id}: {e}");
                        return reply_failed(call, &e, message);
                    }
                }
            }
        };
        let new = NewIssue {
            title,
            description,
            labels,
            assign_self: assign_self.unwrap_or(false),
            epic_id,
        };
        let created = match session.gitlab.create_issue(project_id, &new).await {
            Ok(created) => created,
            // Reported, not retried: GitLab may have created it all the
            // same. The sync worker stays the one to judge the session.
            Err(e) => {
                warn!(error = %e, project_id, "creating an issue failed");
                return reply_failed(call, &e, e.to_string());
            }
        };

        // The issue exists from here on: every path below replies success,
        // or the caller would create it again.
        let (iid, web_url) = match serde_json::from_value::<model::Issue>(created) {
            Ok(issue) => {
                info!(project_id, iid = issue.iid, "issue created");
                let iid = Some(issue.iid).filter(|&iid| iid > 0);
                let shown = (iid, wire::some(issue.web_url.clone()));
                let by = Identity {
                    host: session.host,
                    user_id: session.user_id,
                };
                let landed = self.sync.land_issue(issue, by);
                if tokio::time::timeout(LAND_TIMEOUT, landed).await.is_err() {
                    warn!(project_id, "the created issue isn't stored yet; replying");
                }
                shown
            }
            Err(e) => {
                warn!(error = %e, project_id, "issue created, but GitLab's answer is unreadable");
                (None, None)
            }
        };
        self.sync.refresh_soon(&Job::showing_issues_of(project_id));
        call.reply(iid, web_url)
    }

    #[instrument(skip(self, call, token))]
    async fn login(
        &self,
        call: &mut dyn Call_Login,
        host: String,
        token: String,
    ) -> varlink::Result<()> {
        // Without a keychain there is nowhere to keep the token: turned
        // down before GitLab is asked.
        if let Err(e) = self.keychain.require() {
            warn!("Login refused: no keychain");
            return reply_failed(call, &e, format!("logging in is disabled: {e}"));
        }
        let token = Token::new(token);
        let client = match GitlabClient::connect_with_retry(&host, &token).await {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, host, "Login: connecting to GitLab failed");
                return reply_failed(call, &e, e.to_string());
            }
        };
        let creds = Credentials {
            host: host.clone(),
            token,
        };
        if let Err(e) = self.keychain.store(&creds).await {
            warn!(error = %e, "Login: keychain write failed");
            return reply_failed(call, &e, format!("keychain write failed: {e}"));
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
        // Nothing to forget without a keychain: the session stays.
        if let Err(e) = self.keychain.require() {
            warn!("Logout refused: no keychain");
            return reply_failed(call, &e, format!("logging out is disabled: {e}"));
        }
        *self.session.write().await = ConnState::Dormant(DormancyReason::LoggedOut);
        self.rotation.reevaluate();
        if let Err(e) = self.keychain.delete().await {
            warn!(error = %e, "Logout: keychain delete failed");
            return reply_failed(call, &e, format!("keychain delete failed: {e}"));
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
                call.reply(
                    s.host,
                    s.user_id,
                    s.username,
                    token_expires_at,
                    token_rotates,
                )
            }
            Err(e) => {
                let (reason, detail) = dormant_args(&e);
                call.reply_not_authenticated(reason, detail)
            }
        }
    }
}
