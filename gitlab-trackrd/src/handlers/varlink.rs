//! The [`VarlinkInterface`] method implementations plus the write-path helpers
//! they lean on. Each method is a short cascade: consult the cache, fall back
//! to GitLab, reply — see the crate module docs for the error conventions.

use std::collections::HashMap;

use tracing::{debug, info, instrument, warn};

use gitlab_trackr_api::{
    Call_AssignSelf, Call_ClearCache, Call_ClearFailures, Call_Close, Call_DismissFailure,
    Call_GetAssignedIssues, Call_GetAssignedMergeRequests, Call_GetFailures, Call_GetHistory,
    Call_Login, Call_Logout, Call_PostTime, Call_RecordOpen, Call_RetryFailure, Call_Search,
    Call_UnassignSelf, Call_WhoAmI, FailedTask, Group, HistoryEvent, IssuableKind, Issue,
    MergeRequest, Project, VarlinkInterface,
};

use crate::cache::{in_group, namespace_of};
use crate::error::{DormancyReason, Error};
use crate::gitlab::{GitlabClient, Issuable};
use crate::history::HistoryCache;
use crate::search::{SEARCH_SCHEMA_VERSION, SearchIssue, SearchMr, parse_iid_query, text_matches};
use crate::secrets::{self, Credentials};
use crate::usage::{UsageEntry, UsageRecord};
use crate::write::{Write, WriteOp};

use super::refresh::graph_status_from;
use super::{
    ConnState, Handlers, Session, dormant_args, issue_ref_error, looks_like_duration, now_secs,
};

/// The kind strings `Search` accepts, matching the `ClearCache` scope style.
const SEARCH_KINDS: [&str; 4] = ["issues", "merge_requests", "projects", "groups"];

/// Per-kind result cap when the caller doesn't pass a `limit`.
const DEFAULT_SEARCH_LIMIT: usize = 50;

impl Handlers {
    /// The global numeric issuable ID (the one GraphQL embeds in
    /// `gid://gitlab/<Kind>/<id>`) for a cached `(project, iid)`, so a queued
    /// retry can use the GraphQL path. Issues resolve via the assigned-issue
    /// cache, MRs via the search corpus. `None` when not cached — the queue
    /// then falls back to REST without a `spent_at`.
    fn resolve_issuable_id(&self, kind: Issuable, project_id: i64, iid: i64) -> Option<i64> {
        match kind {
            Issuable::Issue => self.cache.issue_id(project_id, iid).ok().flatten(),
            Issuable::MergeRequest => self.search.mr_id(project_id, iid).ok().flatten(),
        }
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
                self.reflect(&write);
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

    /// Queue `write` for the retry worker and reflect it in the caches. A
    /// PostTime gets its issuable id resolved so the replay keeps its time.
    async fn defer(&self, mut write: Write) {
        if let WriteOp::PostTime { issuable_id, .. } = &mut write.op {
            *issuable_id = self.resolve_issuable_id(write.kind, write.project_id, write.iid);
        }
        self.reflect(&write);
        self.queue.enqueue(write).await;
    }

    /// Mirror an accepted write in the caches so the assigned views show it
    /// at once; the next sync reconciles.
    fn reflect(&self, write: &Write) {
        match write.op {
            WriteOp::Close => self.reflect_close(write.kind, write.project_id, write.iid),
            WriteOp::UnassignSelf => self.reflect_unassign(write.kind, write.project_id, write.iid),
            WriteOp::PostTime { .. } | WriteOp::AssignSelf => {}
        }
    }

    /// Reflect a close in the caches immediately: drop the issue from the
    /// assigned cache, or flip the cached MR's state so it leaves the
    /// assigned-MR view. Best-effort — the next refresh/sync reconciles.
    fn reflect_close(&self, kind: Issuable, project_id: i64, iid: i64) {
        match kind {
            Issuable::Issue => self.forget_cached_issue(project_id, iid),
            Issuable::MergeRequest => self.update_cached_mr(project_id, iid, "close", |m| {
                m.state = "closed".to_string();
            }),
        }
    }

    /// Reflect an unassign in the caches immediately: drop the issue, or
    /// remove the synced user from the cached MR's assignees.
    fn reflect_unassign(&self, kind: Issuable, project_id: i64, iid: i64) {
        match kind {
            Issuable::Issue => self.forget_cached_issue(project_id, iid),
            Issuable::MergeRequest => {
                let user = self
                    .search
                    .stamps()
                    .map(|s| s.synced_user_id)
                    .unwrap_or_default();
                if user == 0 {
                    return;
                }
                self.update_cached_mr(project_id, iid, "unassign", move |m| {
                    m.assignees.retain(|a| a.id != user);
                });
            }
        }
    }

    /// Apply a mutation to one cached search MR under the sync gate. Uses
    /// `try_begin_sync` — a write handler must never wait out an in-flight
    /// full resync; when the gate is contended the update is skipped, since
    /// the running sync is fetching fresh data anyway.
    fn update_cached_mr(
        &self,
        project_id: i64,
        iid: i64,
        what: &str,
        f: impl FnOnce(&mut SearchMr),
    ) {
        let Some(guard) = self.search.try_begin_sync() else {
            debug!(
                project_id,
                iid, what, "search sync in flight; skipping MR cache update"
            );
            return;
        };
        match guard.update_mr(project_id, iid, f) {
            Ok(true) => debug!(project_id, iid, what, "cached MR updated"),
            Ok(false) => {}
            Err(e) => warn!(error = %e, project_id, iid, what, "MR cache update failed"),
        }
    }

    /// Drop an issue from the assigned-issues cache so a close/unassign is
    /// reflected in `tt list` immediately. Best-effort — a failure is logged
    /// and swallowed (the next refresh will reconcile the list anyway).
    fn forget_cached_issue(&self, project_id: i64, iid: i64) {
        match self.cache.remove_issue(project_id, iid) {
            Ok(true) => debug!(project_id, iid, "removed issue from cache"),
            Ok(false) => {}
            Err(e) => warn!(error = %e, project_id, iid, "cache issue removal failed"),
        }
    }

    /// Map a cached search issue onto the wire `Issue`. `graph_status` is
    /// best-effort from already-cached board labels only — `Search` is a pure
    /// cache reader, so projects the assigned-issues refresh never touched
    /// simply get an empty status.
    fn wire_search_issue(&self, i: SearchIssue, open_count: i64) -> Issue {
        let board = self.boards.get(i.project_id).ok().flatten();
        let graph_status = graph_status_from(board.as_deref(), &i.labels, &i.state);
        Issue {
            id: i.id,
            iid: i.iid,
            project_id: i.project_id,
            title: i.title,
            web_url: i.web_url,
            state: i.state,
            parent: i.parent,
            total_time: i.total_time,
            graph_status,
            open_count,
        }
    }

    /// The open statistics, degraded to empty on a read failure so ranking
    /// merely falls back to recency (the standing cache-error convention).
    fn usage_or_empty(&self) -> UsageRecord {
        self.usage.snapshot().unwrap_or_else(|e| {
            warn!(error = %e, "usage read failed, ranking without open counts");
            UsageRecord::default()
        })
    }
}

/// Sort key for `Search` hits: most-opened first, then most recently opened,
/// then most recently updated — so never-opened items keep the old
/// newest-first order among themselves.
fn rank_key(usage: Option<UsageEntry>, updated_at_secs: u64) -> std::cmp::Reverse<(u64, u64, u64)> {
    let u = usage.unwrap_or_default();
    std::cmp::Reverse((u.count, u.last_opened_secs, updated_at_secs))
}

/// `open_count` for the wire, from an optional usage entry.
fn open_count_of(usage: Option<UsageEntry>) -> i64 {
    usage.map_or(0, |u| u.count as i64)
}

/// How [`Handlers::perform_write`] ended.
enum WriteOutcome {
    /// Applied, or queued for the retry worker.
    Accepted,
    NotAuthenticated(DormancyReason),
    Rejected(Error),
}

/// Reply to a write call from its [`WriteOutcome`]. A macro because every
/// method has its own generated call trait.
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

/// A cache read for one `Search` kind, degraded to empty on failure so the
/// daemon stays available (the standing cache-error convention).
fn read_or_empty<T>(result: crate::error::Result<Vec<T>>, kind: &str) -> Vec<T> {
    result.unwrap_or_else(|e| {
        warn!(error = %e, kind, "search cache read failed, treating as empty");
        Vec::new()
    })
}

/// Wire → internal issuable kind. The only place the generated enum's
/// lowercase variants are touched.
fn internal_kind(kind: &IssuableKind) -> Issuable {
    match kind {
        IssuableKind::issue => Issuable::Issue,
        IssuableKind::merge_request => Issuable::MergeRequest,
    }
}

/// Internal → wire issuable kind.
fn wire_kind(kind: Issuable) -> IssuableKind {
    match kind {
        Issuable::Issue => IssuableKind::issue,
        Issuable::MergeRequest => IssuableKind::merge_request,
    }
}

/// Map a cached search MR onto the wire `MergeRequest`; assignee usernames
/// come from the pairs captured at sync time.
fn wire_mr(m: SearchMr, open_count: i64) -> MergeRequest {
    MergeRequest {
        id: m.id,
        iid: m.iid,
        project_id: m.project_id,
        title: m.title,
        web_url: m.web_url,
        state: m.state,
        assignees: m.assignees.into_iter().map(|a| a.username).collect(),
        open_count,
    }
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

#[async_trait::async_trait]
impl VarlinkInterface for Handlers {
    #[instrument(skip(self, call))]
    async fn get_assigned_issues(
        &self,
        call: &mut dyn Call_GetAssignedIssues,
        groups: Option<Vec<String>>,
    ) -> varlink::Result<()> {
        let all = match self.cache.get() {
            Ok(Some(all)) => all,
            Ok(None) => {
                return match self.gitlab().await {
                    Ok(_) => call.reply(Vec::new()),
                    Err(e) => {
                        let (reason, detail) = dormant_args(&e);
                        call.reply_not_authenticated(reason, detail)
                    }
                };
            }
            Err(e) => {
                warn!("cache read failed, treating as empty: {e}");
                return call.reply(Vec::new());
            }
        };

        let mut issues = match groups {
            Some(groups) if !groups.is_empty() => {
                let mut seen = std::collections::HashSet::new();
                groups
                    .iter()
                    .flat_map(|g| self.cache.get_group(g).unwrap_or_default())
                    .filter(|i| seen.insert((i.project_id, i.iid)))
                    .collect()
            }
            _ => all,
        };
        // The persisted rows carry a placeholder; the live counts are overlaid
        // at read time so a `RecordOpen` shows up before the next refresh.
        let usage = self.usage_or_empty();
        for i in &mut issues {
            i.open_count = open_count_of(usage.get(Issuable::Issue, i.project_id, i.iid));
        }

        debug!(count = issues.len(), "serving issues from cache");
        call.reply(issues)
    }

    #[instrument(skip(self, call))]
    async fn get_assigned_merge_requests(
        &self,
        call: &mut dyn Call_GetAssignedMergeRequests,
        groups: Option<Vec<String>>,
    ) -> varlink::Result<()> {
        // Cold cache — never synced, or synced under a pre-assignee schema
        // (synced_user_id 0 covers both): mirror `get_assigned_issues` — an
        // honest NotAuthenticated while dormant, an empty reply while the
        // first (re)sync is pending.
        let stamps = self.search.stamps().unwrap_or_else(|e| {
            warn!("search stamp read failed, treating as never synced: {e}");
            Default::default()
        });
        if stamps.last_partial_sync_secs == 0
            || stamps.schema_version < SEARCH_SCHEMA_VERSION
            || stamps.synced_user_id == 0
        {
            return match self.gitlab().await {
                Ok(_) => call.reply(Vec::new()),
                Err(e) => {
                    let (reason, detail) = dormant_args(&e);
                    call.reply_not_authenticated(reason, detail)
                }
            };
        }

        let mut mine: Vec<SearchMr> = read_or_empty(self.search.all_mrs(), "merge requests")
            .into_iter()
            .filter(|m| {
                m.state == "opened" && m.assignees.iter().any(|a| a.id == stamps.synced_user_id)
            })
            .collect();
        if let Some(groups) = groups
            && !groups.is_empty()
        {
            mine.retain(|m| {
                let ns = namespace_of(&m.web_url);
                groups.iter().any(|g| in_group(&ns, g))
            });
        }
        // The wire type carries no timestamp, so order for the picker here.
        mine.sort_by_key(|m| std::cmp::Reverse(m.updated_at_secs));

        debug!(count = mine.len(), "serving assigned MRs from search cache");
        let usage = self.usage_or_empty();
        call.reply(
            mine.into_iter()
                .map(|m| {
                    let u = usage.get(Issuable::MergeRequest, m.project_id, m.iid);
                    wire_mr(m, open_count_of(u))
                })
                .collect(),
        )
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

        // Cold cache (never synced): mirror `get_assigned_issues` — an honest
        // NotAuthenticated while dormant, an empty reply while the first sync
        // is still pending.
        let never_synced = match self.search.stamps() {
            Ok(s) => s.last_partial_sync_secs == 0,
            Err(e) => {
                warn!("search stamp read failed, treating as never synced: {e}");
                true
            }
        };
        if never_synced {
            return match self.gitlab().await {
                Ok(_) => call.reply(Vec::new(), Vec::new(), Vec::new(), Vec::new()),
                Err(e) => {
                    let (reason, detail) = dormant_args(&e);
                    call.reply_not_authenticated(reason, detail)
                }
            };
        }

        let iid_query = parse_iid_query(&query);
        let usage = self.usage_or_empty();

        let mut issues: Vec<Issue> = Vec::new();
        if want("issues") {
            let mut hits: Vec<(Option<UsageEntry>, SearchIssue)> =
                read_or_empty(self.search.all_issues(), "issues")
                    .into_iter()
                    .filter(|i| search_item_matches(&needle, iid_query, &i.title, &i.labels, i.iid))
                    .map(|i| (usage.get(Issuable::Issue, i.project_id, i.iid), i))
                    .filter(|(u, _)| !frequent_only || u.is_some())
                    .collect();
            hits.sort_by_key(|(u, i)| rank_key(*u, i.updated_at_secs));
            hits.truncate(limit);
            issues = hits
                .into_iter()
                .map(|(u, i)| self.wire_search_issue(i, open_count_of(u)))
                .collect();
        }

        let mut merge_requests: Vec<MergeRequest> = Vec::new();
        if want("merge_requests") {
            let mut hits: Vec<(Option<UsageEntry>, SearchMr)> =
                read_or_empty(self.search.all_mrs(), "merge requests")
                    .into_iter()
                    .filter(|m| search_item_matches(&needle, iid_query, &m.title, &m.labels, m.iid))
                    .map(|m| (usage.get(Issuable::MergeRequest, m.project_id, m.iid), m))
                    .filter(|(u, _)| !frequent_only || u.is_some())
                    .collect();
            hits.sort_by_key(|(u, m)| rank_key(*u, m.updated_at_secs));
            hits.truncate(limit);
            merge_requests = hits
                .into_iter()
                .map(|(u, m)| wire_mr(m, open_count_of(u)))
                .collect();
        }

        let mut projects: Vec<Project> = Vec::new();
        if want("projects") && !frequent_only {
            let mut hits = read_or_empty(self.search.all_projects(), "projects");
            hits.retain(|p| text_matches(&needle, &p.name) || text_matches(&needle, &p.path));
            hits.sort_by(|a, b| a.path.cmp(&b.path));
            hits.truncate(limit);
            projects = hits
                .into_iter()
                .map(|p| Project {
                    id: p.id,
                    name: p.name,
                    path: p.path,
                    web_url: p.web_url,
                })
                .collect();
        }

        let mut groups: Vec<Group> = Vec::new();
        if want("groups") && !frequent_only {
            let mut hits = read_or_empty(self.search.all_groups(), "groups");
            hits.retain(|g| text_matches(&needle, &g.name) || text_matches(&needle, &g.path));
            hits.sort_by(|a, b| a.path.cmp(&b.path));
            hits.truncate(limit);
            groups = hits
                .into_iter()
                .map(|g| Group {
                    id: g.id,
                    name: g.name,
                    path: g.path,
                    web_url: g.web_url,
                })
                .collect();
        }

        debug!(
            issues = issues.len(),
            merge_requests = merge_requests.len(),
            projects = projects.len(),
            groups = groups.len(),
            "serving search results from cache"
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

        if want("issues") {
            if let Err(e) = self.cache.clear() {
                warn!("issue cache clear failed: {e}");
            } else {
                info!("issue cache cleared");
            }
            if let Err(e) = self.boards.clear() {
                warn!("board cache clear failed: {e}");
            } else {
                info!("board cache cleared");
            }
            // Zeroed so the next quick tick actually refetches — a fresh stamp
            // would serve the just-cleared cache as stale-empty until the
            // interval elapses.
            if let Err(e) = self.refresh_meta.update(|s| s.last_quick_sync_secs = 0) {
                warn!("refresh stamp reset failed: {e}");
            }
        }

        if want("search") {
            // begin_sync waits out an in-flight sync, so its final
            // set_stamps can't stamp "synced" over the half-wiped corpus.
            // The guard must drop before the refill sync below, whose
            // try_begin_sync would otherwise lose and silently skip.
            let guard = self.search.begin_sync().await;
            if let Err(e) = guard.clear() {
                warn!("search cache clear failed: {e}");
            } else {
                info!("search cache cleared; next sync will be full");
            }
        }

        let now = now_secs();
        let (quick_secs, slow_secs) = {
            let c = self.config.read().unwrap();
            (
                c.refresh.quick.window().as_secs(),
                c.refresh.slow.window().as_secs(),
            )
        };
        let quick_start = now.saturating_sub(quick_secs);
        let slow_start = now.saturating_sub(slow_secs);

        if all {
            if let Err(e) = self.history.clear() {
                warn!("history clear failed: {e}");
            } else {
                info!("history cleared");
            }
            if let Err(e) = self.refresh_meta.clear() {
                warn!("refresh stamp clear failed: {e}");
            }
        } else {
            if want("quick") {
                clear_band(&self.history, quick_start, u64::MAX, "quick");
            }
            if want("slow") {
                clear_band(&self.history, slow_start, quick_start, "slow");
            }
            if want("stale") {
                clear_band(&self.history, 0, slow_start, "stale");
            }
            if want("quick") || want("slow") || want("stale") {
                // The refill below repopulates immediately when connected; the
                // zeroed slow stamp covers the dormant case, so the cleared
                // band is refetched at the next opportunity instead of being
                // stamped over as fresh.
                if let Err(e) = self.refresh_meta.update(|s| {
                    s.last_slow_sync_secs = 0;
                    if want("stale") {
                        s.backfilled_retention_hours = 0;
                    }
                }) {
                    warn!("refresh stamp reset failed: {e}");
                }
            }
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

        if let Ok(gitlab) = self.gitlab().await {
            if all {
                self.warm_up().await;
            } else {
                if want("search") {
                    // The clear above zeroed the stamps, so this runs full.
                    self.sync_search_cache().await;
                }
                if want("stale") {
                    let retention = self.config.read().unwrap().history.retention();
                    let _ = self.refresh_history_window(&gitlab, retention).await;
                    self.prune_history();
                } else if want("slow") {
                    let slow_window = self.config.read().unwrap().refresh.slow.window();
                    let _ = self.refresh_history_window(&gitlab, slow_window).await;
                } else if want("quick") {
                    let quick_window = self.config.read().unwrap().refresh.quick.window();
                    let _ = self.refresh_history_window(&gitlab, quick_window).await;
                }
            }
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
            kind: internal_kind(&kind),
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

        let cached_issues = self.cache.get().ok().flatten().unwrap_or_default();
        let by_key: HashMap<(i64, i64), &Issue> = cached_issues
            .iter()
            .map(|i| ((i.project_id, i.iid), i))
            .collect();

        let mut events: Vec<HistoryEvent> = Vec::new();

        match self.queue.pending() {
            Ok(pending) => {
                let posts: Vec<_> = pending
                    .into_iter()
                    .filter(|p| matches!(p.write.op, WriteOp::PostTime { .. }))
                    .collect();
                // Title/url joins for queued MR entries come from the search
                // corpus; only scanned when an MR is actually pending.
                let mrs = if posts.iter().any(|p| p.write.kind == Issuable::MergeRequest) {
                    read_or_empty(self.search.all_mrs(), "merge requests")
                } else {
                    Vec::new()
                };
                let mr_by_key: HashMap<(i64, i64), &SearchMr> =
                    mrs.iter().map(|m| ((m.project_id, m.iid), m)).collect();

                for p in posts {
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
                    let (title, web_url) = match kind {
                        Issuable::Issue => {
                            let issue = by_key.get(&(project_id, iid));
                            (
                                issue.map(|i| i.title.clone()).unwrap_or_default(),
                                issue.map(|i| i.web_url.clone()).unwrap_or_default(),
                            )
                        }
                        Issuable::MergeRequest => {
                            let mr = mr_by_key.get(&(project_id, iid));
                            (
                                mr.map(|m| m.title.clone()).unwrap_or_default(),
                                mr.map(|m| m.web_url.clone()).unwrap_or_default(),
                            )
                        }
                    };
                    events.push(HistoryEvent {
                        timestamp: p.queued_at_secs as i64,
                        source: "queued".to_string(),
                        kind: wire_kind(kind),
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

        match self.history.all_since(cutoff) {
            Ok(entries) => {
                for e in entries {
                    events.push(HistoryEvent {
                        timestamp: e.spent_at_secs as i64,
                        source: "gitlab".to_string(),
                        kind: wire_kind(e.kind),
                        project_id: e.project_id,
                        iid: e.iid,
                        title: e.title,
                        web_url: e.web_url,
                        duration: e.duration,
                        summary: e.summary,
                    });
                }
            }
            Err(e) => warn!(error = %e, "history read failed; returning queued only"),
        }

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
                kind: wire_kind(f.kind),
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
        let key = crate::usage::usage_key(internal_kind(&kind), project_id, iid);
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
            kind: internal_kind(&kind),
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
            kind: internal_kind(&kind),
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
            kind: internal_kind(&kind),
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
        call.reply()
    }

    #[instrument(skip(self, call))]
    async fn logout(&self, call: &mut dyn Call_Logout) -> varlink::Result<()> {
        *self.session.write().await = ConnState::Dormant(DormancyReason::LoggedOut);
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
            Ok(s) => call.reply(s.host, s.user_id),
            Err(e) => {
                let (reason, detail) = dormant_args(&e);
                call.reply_not_authenticated(reason, detail)
            }
        }
    }
}

/// Clear one history tier's `spent_at` band, logging the outcome.
fn clear_band(history: &HistoryCache, min_secs: u64, max_secs: u64, tier: &str) {
    match history.clear_between(min_secs, max_secs) {
        Ok(n) => info!(removed = n, tier, "history tier cleared"),
        Err(e) => warn!(error = %e, tier, "history tier clear failed"),
    }
}
