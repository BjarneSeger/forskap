use super::*;

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Notify, RwLock};

use forskap_api::{
    AsyncCall, CacheScope, Call_AssignSelf, Call_ClearCache, Call_Close, Call_CreateWorkItem,
    Call_DismissFailure, Call_GetActivity, Call_GetAssignedMergeRequests,
    Call_GetAssignedWorkItems, Call_GetHistory, Call_GetStatus, Call_GetSyncJobs,
    Call_ListWorkItems, Call_Login, Call_Logout, Call_PostTime, Call_RecordOpen, Call_RetryFailure,
    Call_Search, Call_UnassignSelf, Call_WhoAmI, CreateWorkItem_Reply, GetActivity_Reply,
    GetAssignedMergeRequests_Reply, GetAssignedWorkItems_Reply, GetHistory_Reply, GetStatus_Reply,
    GetSyncJobs_Reply, HistorySource, IssuableKind, ListWorkItems_Reply, MergeRequest,
    Search_Reply, SearchKind, SearchScope, SyncJobStatus, VarlinkInterface, WhoAmI_Reply, WorkItem,
    WorkItemRef, WorkItemRole, WorkItemState,
};

use crate::config::SharedConfig;
use crate::error::DormancyReason;
use crate::gitlab::{Issuable, NewIssue};
use crate::queue::RetryQueue;
use crate::sync::avatars::Avatar;
use crate::sync::jobs::{
    ASSIGNED_ISSUES, ASSIGNED_MERGE_REQUESTS, RECENT_ASSIGNED_ISSUES, RECENT_AUTHORED_ISSUES,
};
use crate::sync::model::{self, Board, BoardList, LabelRef, RowKey, UserRef};
use crate::sync::schedule::JobState;
use crate::sync::store::{RowScope, Stored, SyncStore, View};
use crate::sync::{Job, SyncHandle};
use crate::testing::{
    FakeErr, FakeGitlab, epic_json, epic_path, event_json, eventually, issue_json,
};
use crate::usage::{epic_usage_key, usage_key};
use crate::write::{Write, WriteOp};

const NOT_AUTHENTICATED: &str = "org.thehoster.forskapd.NotAuthenticated";
const GITLAB_ERROR: &str = "org.thehoster.forskapd.GitlabError";
const GITLAB_UNAVAILABLE: &str = "org.thehoster.forskapd.GitlabUnavailable";
const INVALID_ARGUMENT: &str = "org.thehoster.forskapd.InvalidArgument";
const NOT_FOUND: &str = "org.thehoster.forskapd.NotFound";
const INTERNAL: &str = "org.thehoster.forskapd.Internal";

// ── Scaffolding ────────────────────────────────────────────────────────

/// `Handlers` around `state` on a fresh temp database. The sync worker runs
/// only demanded jobs, so a test controls when GitLab is read; the store is
/// seeded directly.
fn handlers_with(state: ConnState) -> (Handlers, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = fjall::Database::builder(dir.path().join("db"))
        .open()
        .unwrap();
    let session: SessionSlot = Arc::new(RwLock::new(state));
    let config: SharedConfig = Arc::new(std::sync::RwLock::new(crate::config::defaults()));
    let reconnect_signal = Arc::new(Notify::new());
    let sync = SyncHandle::spawn_on_demand(
        Arc::new(SyncStore::open(&db).unwrap()),
        crate::sync::AvatarDir::new(dir.path().join("avatars")),
        Arc::clone(&session),
        Arc::clone(&config),
        Arc::clone(&reconnect_signal),
    );
    let queue = RetryQueue::new(Arc::clone(&session), &db, Arc::clone(&config)).unwrap();
    (
        Handlers {
            session,
            sync,
            usage: Arc::new(crate::usage::UsageStats::open(&db).unwrap()),
            queue,
            config,
            reconnect_signal,
            rotation: Default::default(),
            // No test reaches the OS keychain.
            keychain: crate::secrets::Keychain::disabled(),
        },
        dir,
    )
}

/// No credentials. `pub(crate)` for the `service.rs` dispatch tests.
pub(crate) fn dormant_handlers() -> (Handlers, tempfile::TempDir) {
    handlers_with(ConnState::Dormant(DormancyReason::NoCredentials))
}

fn unreachable_handlers() -> (Handlers, tempfile::TempDir) {
    handlers_with(ConnState::Dormant(DormancyReason::Unreachable {
        host: "gitlab.test".into(),
        detail: "connection refused".into(),
    }))
}

fn connected_handlers(fake: &Arc<FakeGitlab>) -> (Handlers, tempfile::TempDir) {
    handlers_with(ConnState::Connected(Session {
        gitlab: Arc::clone(fake) as Arc<dyn crate::gitlab::GitlabApi>,
        host: "gitlab.test".into(),
        user_id: 1,
        username: "tester".into(),
        token: Default::default(),
    }))
}

fn seed<R: Stored>(h: &Handlers, rows: &[R]) {
    let mut c = h.sync.store().begin();
    c.upsert(rows).unwrap();
    c.commit().unwrap();
}

fn seed_view(h: &Handlers, name: &str, keys: &[RowKey], fetched_at: u64) {
    let mut c = h.sync.store().begin();
    c.set_view(
        name,
        &View {
            keys: keys.to_vec(),
            fetched_at,
        },
    )
    .unwrap();
    c.commit().unwrap();
}

/// Mark `jobs` as synced a minute ago, so reads treat their data as warm.
fn mark_synced(h: &Handlers, jobs: &[Job]) {
    let at = now_secs() - 60;
    let mut c = h.sync.store().begin();
    for job in jobs {
        c.set_job(
            &job.key(),
            &JobState {
                last_ok: at,
                last_full: at,
                ..Default::default()
            },
        )
        .unwrap();
    }
    c.commit().unwrap();
}

fn issue(project_id: i64, iid: i64, title: &str, web_url: &str) -> model::Issue {
    model::Issue {
        id: project_id * 1000 + iid,
        iid,
        project_id,
        title: title.into(),
        web_url: web_url.into(),
        state: "opened".into(),
        ..Default::default()
    }
}

fn mr(
    project_id: i64,
    iid: i64,
    title: &str,
    web_url: &str,
    updated_at: u64,
) -> model::MergeRequest {
    model::MergeRequest {
        id: project_id * 1000 + iid,
        iid,
        project_id,
        title: title.into(),
        web_url: web_url.into(),
        state: "opened".into(),
        assignees: vec![UserRef {
            id: 1,
            username: "me".into(),
        }],
        updated_at,
        ..Default::default()
    }
}

/// An epic of `team`, its legacy id `group_id * 1000 + iid`, its work item
/// id that plus 900 000.
fn epic(group_id: i64, iid: i64, title: &str, updated_at: u64) -> model::Epic {
    model::Epic {
        id: group_id * 1000 + iid,
        iid,
        group_id,
        work_item_id: 900_000 + group_id * 1000 + iid,
        title: title.into(),
        web_url: format!("https://gl/groups/team/-/epics/{iid}"),
        state: "opened".into(),
        updated_at,
        ..Default::default()
    }
}

/// Three assigned issues: two under `team` (one in a subgroup), one under
/// `other`, listed in that GitLab order.
fn seed_assigned_issues(h: &Handlers) {
    seed(
        h,
        &[
            issue(2, 3, "other", "https://gl/other/x/-/issues/3"),
            issue(1, 1, "api", "https://gl/team/api/-/issues/1"),
            issue(1, 2, "web", "https://gl/team/sub/web/-/issues/2"),
        ],
    );
    seed_view(
        h,
        ASSIGNED_ISSUES,
        &[(2, 3), (1, 1), (1, 2)],
        now_secs() - 60,
    );
    mark_synced(h, &[Job::AssignedIssues]);
}

fn seed_assigned_mrs(h: &Handlers) {
    seed(
        h,
        &[
            mr(
                1,
                10,
                "older",
                "https://gl/team/api/-/merge_requests/10",
                100,
            ),
            mr(
                2,
                11,
                "newer",
                "https://gl/other/x/-/merge_requests/11",
                200,
            ),
        ],
    );
    seed_view(
        h,
        ASSIGNED_MERGE_REQUESTS,
        &[(1, 10), (2, 11)],
        now_secs() - 60,
    );
    mark_synced(h, &[Job::AssignedMergeRequests]);
}

/// The recent views, a minute old: authored are 1/1 (updated at 300) and the
/// closed 1/2 (at 200), assigned are 1/2 again and 2/3 (at 100).
fn seed_recent_issues(h: &Handlers) {
    let at = |mut issue: model::Issue, updated_at, state: &str| {
        issue.updated_at = updated_at;
        issue.state = state.into();
        issue
    };
    let (api, web, other) = (
        issue(1, 1, "api", "https://gl/team/api/-/issues/1"),
        issue(1, 2, "web", "https://gl/team/api/-/issues/2"),
        issue(2, 3, "other", "https://gl/other/x/-/issues/3"),
    );
    seed(
        h,
        &[
            at(api, 300, "opened"),
            at(web, 200, "closed"),
            at(other, 100, "opened"),
        ],
    );
    seed_view(
        h,
        RECENT_AUTHORED_ISSUES,
        &[(1, 2), (1, 1)],
        now_secs() - 60,
    );
    seed_view(
        h,
        RECENT_ASSIGNED_ISSUES,
        &[(2, 3), (1, 2)],
        now_secs() - 60,
    );
    mark_synced(h, &[Job::RecentAuthoredIssues, Job::RecentAssignedIssues]);
}

/// Issues "OAuth token refresh" (1/10) and a labeled one (1/20), MR "Fix
/// oauth flow" (1/30), project `team/auth-service`, group `team` with the
/// epics "Identity roadmap" (5/7) and "Billing" (5/8).
pub(crate) fn seed_corpus(h: &Handlers) {
    let mut labeled = issue(1, 20, "unrelated title", "https://gl/team/p/-/issues/20");
    labeled.labels = vec!["Backend".into()];
    labeled.updated_at = 200;
    let mut oauth = issue(
        1,
        10,
        "OAuth token refresh",
        "https://gl/team/p/-/issues/10",
    );
    oauth.updated_at = 100;
    seed(h, &[oauth, labeled]);
    seed(
        h,
        &[mr(
            1,
            30,
            "Fix oauth flow",
            "https://gl/team/p/-/merge_requests/30",
            50,
        )],
    );
    seed(
        h,
        &[model::Project {
            id: 4,
            name: "auth-service".into(),
            path_with_namespace: "team/auth-service".into(),
            web_url: "https://gl/team/auth-service".into(),
            ..Default::default()
        }],
    );
    seed(
        h,
        &[model::Group {
            id: 5,
            name: "team".into(),
            full_path: "team".into(),
            web_url: "https://gl/team".into(),
        }],
    );
    seed(
        h,
        &[
            epic(5, 7, "Identity roadmap", 100),
            epic(5, 8, "Billing", 200),
        ],
    );
    mark_synced(h, &[Job::MemberProjects]);
}

fn reply<T: serde::de::DeserializeOwned>(call: &mut AsyncCall) -> T {
    let reply = call.take_reply().expect("a reply");
    assert!(
        reply.error.is_none(),
        "expected success, got {:?}",
        reply.error
    );
    serde_json::from_value(reply.parameters.expect("parameters")).expect("parse reply")
}

fn reply_error(call: &mut AsyncCall) -> Option<String> {
    reply_error_with(call).map(|(name, _)| name)
}

/// The error a call replied and its parameters, `None` for a success.
fn reply_error_with(call: &mut AsyncCall) -> Option<(String, serde_json::Value)> {
    let reply = call.take_reply().expect("a reply");
    Some((
        reply.error?.to_string(),
        reply.parameters.unwrap_or_default(),
    ))
}

/// The argument an `InvalidArgument` reply names; panics on any other reply.
fn invalid_argument(call: &mut AsyncCall) -> String {
    match reply_error_with(call) {
        Some((name, args)) if name == INVALID_ARGUMENT => {
            assert!(args["message"].is_string(), "{args}");
            args["argument"].as_str().unwrap().to_string()
        }
        other => panic!("expected InvalidArgument, got {other:?}"),
    }
}

/// The status a `GitlabError` reply carries; panics on any other reply.
fn gitlab_status(call: &mut AsyncCall) -> Option<i64> {
    match reply_error_with(call) {
        Some((name, args)) if name == GITLAB_ERROR => {
            args.get("status").map(|s| s.as_i64().unwrap())
        }
        other => panic!("expected GitlabError, got {other:?}"),
    }
}

async fn assigned_work_items(h: &Handlers, groups: Option<Vec<String>>) -> Vec<WorkItem> {
    let mut call = AsyncCall::default();
    h.get_assigned_work_items(&mut call as &mut dyn Call_GetAssignedWorkItems, groups)
        .await
        .unwrap();
    reply::<GetAssignedWorkItems_Reply>(&mut call).work_items
}

async fn list_work_items(
    h: &Handlers,
    role: Option<WorkItemRole>,
    updated_after: Option<i64>,
    states: Option<Vec<WorkItemState>>,
) -> Vec<WorkItem> {
    let mut call = AsyncCall::default();
    h.list_work_items(
        &mut call as &mut dyn Call_ListWorkItems,
        role,
        updated_after,
        states,
    )
    .await
    .unwrap();
    reply::<ListWorkItems_Reply>(&mut call).work_items
}

/// The error `ListWorkItems` replies for `role`, `None` for a success.
async fn list_work_items_error(h: &Handlers, role: Option<WorkItemRole>) -> Option<String> {
    let mut call = AsyncCall::default();
    h.list_work_items(&mut call as &mut dyn Call_ListWorkItems, role, None, None)
        .await
        .unwrap();
    reply_error(&mut call)
}

fn iids(items: &[WorkItem]) -> Vec<i64> {
    items.iter().map(|i| i.iid).collect()
}

fn is_epic(item: &WorkItem) -> bool {
    item.r#type == "epic"
}

/// The numbers of the project work items `Search` found, in its order.
fn issue_iids(r: &Search_Reply) -> Vec<i64> {
    r.work_items
        .iter()
        .filter(|w| !is_epic(w))
        .map(|w| w.iid)
        .collect()
}

/// The epics `Search` found, in its order.
fn epics(r: &Search_Reply) -> Vec<&WorkItem> {
    r.work_items.iter().filter(|w| is_epic(w)).collect()
}

/// The `(group_id, iid)` of the epics `Search` found, in its order.
fn epic_keys(r: &Search_Reply) -> Vec<(i64, i64)> {
    epics(r)
        .iter()
        .map(|e| (e.group_id.unwrap(), e.iid))
        .collect()
}

async fn unassign(h: &Handlers, project_id: i64, iid: i64, kind: IssuableKind) -> Option<String> {
    let mut call = AsyncCall::default();
    h.unassign_self(
        &mut call as &mut dyn Call_UnassignSelf,
        project_id,
        iid,
        kind,
    )
    .await
    .unwrap();
    reply_error(&mut call)
}

async fn assigned_mrs(h: &Handlers, groups: Option<Vec<String>>) -> Vec<MergeRequest> {
    let mut call = AsyncCall::default();
    h.get_assigned_merge_requests(&mut call as &mut dyn Call_GetAssignedMergeRequests, groups)
        .await
        .unwrap();
    reply::<GetAssignedMergeRequests_Reply>(&mut call).merge_requests
}

async fn run_search(
    h: &Handlers,
    query: &str,
    kinds: Option<Vec<SearchKind>>,
    limit: Option<i64>,
) -> Search_Reply {
    run_scoped_search(h, query, kinds, limit, None).await
}

async fn run_scoped_search(
    h: &Handlers,
    query: &str,
    kinds: Option<Vec<SearchKind>>,
    limit: Option<i64>,
    scope: Option<SearchScope>,
) -> Search_Reply {
    search_with(h, query, kinds, limit, scope, None, None).await
}

/// `Search` for the work items of `types` only.
async fn run_typed_search(
    h: &Handlers,
    query: &str,
    types: &[&str],
    limit: Option<i64>,
) -> Search_Reply {
    run_filtered_search(h, query, types, &[], limit).await
}

/// `Search` for the work items of `types` (empty: any) but not of `excluded`.
async fn run_filtered_search(
    h: &Handlers,
    query: &str,
    types: &[&str],
    excluded: &[&str],
    limit: Option<i64>,
) -> Search_Reply {
    let names = |list: &[&str]| Some(list.iter().map(|t| t.to_string()).collect());
    let kinds = Some(vec![SearchKind::work_items]);
    search_with(h, query, kinds, limit, None, names(types), names(excluded)).await
}

async fn search_with(
    h: &Handlers,
    query: &str,
    kinds: Option<Vec<SearchKind>>,
    limit: Option<i64>,
    scope: Option<SearchScope>,
    types: Option<Vec<String>>,
    exclude_types: Option<Vec<String>>,
) -> Search_Reply {
    let mut call = AsyncCall::default();
    h.search(
        &mut call as &mut dyn Call_Search,
        query.to_string(),
        kinds,
        limit,
        scope,
        types,
        exclude_types,
    )
    .await
    .unwrap();
    reply(&mut call)
}

/// `RecordOpen` of a project's work item or merge request.
async fn run_record_open(h: &Handlers, project_id: i64, iid: i64, kind: IssuableKind) {
    let error = record_open(h, kind, iid, Some(project_id), None).await;
    assert_eq!(error, None);
}

/// The error `RecordOpen` replies, `None` for a success.
async fn record_open(
    h: &Handlers,
    kind: IssuableKind,
    iid: i64,
    project_id: Option<i64>,
    group_id: Option<i64>,
) -> Option<String> {
    let mut call = AsyncCall::default();
    h.record_open(
        &mut call as &mut dyn Call_RecordOpen,
        kind,
        iid,
        project_id,
        group_id,
    )
    .await
    .unwrap();
    reply_error(&mut call)
}

async fn post_time(h: &Handlers, project_id: i64, iid: i64, kind: IssuableKind) -> Option<String> {
    let mut call = AsyncCall::default();
    h.post_time(
        &mut call as &mut dyn Call_PostTime,
        project_id,
        iid,
        kind,
        "30m".to_string(),
        None,
    )
    .await
    .unwrap();
    reply_error(&mut call)
}

async fn close(h: &Handlers, project_id: i64, iid: i64, kind: IssuableKind) -> Option<String> {
    let mut call = AsyncCall::default();
    h.close(&mut call as &mut dyn Call_Close, project_id, iid, kind)
        .await
        .unwrap();
    reply_error(&mut call)
}

/// `CreateWorkItem` with just a title, self-assigned or not; the call holds
/// the reply.
async fn create_work_item(
    h: &Handlers,
    project_id: i64,
    title: &str,
    assign_self: Option<bool>,
) -> AsyncCall {
    create_work_item_with(h, project_id, title, None, None, assign_self, None).await
}

async fn create_work_item_with(
    h: &Handlers,
    project_id: i64,
    title: &str,
    description: Option<&str>,
    labels: Option<&[&str]>,
    assign_self: Option<bool>,
    parent: Option<WorkItemRef>,
) -> AsyncCall {
    let mut call = AsyncCall::default();
    h.create_work_item(
        &mut call as &mut dyn Call_CreateWorkItem,
        project_id,
        title.to_string(),
        description.map(str::to_string),
        labels.map(|labels| labels.iter().map(|l| l.to_string()).collect()),
        assign_self,
        parent,
    )
    .await
    .unwrap();
    call
}

/// The epic `iid` of the group as a parent, named by its group and number.
fn parent(group_id: i64, iid: i64) -> WorkItemRef {
    WorkItemRef {
        project_id: None,
        group_id: Some(group_id),
        iid,
        r#type: Some("epic".into()),
        title: None,
        web_url: None,
    }
}

/// `CreateWorkItem` of "Fix it" in project 7 under `parent`.
async fn create_under(h: &Handlers, parent: WorkItemRef) -> AsyncCall {
    create_work_item_with(h, 7, "Fix it", None, None, None, Some(parent)).await
}

/// The issue GitLab answers a create in project 7 with, numbered `iid` and
/// assigned to the users `assignees`.
fn created_json(iid: i64, title: &str, assignees: &[i64]) -> serde_json::Value {
    let mut row = issue_json(7, iid, title);
    row["assignees"] = assignees
        .iter()
        .map(|id| serde_json::json!({"id": id, "username": format!("user{id}")}))
        .collect();
    row
}

async fn clear_cache(h: &Handlers, scope: Option<Vec<CacheScope>>) {
    let mut call = AsyncCall::default();
    h.clear_cache(&mut call as &mut dyn Call_ClearCache, scope)
        .await
        .unwrap();
    assert_eq!(reply_error(&mut call), None);
}

async fn history(h: &Handlers, days: Option<i64>) -> Vec<forskap_api::HistoryEvent> {
    let mut call = AsyncCall::default();
    h.get_history(&mut call as &mut dyn Call_GetHistory, days)
        .await
        .unwrap();
    reply::<GetHistory_Reply>(&mut call).events
}

async fn activity(h: &Handlers, days: Option<i64>) -> Vec<forskap_api::ActivityEvent> {
    let mut call = AsyncCall::default();
    h.get_activity(&mut call as &mut dyn Call_GetActivity, days)
        .await
        .unwrap();
    reply::<GetActivity_Reply>(&mut call).events
}

// ── Validators ─────────────────────────────────────────────────────────

#[test]
fn looks_like_duration_accepts_valid_and_rejects_typos() {
    for ok in ["30m", "1h30m", "1.5h", "2d", "1w", "1mo", "1h 30m"] {
        assert!(looks_like_duration(ok), "{ok}");
    }
    for bad in ["", "   ", "abc", "1x", "h"] {
        assert!(!looks_like_duration(bad), "{bad}");
    }
}

/// Named by the argument that is off, while dormant too.
#[tokio::test]
async fn writes_refuse_a_bad_issuable_ref() {
    let (h, _dir) = dormant_handlers();
    for (project_id, iid, argument) in
        [(0, 42, "project_id"), (7, -1, "iid"), (-1, 0, "project_id")]
    {
        let mut call = AsyncCall::default();
        h.close(
            &mut call as &mut dyn Call_Close,
            project_id,
            iid,
            IssuableKind::work_item,
        )
        .await
        .unwrap();
        assert_eq!(invalid_argument(&mut call), argument);
        let mut call = AsyncCall::default();
        h.assign_self(
            &mut call as &mut dyn Call_AssignSelf,
            project_id,
            iid,
            IssuableKind::merge_request,
        )
        .await
        .unwrap();
        assert_eq!(invalid_argument(&mut call), argument);
        let mut call = AsyncCall::default();
        h.unassign_self(
            &mut call as &mut dyn Call_UnassignSelf,
            project_id,
            iid,
            IssuableKind::work_item,
        )
        .await
        .unwrap();
        assert_eq!(invalid_argument(&mut call), argument);
    }
    let mut call = AsyncCall::default();
    h.post_time(
        &mut call as &mut dyn Call_PostTime,
        7,
        42,
        IssuableKind::work_item,
        "soon".into(),
        None,
    )
    .await
    .unwrap();
    assert_eq!(invalid_argument(&mut call), "duration");
    assert!(h.queue.pending().unwrap().is_empty());
}

/// GitLab's refusal of a write comes back with its status, a dead token's
/// too; nothing is queued.
#[tokio::test]
async fn a_refused_write_replies_gitlab_error_with_the_status() {
    for (err, status) in [
        (FakeErr::Rejected, Some(403)),
        (FakeErr::RejectedWith(404), Some(404)),
        (FakeErr::Unauthorized, Some(401)),
    ] {
        let fake = Arc::new(FakeGitlab::default());
        fake.fail_next_write(err);
        let (h, _dir) = connected_handlers(&fake);
        let mut call = AsyncCall::default();
        h.close(
            &mut call as &mut dyn Call_Close,
            7,
            42,
            IssuableKind::work_item,
        )
        .await
        .unwrap();
        assert_eq!(gitlab_status(&mut call), status, "{err:?}");
        assert!(h.queue.pending().unwrap().is_empty(), "{err:?}");
    }
}

// ── Writes ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn post_time_queues_through_an_unreachable_outage() {
    let (h, _dir) = unreachable_handlers();
    assert_eq!(post_time(&h, 7, 42, IssuableKind::work_item).await, None);
    assert_eq!(h.queue.pending().unwrap().len(), 1, "drains on reconnect");
}

#[tokio::test]
async fn post_time_rejects_when_dormant_but_not_unreachable() {
    let (h, _dir) = dormant_handlers();
    assert_eq!(
        post_time(&h, 7, 42, IssuableKind::work_item)
            .await
            .as_deref(),
        Some(NOT_AUTHENTICATED),
        "no credentials: queuing wouldn't help"
    );
    assert!(h.queue.pending().unwrap().is_empty());
}

/// A 429 is refused before GitLab does any work, so even a PostTime is safe
/// to queue; a 5xx may already have booked the time, so it is reported as an
/// unknown outcome.
#[tokio::test]
async fn post_time_queues_rate_limits_but_reports_server_errors() {
    for (status, error) in [(429, None), (502, Some(GITLAB_UNAVAILABLE))] {
        let fake = Arc::new(FakeGitlab::default());
        fake.fail_next_write(FakeErr::Throttled(status));
        let (h, _dir) = connected_handlers(&fake);
        let replied = post_time(&h, 7, 42, IssuableKind::work_item).await;
        assert_eq!(replied.as_deref(), error, "{status}");
        assert_eq!(
            h.queue.pending().unwrap().len(),
            usize::from(error.is_none()),
            "{status}"
        );
    }
}

#[tokio::test]
async fn close_queues_through_a_server_error() {
    let fake = Arc::new(FakeGitlab::default());
    fake.fail_next_write(FakeErr::Throttled(503));
    let (h, _dir) = connected_handlers(&fake);
    assert_eq!(close(&h, 7, 42, IssuableKind::work_item).await, None);
    let pending = h.queue.pending().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].write.op, WriteOp::Close);
}

/// One blip on a write is not a disconnect: the write is queued and the
/// session stays connected — only the sync worker demotes.
#[tokio::test]
async fn post_time_transient_queues_without_demoting() {
    let fake = Arc::new(FakeGitlab::default());
    fake.fail_next_write(FakeErr::Transient);
    let (h, _dir) = connected_handlers(&fake);

    assert_eq!(post_time(&h, 7, 42, IssuableKind::work_item).await, None);
    assert_eq!(h.queue.pending().unwrap().len(), 1);
    assert!(matches!(&*h.session.read().await, ConnState::Connected(_)));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), h.reconnect_signal.notified())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn post_time_on_mr_defers_with_the_global_mr_id() {
    let (h, _dir) = unreachable_handlers();
    seed(
        &h,
        &[mr(7, 10, "m", "https://gl/g/p/-/merge_requests/10", 1)],
    );

    assert_eq!(
        post_time(&h, 7, 10, IssuableKind::merge_request).await,
        None
    );
    let pending = h.queue.pending().unwrap();
    assert_eq!(pending[0].write.kind, Issuable::MergeRequest);
    assert!(matches!(
        pending[0].write.op,
        WriteOp::PostTime {
            issuable_id: Some(7010),
            ..
        }
    ));
}

#[tokio::test]
async fn an_applied_write_reruns_the_jobs_that_show_it() {
    let fake = Arc::new(FakeGitlab::default());
    let (h, _dir) = connected_handlers(&fake);

    assert_eq!(close(&h, 7, 1, IssuableKind::work_item).await, None);
    assert_eq!(fake.writes(), [("close", Issuable::Issue, 7, 1)]);
    eventually("the assigned-issue refresh", || {
        !fake.calls_to("issues").is_empty()
    })
    .await;
}

// ── Creating an issue ──────────────────────────────────────────────────

#[tokio::test]
async fn create_work_item_rejects_a_blank_title_or_bad_project() {
    let fake = Arc::new(FakeGitlab::default());
    let (h, _dir) = connected_handlers(&fake);
    for (project_id, title, argument) in [
        (7, "", "title"),
        (7, " \t\n", "title"),
        (0, "Fix it", "project_id"),
        (-3, "Fix it", "project_id"),
    ] {
        let mut call = create_work_item(&h, project_id, title, None).await;
        assert_eq!(
            invalid_argument(&mut call),
            argument,
            "{project_id} {title:?}"
        );
    }
    // GitLab would read one label with a comma as two.
    let labels = ["bug", "auth,flow"];
    let mut call = create_work_item_with(&h, 7, "Fix it", None, Some(&labels), None, None).await;
    assert_eq!(invalid_argument(&mut call), "labels");

    assert!(fake.writes().is_empty(), "refused before GitLab is asked");
    assert_eq!(fake.read_calls(), 0);

    // Refused while dormant too, as what it is: an invalid call.
    let (h, _dir) = dormant_handlers();
    let mut call = create_work_item(&h, 7, "", None).await;
    assert_eq!(invalid_argument(&mut call), "title");
}

/// Where the other writes are queued, a create fails: a replay has nothing
/// to tell it whether the first attempt landed.
#[tokio::test]
async fn create_work_item_is_never_queued() {
    for (h, _dir) in [unreachable_handlers(), dormant_handlers()] {
        let mut call = create_work_item(&h, 7, "Fix it", Some(true)).await;
        assert_eq!(reply_error(&mut call).as_deref(), Some(NOT_AUTHENTICATED));
        assert!(h.queue.pending().unwrap().is_empty());
        assert!(h.queue.failures().unwrap().is_empty());
    }

    // The failures every other write is queued on: the outcome is unknown.
    for err in [
        FakeErr::Transient,
        FakeErr::Throttled(429),
        FakeErr::Throttled(503),
    ] {
        let fake = Arc::new(FakeGitlab::default());
        fake.fail_next_write(err);
        let (h, _dir) = connected_handlers(&fake);
        let mut call = create_work_item(&h, 7, "Fix it", None).await;
        assert_eq!(
            reply_error(&mut call).as_deref(),
            Some(GITLAB_UNAVAILABLE),
            "{err:?}"
        );
        assert!(h.queue.pending().unwrap().is_empty(), "{err:?}");
        assert!(h.queue.failures().unwrap().is_empty(), "{err:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            fake.writes(),
            [("create_issue", Issuable::Issue, 7, 0)],
            "{err:?}: tried once"
        );
    }
}

/// A refusal with its status, an unknown outcome as such; a 401 included:
/// the sync worker judges the session, not a write.
#[tokio::test]
async fn create_work_item_reports_any_gitlab_failure_without_demoting() {
    for (err, replied, status) in [
        (FakeErr::Transient, GITLAB_UNAVAILABLE, None),
        (FakeErr::Throttled(429), GITLAB_UNAVAILABLE, None),
        (FakeErr::Throttled(502), GITLAB_UNAVAILABLE, None),
        (FakeErr::Rejected, GITLAB_ERROR, Some(403)),
        (FakeErr::RejectedWith(422), GITLAB_ERROR, Some(422)),
        (FakeErr::Unauthorized, GITLAB_ERROR, Some(401)),
    ] {
        let fake = Arc::new(FakeGitlab::default());
        fake.fail_next_write(err);
        let (h, _dir) = connected_handlers(&fake);
        seed_recent_issues(&h);

        let mut call = create_work_item(&h, 7, "Fix it", Some(true)).await;
        let (name, args) = reply_error_with(&mut call).unwrap();
        assert_eq!(name, replied, "{err:?}");
        assert_eq!(
            args.get("status").and_then(|s| s.as_i64()),
            status,
            "{err:?}"
        );
        assert!(
            matches!(&*h.session.read().await, ConnState::Connected(_)),
            "{err:?}"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), h.reconnect_signal.notified())
                .await
                .is_err(),
            "{err:?}"
        );
        // Nothing was created as far as the daemon knows: nothing to show.
        assert_eq!(h.sync.store().issues.scan(RowScope::Prefix(7)).unwrap(), []);
        assert_eq!(fake.read_calls(), 0, "{err:?}: no list reruns");
    }
}

/// Before any list fetched it: the reruns the create sets off are held.
#[tokio::test]
async fn a_created_issue_is_searchable_at_once() {
    let fake = Arc::new(FakeGitlab::default());
    fake.serve_create(created_json(12, "Fix the login", &[]));
    let _held = fake.gate("issues");
    let (h, _dir) = connected_handlers(&fake);
    seed_recent_issues(&h);
    mark_synced(&h, &[Job::MemberProjects]);

    let mut call = create_work_item(&h, 7, "Fix the login", None).await;
    let created: CreateWorkItem_Reply = reply(&mut call);
    assert_eq!(created.iid, Some(12));
    assert_eq!(
        created.web_url.as_deref(),
        Some("https://gitlab.test/g/p7/-/issues/12")
    );

    let found = run_search(&h, "login", None, None).await.work_items;
    assert_eq!(iids(&found), [12]);
    assert_eq!(found[0].title, "Fix the login");
    assert_eq!(found[0].project_id, Some(7));
    // The newest of what the user authored; nobody is assigned.
    let authored = list_work_items(&h, Some(WorkItemRole::author), None, None).await;
    assert_eq!(iids(&authored), [12, 1, 2]);
    let assigned = list_work_items(&h, Some(WorkItemRole::assignee), None, None).await;
    assert_eq!(iids(&assigned), [2, 3]);
    assert!(fake.calls_to("issues").len() <= 1, "nothing landed since");
}

#[tokio::test]
async fn a_created_issue_assigned_to_me_is_listed_at_once() {
    let fake = Arc::new(FakeGitlab::default());
    fake.serve_create(created_json(12, "Fix the login", &[1]));
    // A Guest may not assign: GitLab creates the issue without assignee.
    fake.serve_create(created_json(13, "Fix the logout", &[]));
    let (h, _dir) = connected_handlers(&fake);
    seed_assigned_issues(&h);
    seed_recent_issues(&h);

    for (title, iid) in [("Fix the login", 12), ("Fix the logout", 13)] {
        // Held anew: a create restarts the list the one before set off.
        let _held = fake.gate("issues");
        let mut call = create_work_item(&h, 7, title, Some(true)).await;
        assert_eq!(reply::<CreateWorkItem_Reply>(&mut call).iid, Some(iid));
    }

    let assigned = assigned_work_items(&h, None).await;
    assert!(iids(&assigned).contains(&12), "{:?}", iids(&assigned));
    assert!(!iids(&assigned).contains(&13), "by GitLab's answer");
    let recent = list_work_items(&h, Some(WorkItemRole::assignee), None, None).await;
    assert_eq!(iids(&recent), [12, 2, 3]);
    let authored = list_work_items(&h, Some(WorkItemRole::author), None, None).await;
    assert_eq!(iids(&authored)[..2], [12, 13]);
}

/// The parent is the stored epic, which GitLab takes by its legacy id.
#[tokio::test]
async fn create_work_item_passes_its_arguments_on() {
    let fake = Arc::new(FakeGitlab::default());
    let (h, _dir) = connected_handlers(&fake);
    seed(&h, &[epic(5, 5, "Accounts", 100)]);

    let labels = ["bug", "auth flow"];
    let mut call = create_work_item_with(
        &h,
        7,
        "Fix the login",
        Some("It fails."),
        Some(&labels),
        Some(true),
        Some(parent(5, 5)),
    )
    .await;
    assert_eq!(reply_error(&mut call), None);
    // Omitted on the wire: no labels, no epic, nobody assigned.
    let mut call = create_work_item(&h, 8, "Bare", None).await;
    assert_eq!(reply_error(&mut call), None);

    let full = NewIssue {
        title: "Fix the login".into(),
        description: Some("It fails.".into()),
        labels: vec!["bug".into(), "auth flow".into()],
        assign_self: true,
        epic_id: Some(5005),
    };
    let bare = NewIssue {
        title: "Bare".into(),
        ..Default::default()
    };
    assert_eq!(fake.created(), [(7, full), (8, bare)]);
    assert_eq!(
        fake.writes(),
        [
            ("create_issue", Issuable::Issue, 7, 0),
            ("create_issue", Issuable::Issue, 8, 0)
        ]
    );
    assert_eq!(fake.epic_calls(), [], "the epic is stored");
    // The lists that show it run again.
    eventually("the list reruns", || !fake.calls_to("issues").is_empty()).await;
}

/// An epic the store doesn't hold is asked of GitLab, once, before the
/// create; the type may be left out or spelled in any case.
#[tokio::test]
async fn create_work_item_looks_an_unknown_parent_up() {
    let fake = Arc::new(FakeGitlab::default());
    fake.serve_epic(epic_json(9, 4, "Roadmap"));
    let (h, _dir) = connected_handlers(&fake);

    for spelled in [Some("Epic"), None] {
        let parent = WorkItemRef {
            r#type: spelled.map(str::to_string),
            ..parent(9, 4)
        };
        let mut call = create_under(&h, parent).await;
        assert_eq!(reply_error(&mut call), None, "{spelled:?}");
    }
    let epic_ids: Vec<_> = fake.created().iter().map(|(_, new)| new.epic_id).collect();
    assert_eq!(epic_ids, [Some(9004), Some(9004)], "the legacy id");
    assert_eq!(fake.epic_calls(), [(9, 4), (9, 4)]);
}

/// Nothing is created under a parent that can't be found, whatever kept it.
#[tokio::test]
async fn a_failed_parent_lookup_creates_nothing() {
    let fake = Arc::new(FakeGitlab::default());
    fake.fail_next(&epic_path(9, 4), FakeErr::Transient);
    fake.fail_next(&epic_path(9, 4), FakeErr::Throttled(503));
    // Not served at all: a 404.
    let (h, _dir) = connected_handlers(&fake);

    for _ in 0..2 {
        let mut call = create_under(&h, parent(9, 4)).await;
        assert_eq!(reply_error(&mut call).as_deref(), Some(GITLAB_UNAVAILABLE));
    }
    let mut call = create_under(&h, parent(9, 4)).await;
    assert_eq!(gitlab_status(&mut call), Some(404));
    assert_eq!(fake.epic_calls().len(), 3, "each looked up once");
    assert!(fake.created().is_empty() && fake.writes().is_empty());
    assert!(h.queue.pending().unwrap().is_empty());
    assert!(matches!(&*h.session.read().await, ConnState::Connected(_)));

    // GitLab's answer has to name the epic.
    let fake = Arc::new(FakeGitlab::default());
    let mut unnamed = epic_json(9, 4, "Roadmap");
    unnamed["id"] = serde_json::Value::Null;
    fake.serve_epic(unnamed);
    let (h, _dir) = connected_handlers(&fake);
    let mut call = create_under(&h, parent(9, 4)).await;
    assert_eq!(gitlab_status(&mut call), None, "no status to an answer");
    assert!(fake.created().is_empty());
    // Nor one the epic mirror can't read.
    let fake = Arc::new(FakeGitlab::default());
    let mut unreadable = epic_json(9, 4, "Roadmap");
    unreadable["id"] = "9004".into();
    fake.serve_epic(unreadable);
    let (h, _dir) = connected_handlers(&fake);
    let mut call = create_under(&h, parent(9, 4)).await;
    assert_eq!(gitlab_status(&mut call), None);
    assert!(fake.created().is_empty());
}

/// Only an epic, named by its group and number, can be a parent; anything
/// else is refused before GitLab is asked, while dormant too.
#[tokio::test]
async fn create_work_item_refuses_a_parent_that_is_no_epic() {
    let fake = Arc::new(FakeGitlab::default());
    let (h, _dir) = connected_handlers(&fake);
    let in_project = WorkItemRef {
        project_id: Some(7),
        group_id: None,
        ..parent(9, 4)
    };
    let both = WorkItemRef {
        project_id: Some(7),
        ..parent(9, 4)
    };
    let a_task = WorkItemRef {
        r#type: Some("task".into()),
        ..parent(9, 4)
    };
    for refused in [
        in_project,
        both,
        a_task,
        parent(0, 4),
        parent(9, 0),
        parent(-1, 4),
    ] {
        let mut call = create_under(&h, refused.clone()).await;
        assert_eq!(invalid_argument(&mut call), "parent", "{refused:?}");
    }
    assert_eq!(fake.read_calls(), 0);
    assert!(fake.writes().is_empty());

    let (h, _dir) = dormant_handlers();
    let mut call = create_under(&h, parent(9, 0)).await;
    assert_eq!(invalid_argument(&mut call), "parent");
}

/// The issue exists: an answer the daemon can't read must not read as a
/// failure, or the caller files it again.
#[tokio::test]
async fn create_work_item_replies_success_whatever_gitlab_answered() {
    let fake = Arc::new(FakeGitlab::default());
    fake.serve_create(serde_json::json!("created"));
    // No global id: nothing to store, but the number and link are there.
    let mut partial = created_json(12, "Fix the login", &[]);
    partial["id"] = serde_json::Value::Null;
    fake.serve_create(partial);
    let (h, _dir) = connected_handlers(&fake);

    let mut call = create_work_item(&h, 7, "Fix the login", None).await;
    let unreadable = call.take_reply().unwrap();
    assert_eq!(unreadable.error, None);
    // Success, with nothing to say: neither field.
    assert_eq!(unreadable.parameters, Some(serde_json::json!({})));

    let mut call = create_work_item(&h, 7, "Fix the login", None).await;
    let created: CreateWorkItem_Reply = reply(&mut call);
    assert_eq!(created.iid, Some(12));
    assert!(
        created.web_url.as_deref().unwrap().ends_with("/issues/12"),
        "{created:?}"
    );
    assert_eq!(h.sync.store().issues.scan(RowScope::Prefix(7)).unwrap(), []);
}

// ── Read-time overlay of writes ────────────────────────────────────────

#[tokio::test]
async fn a_queued_close_hides_the_issue_until_it_settles() {
    let (h, _dir) = unreachable_handlers();
    seed_assigned_issues(&h);

    assert_eq!(close(&h, 1, 1, IssuableKind::work_item).await, None);
    let iids: Vec<i64> = assigned_work_items(&h, None)
        .await
        .iter()
        .map(|i| i.iid)
        .collect();
    assert_eq!(iids, [3, 2], "the pending close already applies");
}

#[tokio::test]
async fn an_applied_write_hides_only_views_fetched_before_it() {
    let (h, _dir) = dormant_handlers();
    seed_assigned_issues(&h);
    h.sync.note_write(&Write {
        kind: Issuable::Issue,
        project_id: 1,
        iid: 1,
        op: WriteOp::UnassignSelf,
    });
    let iids = |v: Vec<WorkItem>| v.iter().map(|i| i.iid).collect::<Vec<_>>();
    assert_eq!(iids(assigned_work_items(&h, None).await), [3, 2]);

    // A view fetched after the write reflects GitLab, including the write.
    seed_view(
        &h,
        ASSIGNED_ISSUES,
        &[(2, 3), (1, 1), (1, 2)],
        now_secs() + 5,
    );
    assert_eq!(iids(assigned_work_items(&h, None).await), [3, 1, 2]);
}

#[tokio::test]
async fn a_queued_mr_unassign_hides_the_merge_request() {
    let (h, _dir) = unreachable_handlers();
    seed_assigned_mrs(&h);

    assert_eq!(unassign(&h, 2, 11, IssuableKind::merge_request).await, None);

    let iids: Vec<i64> = assigned_mrs(&h, None).await.iter().map(|m| m.iid).collect();
    assert_eq!(iids, [10]);
}

/// A project sync may update a viewed row before the assigned list re-syncs:
/// a row that is closed or no longer assigned to the user drops out at once.
#[tokio::test]
async fn assigned_views_follow_the_rows_current_state() {
    let (h, _dir) = dormant_handlers();
    seed_assigned_issues(&h);
    let mut c = h.sync.store().begin();
    c.set_identity(&crate::sync::store::Identity {
        host: "gitlab.test".into(),
        user_id: 1,
    })
    .unwrap();
    c.commit().unwrap();

    let mut closed = issue(1, 1, "api", "https://gl/team/api/-/issues/1");
    closed.state = "closed".into();
    let mut reassigned = issue(1, 2, "web", "https://gl/team/sub/web/-/issues/2");
    reassigned.assignees = vec![UserRef {
        id: 2,
        username: "someone".into(),
    }];
    seed(&h, &[closed, reassigned]);

    let iids: Vec<i64> = assigned_work_items(&h, None)
        .await
        .iter()
        .map(|i| i.iid)
        .collect();
    assert_eq!(iids, [3]);
}

// ── Assigned issues ────────────────────────────────────────────────────

/// Served under a dormant session: proves the read never fetches — a fetch
/// would have replied NotAuthenticated instead.
#[tokio::test]
async fn get_assigned_work_items_serves_the_view_while_dormant_grouped_by_namespace() {
    let (h, _dir) = dormant_handlers();
    seed_assigned_issues(&h);
    let iids: Vec<i64> = assigned_work_items(&h, None)
        .await
        .iter()
        .map(|i| i.iid)
        .collect();
    assert_eq!(iids, [3, 1, 2], "other/x, team/api, team/sub/web");
}

#[tokio::test]
async fn get_assigned_work_items_filters_by_group_and_subgroups() {
    let (h, _dir) = dormant_handlers();
    seed_assigned_issues(&h);
    let iids = |v: Vec<WorkItem>| v.iter().map(|i| i.iid).collect::<Vec<_>>();
    assert_eq!(
        iids(assigned_work_items(&h, Some(vec!["team".into()])).await),
        [1, 2]
    );
    assert_eq!(
        iids(assigned_work_items(&h, Some(vec!["team/sub".into(), "team".into()])).await),
        [1, 2],
        "overlapping groups list each issue once"
    );
    assert_eq!(
        iids(assigned_work_items(&h, Some(vec!["tea".into()])).await),
        Vec::<i64>::new(),
        "a shared prefix is not a group"
    );
}

#[tokio::test]
async fn get_assigned_work_items_never_synced_is_honest_about_the_session() {
    let (h, _dir) = dormant_handlers();
    let mut call = AsyncCall::default();
    h.get_assigned_work_items(&mut call as &mut dyn Call_GetAssignedWorkItems, None)
        .await
        .unwrap();
    assert_eq!(reply_error(&mut call).as_deref(), Some(NOT_AUTHENTICATED));

    let fake = Arc::new(FakeGitlab::default());
    let (h, _dir) = connected_handlers(&fake);
    assert!(
        assigned_work_items(&h, None).await.is_empty(),
        "connected: the first sync is pending"
    );
    assert_eq!(fake.read_calls(), 0, "reads never fetch");
}

#[tokio::test]
async fn get_assigned_work_items_overlays_open_counts_and_board_status() {
    let (h, _dir) = dormant_handlers();
    let mut labeled = issue(1, 1, "api", "https://gl/team/api/-/issues/1");
    labeled.labels = vec!["bug".into(), "Doing".into()];
    seed(
        &h,
        &[
            labeled,
            issue(2, 3, "other", "https://gl/other/x/-/issues/3"),
        ],
    );
    seed_view(&h, ASSIGNED_ISSUES, &[(1, 1), (2, 3)], now_secs() - 60);
    seed(
        &h,
        &[Board {
            id: 9,
            project_id: 1,
            lists: vec![BoardList {
                label: Some(LabelRef {
                    name: "Doing".into(),
                }),
            }],
        }],
    );
    mark_synced(&h, &[Job::AssignedIssues, Job::ProjectBoards(1)]);
    run_record_open(&h, 1, 1, IssuableKind::work_item).await;

    let issues = assigned_work_items(&h, None).await;
    let api = issues.iter().find(|i| i.iid == 1).unwrap();
    assert_eq!(api.open_count, 1);
    assert_eq!(api.graph_status, "Doing");
    let other = issues.iter().find(|i| i.iid == 3).unwrap();
    assert_eq!(other.graph_status, "", "project 2's boards never synced");
}

/// Assigned issues are work items of their project, of their type, under
/// the epic they name. GitLab links that epic relative to the instance.
#[tokio::test]
async fn assigned_work_items_name_their_epic_as_parent() {
    let (h, _dir) = dormant_handlers();
    seed_assigned_issues(&h);
    let in_epic = |iid, group_id| model::EpicRef {
        id: group_id * 1000 + iid,
        iid,
        group_id,
        title: "Roadmap".into(),
        url: format!("/groups/team/-/epics/{iid}"),
    };
    let mut task = issue(1, 1, "api", "https://gl/team/api/-/issues/1");
    task.issue_type = "task".into();
    task.epic = Some(in_epic(7, 5));
    let mut stored = issue(1, 2, "web", "https://gl/team/sub/web/-/issues/2");
    stored.epic = Some(in_epic(8, 5));
    seed(&h, &[task, stored]);
    let mut epic_row = epic(5, 8, "Roadmap", 100);
    epic_row.web_url = "https://gl.example/groups/team/-/epics/8".into();
    seed(&h, &[epic_row]);

    let items = assigned_work_items(&h, None).await;
    let of = |iid| items.iter().find(|i| i.iid == iid).unwrap();
    assert_eq!(
        (of(1).r#type.as_str(), of(3).r#type.as_str()),
        ("task", "issue")
    );
    assert_eq!((of(1).project_id, of(1).group_id), (Some(1), None));
    assert_eq!(of(1).id, 1001);
    assert_eq!(
        of(1).parent,
        Some(WorkItemRef {
            project_id: None,
            group_id: Some(5),
            iid: 7,
            r#type: Some("epic".into()),
            title: Some("Roadmap".into()),
            web_url: Some("https://gl/groups/team/-/epics/7".into()),
        }),
        "no epic row: the issue's own host"
    );
    let parent = of(2).parent.clone().unwrap();
    assert_eq!(
        parent.web_url.as_deref(),
        Some("https://gl.example/groups/team/-/epics/8"),
        "the stored epic's link"
    );
    assert_eq!(of(3).parent, None);
}

// ── Recent issues ──────────────────────────────────────────────────────

/// Served under a dormant session: the read never fetches. 1/2 is in both
/// views and listed once.
#[tokio::test]
async fn list_work_items_serves_both_roles_once_newest_first() {
    let (h, _dir) = dormant_handlers();
    seed_recent_issues(&h);
    run_record_open(&h, 2, 3, IssuableKind::work_item).await;

    let issues = list_work_items(&h, None, None, None).await;
    assert_eq!(iids(&issues), [1, 2, 3]);
    let updated: Vec<Option<i64>> = issues.iter().map(|i| i.updated_at).collect();
    assert_eq!(updated, [Some(300), Some(200), Some(100)]);
    assert_eq!(issues[1].state, "closed", "closed ones are listed too");
    assert_eq!(issues[2].namespace_path.as_deref(), Some("other/x"));
    assert_eq!(issues[2].open_count, 1);
    assert_eq!(issues[2].graph_status, "", "its boards never synced");
}

#[tokio::test]
async fn list_work_items_filters_by_role_state_and_time() {
    let (h, _dir) = dormant_handlers();
    seed_recent_issues(&h);
    let (author, assignee) = (Some(WorkItemRole::author), Some(WorkItemRole::assignee));
    let (opened, closed) = (
        || Some(vec![WorkItemState::opened]),
        || Some(vec![WorkItemState::closed]),
    );

    assert_eq!(iids(&list_work_items(&h, author, None, None).await), [1, 2]);
    let assigned = list_work_items(&h, assignee.clone(), None, None).await;
    assert_eq!(iids(&assigned), [2, 3]);

    assert_eq!(iids(&list_work_items(&h, None, None, closed()).await), [2]);
    assert_eq!(
        iids(&list_work_items(&h, None, None, opened()).await),
        [1, 3]
    );
    let both = Some(vec![WorkItemState::closed, WorkItemState::opened]);
    assert_eq!(
        iids(&list_work_items(&h, None, None, both).await),
        [1, 2, 3]
    );
    let none = Some(Vec::new());
    assert_eq!(
        iids(&list_work_items(&h, None, None, none).await),
        [1, 2, 3],
        "no state is every state"
    );

    assert_eq!(
        iids(&list_work_items(&h, None, Some(200), None).await),
        [1, 2],
        "inclusive, as GitLab's updated_after"
    );
    assert_eq!(iids(&list_work_items(&h, None, Some(201), None).await), [1]);
    assert_eq!(
        iids(&list_work_items(&h, None, Some(-5), None).await),
        [1, 2, 3]
    );
    let narrow = list_work_items(&h, assignee, Some(100), opened()).await;
    assert_eq!(iids(&narrow), [3], "the filters combine");
}

#[tokio::test]
async fn list_work_items_never_synced_is_honest_about_the_session() {
    let (h, _dir) = dormant_handlers();
    let cold = |role| list_work_items_error(&h, role);
    assert_eq!(cold(None).await.as_deref(), Some(NOT_AUTHENTICATED));

    // One list alone answers for its role, not for both.
    seed(&h, &[issue(1, 1, "api", "https://gl/team/api/-/issues/1")]);
    seed_view(&h, RECENT_AUTHORED_ISSUES, &[(1, 1)], now_secs() - 60);
    mark_synced(&h, &[Job::RecentAuthoredIssues]);
    assert_eq!(cold(Some(WorkItemRole::author)).await, None);
    let assigned = cold(Some(WorkItemRole::assignee)).await;
    assert_eq!(assigned.as_deref(), Some(NOT_AUTHENTICATED));
    assert_eq!(cold(None).await.as_deref(), Some(NOT_AUTHENTICATED));

    let fake = Arc::new(FakeGitlab::default());
    let (h, _dir) = connected_handlers(&fake);
    assert!(
        list_work_items(&h, None, None, None).await.is_empty(),
        "connected: the first sync is pending"
    );
    assert_eq!(fake.read_calls(), 0, "reads never fetch");
}

/// An unassign the list doesn't reflect yet takes the issue out of the
/// assigned ones, not out of the authored ones; so does a row a project sync
/// stored with other assignees.
#[tokio::test]
async fn list_work_items_drops_what_was_unassigned_since() {
    let (h, _dir) = unreachable_handlers();
    seed_recent_issues(&h);
    let mut c = h.sync.store().begin();
    c.set_identity(&crate::sync::store::Identity {
        host: "gitlab.test".into(),
        user_id: 1,
    })
    .unwrap();
    c.commit().unwrap();
    let assignee = || Some(WorkItemRole::assignee);

    assert_eq!(unassign(&h, 1, 2, IssuableKind::work_item).await, None);
    assert_eq!(
        iids(&list_work_items(&h, assignee(), None, None).await),
        [3]
    );
    assert_eq!(
        iids(&list_work_items(&h, None, None, None).await),
        [1, 2, 3],
        "still authored"
    );

    let mut reassigned = issue(2, 3, "other", "https://gl/other/x/-/issues/3");
    reassigned.assignees = vec![UserRef {
        id: 2,
        username: "someone".into(),
    }];
    seed(&h, &[reassigned]);
    assert!(list_work_items(&h, assignee(), None, None).await.is_empty());
    assert_eq!(iids(&list_work_items(&h, None, None, None).await), [1, 2]);
}

/// A close the lists don't reflect yet reads as closed, before the state
/// filter: queued, or applied since the list was fetched.
#[tokio::test]
async fn a_just_closed_issue_lists_as_closed() {
    let (h, _dir) = unreachable_handlers();
    seed_recent_issues(&h);
    let closed = || Some(vec![WorkItemState::closed]);
    let opened = || Some(vec![WorkItemState::opened]);

    assert_eq!(close(&h, 1, 1, IssuableKind::work_item).await, None);
    let issues = list_work_items(&h, None, None, None).await;
    assert_eq!(iids(&issues), [1, 2, 3]);
    assert_eq!(
        issues[0].state, "closed",
        "the pending close already applies"
    );
    assert_eq!(
        iids(&list_work_items(&h, None, None, closed()).await),
        [1, 2]
    );
    assert_eq!(iids(&list_work_items(&h, None, None, opened()).await), [3]);

    h.sync.note_write(&Write {
        kind: Issuable::Issue,
        project_id: 2,
        iid: 3,
        op: WriteOp::Close,
    });
    assert!(list_work_items(&h, None, None, opened()).await.is_empty());
    // A list fetched after the write reflects GitLab, including the write.
    seed_view(
        &h,
        RECENT_ASSIGNED_ISSUES,
        &[(2, 3), (1, 2)],
        now_secs() + 5,
    );
    assert_eq!(iids(&list_work_items(&h, None, None, opened()).await), [3]);
}

// ── Assigned merge requests ────────────────────────────────────────────

#[tokio::test]
async fn get_assigned_merge_requests_serves_newest_first_with_group_filter() {
    let (h, _dir) = dormant_handlers();
    seed_assigned_mrs(&h);
    let all = assigned_mrs(&h, None).await;
    assert_eq!(all.iter().map(|m| m.iid).collect::<Vec<_>>(), [11, 10]);
    assert_eq!(all[0].assignees, ["me"]);
    assert_eq!(all[0].updated_at, Some(200));
    let team = assigned_mrs(&h, Some(vec!["team".into()])).await;
    assert_eq!(team.iter().map(|m| m.iid).collect::<Vec<_>>(), [10]);
}

#[tokio::test]
async fn get_assigned_merge_requests_never_synced_is_honest_about_the_session() {
    let (h, _dir) = dormant_handlers();
    let mut call = AsyncCall::default();
    h.get_assigned_merge_requests(&mut call as &mut dyn Call_GetAssignedMergeRequests, None)
        .await
        .unwrap();
    assert_eq!(reply_error(&mut call).as_deref(), Some(NOT_AUTHENTICATED));

    let fake = Arc::new(FakeGitlab::default());
    let (h, _dir) = connected_handlers(&fake);
    assert!(assigned_mrs(&h, None).await.is_empty());
}

// ── Search ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn search_matches_title_labels_and_paths_case_insensitively() {
    let (h, _dir) = dormant_handlers();
    seed_corpus(&h);
    let r = run_search(&h, "OAUTH", None, None).await;
    assert_eq!(r.work_items.iter().map(|i| i.iid).collect::<Vec<_>>(), [10]);
    assert_eq!(r.merge_requests.len(), 1);
    assert!(r.projects.is_empty() && r.groups.is_empty());

    let r = run_search(&h, "backend", None, None).await;
    assert_eq!(
        r.work_items.iter().map(|i| i.iid).collect::<Vec<_>>(),
        [20],
        "by label"
    );

    let r = run_search(&h, "auth-serv", None, None).await;
    assert_eq!(r.projects[0].path, "team/auth-service");
    let r = run_search(&h, "tea", None, None).await;
    assert_eq!(r.groups[0].path, "team");
}

#[tokio::test]
async fn search_iid_reference_matches_issues_and_mrs() {
    let (h, _dir) = dormant_handlers();
    seed_corpus(&h);
    let r = run_search(&h, "#30", None, None).await;
    assert!(r.work_items.is_empty());
    assert_eq!(r.merge_requests[0].iid, 30);
}

#[tokio::test]
async fn search_finds_epics_by_title_label_and_reference() {
    let (h, _dir) = dormant_handlers();
    seed_corpus(&h);
    let mut labeled = epic(6, 7, "Q3", 50);
    labeled.labels = vec!["Roadmap".into()];
    labeled.web_url = "https://gl/groups/other/-/epics/7".into();
    seed(&h, &[labeled]);

    let r = run_search(&h, "ROADMAP", None, None).await;
    assert_eq!(
        epic_keys(&r),
        [(5, 7), (6, 7)],
        "title and label, newest first"
    );
    let found = epics(&r);
    assert_eq!(found[0].web_url, "https://gl/groups/team/-/epics/7");
    assert_eq!(
        found[0].namespace_path.as_deref(),
        Some("team"),
        "from the group row"
    );
    assert_eq!(
        found[1].namespace_path.as_deref(),
        Some("other"),
        "no row for group 6: from the link"
    );
    assert!(issue_iids(&r).is_empty() && r.groups.is_empty());

    // `&7` is the epic reference; `#7` stays with issues and MRs.
    let r = run_search(&h, "&7", None, None).await;
    assert_eq!(epic_keys(&r), [(5, 7), (6, 7)], "one per group");
    assert!(issue_iids(&r).is_empty() && r.merge_requests.is_empty());
    assert!(epics(&run_search(&h, "#7", None, None).await).is_empty());

    let r = run_typed_search(&h, "i", &["epic"], Some(1)).await;
    assert_eq!(iids(&r.work_items), [8]);
    assert!(r.projects.is_empty());
    let r = run_typed_search(&h, "i", &["issue"], None).await;
    assert!(epics(&r).is_empty(), "not asked for");
}

/// An epic is a work item of its group, identified by its work item id.
#[tokio::test]
async fn search_serves_an_epic_as_its_groups_work_item() {
    let (h, _dir) = dormant_handlers();
    seed_corpus(&h);

    let r = run_search(&h, "billing", None, None).await;
    let [billing] = r.work_items.as_slice() else {
        panic!("{r:?}");
    };
    assert_eq!(
        (billing.id, billing.iid),
        (905_008, 8),
        "never the legacy id"
    );
    assert_eq!(billing.r#type, "epic");
    assert_eq!((billing.project_id, billing.group_id), (None, Some(5)));
    assert_eq!(billing.parent, None);
    assert_eq!((billing.time_spent, &billing.project_avatar), (None, &None));
}

/// Issues and epics are ranked together and share one limit.
#[tokio::test]
async fn search_ranks_issues_and_epics_under_one_limit() {
    let (h, _dir) = dormant_handlers();
    seed_corpus(&h);

    // Issue 20 and epic 8 both updated at 200, epic 7 at 100.
    let r = run_search(&h, "i", Some(vec![SearchKind::work_items]), Some(2)).await;
    let hits: Vec<_> = r
        .work_items
        .iter()
        .map(|w| (w.r#type.as_str(), w.iid))
        .collect();
    assert_eq!(hits, [("issue", 20), ("epic", 8)]);

    record_open(&h, IssuableKind::work_item, 7, None, Some(5)).await;
    let r = run_search(&h, "i", None, Some(2)).await;
    let hits: Vec<_> = r
        .work_items
        .iter()
        .map(|w| (w.r#type.as_str(), w.iid))
        .collect();
    assert_eq!(hits, [("epic", 7), ("issue", 20)], "the opened epic first");
}

/// `types` keeps the work items of the listed types, in any case.
#[tokio::test]
async fn search_narrows_work_items_to_types() {
    let (h, _dir) = dormant_handlers();
    seed_corpus(&h);
    let mut task = issue(1, 40, "Write the tests", "https://gl/team/p/-/issues/40");
    task.issue_type = "task".into();
    seed(&h, &[task]);
    let found = async |types: &[&str]| -> Vec<(String, i64)> {
        let r = run_typed_search(&h, "t", types, None).await;
        r.work_items
            .into_iter()
            .map(|w| (w.r#type, w.iid))
            .collect()
    };

    let tasks = [("task".to_string(), 40)];
    assert_eq!(found(&["task"]).await, tasks);
    assert_eq!(found(&["TASK"]).await, tasks);
    let both = found(&["Task", "epic"]).await;
    assert_eq!(both, [("epic".into(), 7), ("task".into(), 40)]);
    let issues = found(&["issue"]).await;
    assert_eq!(
        issues,
        [("issue".into(), 20), ("issue".into(), 10)],
        "untyped rows are issues"
    );
    assert!(found(&["incident"]).await.is_empty());
    assert_eq!(found(&[]).await.len(), 4, "no types is every type");
}

/// `exclude_types` drops the work items of the listed types, in any case;
/// with `types`, an item must be in the one and not in the other.
#[tokio::test]
async fn search_leaves_out_work_items_of_excluded_types() {
    let (h, _dir) = dormant_handlers();
    seed_corpus(&h);
    let mut task = issue(1, 40, "Write the tests", "https://gl/team/p/-/issues/40");
    task.issue_type = "task".into();
    seed(&h, &[task]);
    let found = async |types: &[&str], excluded: &[&str]| -> Vec<i64> {
        iids(
            &run_filtered_search(&h, "t", types, excluded, None)
                .await
                .work_items,
        )
    };

    assert_eq!(found(&[], &[]).await, [20, 10, 7, 40]);
    assert_eq!(found(&[], &["epic"]).await, [20, 10, 40], "the task stays");
    assert_eq!(found(&[], &["EPIC", "Task"]).await, [20, 10]);
    assert_eq!(found(&[], &["incident"]).await, [20, 10, 7, 40]);
    assert_eq!(found(&["task", "epic"], &["Epic"]).await, [40]);
    assert!(found(&["epic"], &["epic"]).await.is_empty());

    // The other kinds don't have a type to leave out.
    let mrs = search_with(
        &h,
        "oauth",
        None,
        None,
        None,
        None,
        Some(vec!["epic".into()]),
    )
    .await;
    assert_eq!(iids(&mrs.work_items), [10]);
    assert_eq!(mrs.merge_requests.len(), 1);
}

/// Left out before the limit: it fills from the work items that remain.
#[tokio::test]
async fn search_fills_its_limit_from_what_is_not_excluded() {
    let (h, _dir) = dormant_handlers();
    seed_corpus(&h);
    let mut task = issue(1, 40, "Write the tests", "https://gl/team/p/-/issues/40");
    task.issue_type = "task".into();
    seed(&h, &[task]);

    // Issue 20 and epic 8 updated at 200, epic 7 at 100, the task never.
    let r = run_filtered_search(&h, "i", &[], &[], Some(2)).await;
    assert_eq!(epic_keys(&r), [(5, 8)]);
    let r = run_filtered_search(&h, "i", &[], &["epic"], Some(2)).await;
    assert_eq!(issue_iids(&r), [20, 40]);
    assert!(epics(&r).is_empty());
}

/// A group's work item is counted by its group, apart from the issue of a
/// project sharing id and number; both apart from the merge requests.
#[tokio::test]
async fn record_open_counts_project_and_group_work_items_apart() {
    let (h, _dir) = dormant_handlers();
    seed_corpus(&h);
    seed(
        &h,
        &[issue(5, 7, "same ids", "https://gl/team/p/-/issues/7")],
    );
    let opened = |kind, iid, project_id, group_id| record_open(&h, kind, iid, project_id, group_id);
    assert_eq!(
        opened(IssuableKind::work_item, 7, None, Some(5)).await,
        None
    );
    assert_eq!(
        opened(IssuableKind::work_item, 10, Some(1), None).await,
        None
    );
    assert_eq!(
        opened(IssuableKind::merge_request, 30, Some(1), None).await,
        None
    );

    let usage = h.usage.snapshot().unwrap();
    let count = |key: &str| usage.entries.get(key).map(|e| e.count);
    assert_eq!(count(&epic_usage_key(5, 7)), Some(1));
    assert_eq!(count(&usage_key(Issuable::Issue, 1, 10)), Some(1));
    assert_eq!(count(&usage_key(Issuable::MergeRequest, 1, 30)), Some(1));
    assert_eq!(count(&usage_key(Issuable::Issue, 5, 7)), None);
    assert_eq!(usage.entries.len(), 3);

    let r = run_search(&h, "", None, None).await;
    assert_eq!(epic_keys(&r), [(5, 7)]);
    assert_eq!(epics(&r)[0].open_count, 1);
    assert_eq!(issue_iids(&r), [10], "the issue 5/7 was never opened");

    let r = run_typed_search(&h, "i", &["epic"], None).await;
    assert_eq!(
        iids(&r.work_items),
        [7, 8],
        "the opened older epic outranks the newer one"
    );
}

/// A reference that names no single work item or merge request is refused,
/// and nothing is counted.
#[tokio::test]
async fn record_open_refuses_a_malformed_reference() {
    let (h, _dir) = dormant_handlers();
    let (item, mr) = (IssuableKind::work_item, IssuableKind::merge_request);
    for (kind, iid, project_id, group_id, argument) in [
        (item.clone(), 7, None, None, "project_id"),
        (item.clone(), 7, Some(1), Some(5), "group_id"),
        (mr.clone(), 7, None, Some(5), "group_id"),
        (mr.clone(), 7, None, None, "project_id"),
        (item.clone(), 0, Some(1), None, "iid"),
        (item.clone(), 7, Some(0), None, "project_id"),
        (item.clone(), 7, Some(-1), None, "project_id"),
        (item.clone(), 0, None, Some(5), "iid"),
        (item.clone(), 7, None, Some(0), "group_id"),
        (mr.clone(), -1, Some(1), None, "iid"),
    ] {
        let mut call = AsyncCall::default();
        h.record_open(
            &mut call as &mut dyn Call_RecordOpen,
            kind.clone(),
            iid,
            project_id,
            group_id,
        )
        .await
        .unwrap();
        assert_eq!(
            invalid_argument(&mut call),
            argument,
            "{kind:?} {iid} {project_id:?} {group_id:?}"
        );
    }
    assert!(h.usage.snapshot().unwrap().entries.is_empty());
}

#[tokio::test]
async fn search_refuses_a_limit_that_is_not_positive() {
    let (h, _dir) = dormant_handlers();
    seed_corpus(&h);
    for limit in [0, -1] {
        let mut call = AsyncCall::default();
        h.search(
            &mut call as &mut dyn Call_Search,
            "oauth".into(),
            None,
            Some(limit),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(invalid_argument(&mut call), "limit");
    }
}

#[tokio::test]
async fn search_kinds_filter_and_limit_apply_per_kind() {
    let (h, _dir) = dormant_handlers();
    seed_corpus(&h);
    let r = run_search(&h, "t", Some(vec![SearchKind::work_items]), Some(1)).await;
    assert_eq!(iids(&r.work_items), [20], "newest first, limited");
    assert!(r.merge_requests.is_empty() && r.projects.is_empty() && r.groups.is_empty());
}

#[tokio::test]
async fn search_ranks_frequently_opened_first() {
    let (h, _dir) = dormant_handlers();
    seed_corpus(&h);
    run_record_open(&h, 1, 10, IssuableKind::work_item).await;
    run_record_open(&h, 1, 10, IssuableKind::work_item).await;

    let r = run_search(&h, "", None, None).await;
    assert_eq!(
        iids(&r.work_items),
        [10],
        "an empty query lists only opened items"
    );
    assert_eq!(r.work_items[0].open_count, 2);
    assert!(r.merge_requests.is_empty() && r.projects.is_empty());

    let r = run_typed_search(&h, "t", &["issue"], None).await;
    assert_eq!(
        iids(&r.work_items),
        [10, 20],
        "the opened older issue outranks the newer one"
    );
}

/// Unix seconds, as the rows store them.
#[tokio::test]
async fn search_hits_carry_their_update_time() {
    let (h, _dir) = dormant_handlers();
    seed_corpus(&h);

    let r = run_search(&h, "oauth", None, None).await;
    assert_eq!(r.work_items[0].updated_at, Some(100));
    assert_eq!(r.merge_requests[0].updated_at, Some(50));
    let r = run_typed_search(&h, "i", &["epic"], None).await;
    let updated: Vec<_> = r.work_items.iter().map(|e| (e.iid, e.updated_at)).collect();
    assert_eq!(updated, [(8, Some(200)), (7, Some(100))]);
}

/// The flag is all an archived project differs by: it matches and sorts
/// like any other.
#[tokio::test]
async fn projects_tell_whether_they_are_archived() {
    let (h, _dir) = dormant_handlers();
    seed_corpus(&h);
    seed(
        &h,
        &[model::Project {
            id: 6,
            name: "auth-legacy".into(),
            path_with_namespace: "team/auth-legacy".into(),
            web_url: "https://gl/team/auth-legacy".into(),
            archived: true,
            ..Default::default()
        }],
    );

    let r = run_search(&h, "auth", Some(vec![SearchKind::projects]), None).await;
    let archived: Vec<_> = r
        .projects
        .iter()
        .map(|p| (p.path.as_str(), p.archived))
        .collect();
    assert_eq!(
        archived,
        [("team/auth-legacy", true), ("team/auth-service", false)]
    );
}

#[tokio::test]
async fn search_never_synced_is_honest_about_the_session() {
    let (h, _dir) = dormant_handlers();
    let mut call = AsyncCall::default();
    h.search(
        &mut call as &mut dyn Call_Search,
        "x".into(),
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(reply_error(&mut call).as_deref(), Some(NOT_AUTHENTICATED));

    let fake = Arc::new(FakeGitlab::default());
    let (h, _dir) = connected_handlers(&fake);
    let r = run_search(&h, "x", None, None).await;
    assert!(r.work_items.is_empty() && r.projects.is_empty());
}

fn scope(projects: &[i64], groups: &[&str]) -> Option<SearchScope> {
    Some(SearchScope {
        projects: Some(projects.to_vec()),
        groups: Some(groups.iter().map(|g| g.to_string()).collect()),
    })
}

/// On top of `seed_corpus`: issue "OAuth elsewhere" (2/40) and MR "oauth
/// port" (2/41) in `other/x`, project 6 `other/x`, group 6 `other` with the
/// epic "Identity other" (6/9), and the epic "Identity sub" (7/1) of a
/// subgroup whose row is missing.
fn seed_scoped_corpus(h: &Handlers) {
    seed_corpus(h);
    let mut elsewhere = issue(2, 40, "OAuth elsewhere", "https://gl/other/x/-/issues/40");
    elsewhere.updated_at = 300;
    seed(h, &[elsewhere]);
    seed(
        h,
        &[mr(
            2,
            41,
            "oauth port",
            "https://gl/other/x/-/merge_requests/41",
            300,
        )],
    );
    seed(
        h,
        &[model::Project {
            id: 6,
            name: "x".into(),
            path_with_namespace: "other/x".into(),
            web_url: "https://gl/other/x".into(),
            ..Default::default()
        }],
    );
    seed(
        h,
        &[model::Group {
            id: 6,
            name: "other".into(),
            full_path: "other".into(),
            web_url: "https://gl/other".into(),
        }],
    );
    let mut other = epic(6, 9, "Identity other", 300);
    other.web_url = "https://gl/groups/other/-/epics/9".into();
    let mut sub = epic(7, 1, "Identity sub", 300);
    sub.web_url = "https://gl/groups/team/sub/-/epics/1".into();
    seed(h, &[other, sub]);
}

#[tokio::test]
async fn search_scope_by_project_keeps_only_that_project() {
    let (h, _dir) = dormant_handlers();
    seed_scoped_corpus(&h);
    let r = run_scoped_search(&h, "oauth", None, None, scope(&[1], &[])).await;
    assert_eq!(iids(&r.work_items), [10]);
    assert_eq!(
        r.merge_requests.iter().map(|m| m.iid).collect::<Vec<_>>(),
        [30]
    );

    let r = run_scoped_search(&h, "e", None, None, scope(&[4, 6], &[])).await;
    assert_eq!(
        r.projects.iter().map(|p| p.id).collect::<Vec<_>>(),
        [6, 4],
        "any listed project passes"
    );
    assert!(
        r.groups.is_empty() && epics(&r).is_empty(),
        "a project scope cannot name a group or an epic: {r:?}"
    );
}

#[tokio::test]
async fn search_scope_by_group_covers_subgroups_and_epics() {
    let (h, _dir) = dormant_handlers();
    seed_scoped_corpus(&h);
    let r = run_scoped_search(&h, "t", None, None, scope(&[], &["team"])).await;
    assert_eq!(issue_iids(&r), [20, 10], "team/p is under team");
    assert_eq!(
        r.merge_requests.iter().map(|m| m.iid).collect::<Vec<_>>(),
        [30]
    );
    assert_eq!(r.projects.iter().map(|p| p.id).collect::<Vec<_>>(), [4]);
    assert_eq!(r.groups.iter().map(|g| g.id).collect::<Vec<_>>(), [5]);
    assert_eq!(
        epic_keys(&r),
        [(7, 1), (5, 7)],
        "the group row names the epics' group; without a row the URL does"
    );

    let r = run_scoped_search(&h, "t", None, None, scope(&[], &["tea"])).await;
    assert!(
        r.work_items.is_empty() && r.projects.is_empty() && r.groups.is_empty(),
        "a shared prefix is not a group: {r:?}"
    );

    let r = run_scoped_search(&h, "oauth", None, None, scope(&[2], &["team"])).await;
    assert_eq!(
        iids(&r.work_items),
        [40, 10],
        "a project or a group: either passes"
    );
}

#[tokio::test]
async fn search_scope_applies_before_the_limit() {
    let (h, _dir) = dormant_handlers();
    seed_scoped_corpus(&h);
    let r = run_search(&h, "oauth", Some(vec![SearchKind::work_items]), Some(1)).await;
    assert_eq!(
        iids(&r.work_items),
        [40],
        "unscoped, the newer issue elsewhere wins the one slot"
    );
    let r = run_scoped_search(
        &h,
        "oauth",
        Some(vec![SearchKind::work_items]),
        Some(1),
        scope(&[1], &[]),
    )
    .await;
    assert_eq!(iids(&r.work_items), [10]);
}

#[tokio::test]
async fn search_empty_scope_is_no_scope() {
    let (h, _dir) = dormant_handlers();
    seed_scoped_corpus(&h);
    let unscoped = run_search(&h, "i", None, None).await;
    assert!(!issue_iids(&unscoped).is_empty() && !epics(&unscoped).is_empty());
    for empty in [
        scope(&[], &[]),
        Some(SearchScope {
            projects: None,
            groups: None,
        }),
    ] {
        assert_eq!(
            run_scoped_search(&h, "i", None, None, empty).await,
            unscoped
        );
    }
}

// ── Avatars ────────────────────────────────────────────────────────────

/// Avatars fetched for projects 1 and 4; project 2's fetch found none.
fn seed_avatars(h: &Handlers) {
    let avatar = |project_id, file: &str| Avatar {
        project_id,
        file: file.into(),
    };
    seed(
        h,
        &[avatar(1, "1-a.png"), avatar(2, ""), avatar(4, "4-b.svg")],
    );
}

fn avatar_path(dir: &tempfile::TempDir, file: &str) -> String {
    let path = dir.path().join("avatars").join(file);
    path.to_str().unwrap().to_string()
}

/// The stored project names its items; one without a row (a tracked project
/// the user is no member of) is named by their link.
#[tokio::test]
async fn items_carry_their_projects_path() {
    let (h, _dir) = dormant_handlers();
    seed_corpus(&h);
    seed_assigned_mrs(&h);

    let r = run_search(&h, "oauth", None, None).await;
    assert_eq!(r.work_items[0].namespace_path.as_deref(), Some("team/p"));
    assert_eq!(r.merge_requests[0].project_path.as_deref(), Some("team/p"));

    seed(
        &h,
        &[model::Project {
            id: 1,
            path_with_namespace: "team/moved".into(),
            ..Default::default()
        }],
    );
    let r = run_search(&h, "oauth", None, None).await;
    assert_eq!(
        r.work_items[0].namespace_path.as_deref(),
        Some("team/moved")
    );
    assert_eq!(
        r.merge_requests[0].project_path.as_deref(),
        Some("team/moved")
    );
    let paths: Vec<Option<String>> = assigned_mrs(&h, None)
        .await
        .into_iter()
        .map(|m| m.project_path)
        .collect();
    assert_eq!(
        paths,
        [Some("other/x".to_string()), Some("team/moved".to_string())]
    );
}

/// The rows name the files, so a read works without them on disk.
#[tokio::test]
async fn search_hits_carry_their_projects_avatar() {
    let (h, dir) = dormant_handlers();
    seed_corpus(&h);
    seed_avatars(&h);

    let r = run_search(&h, "oauth", None, None).await;
    let avatar = Some(avatar_path(&dir, "1-a.png"));
    assert_eq!(r.work_items[0].project_avatar, avatar);
    assert_eq!(r.merge_requests[0].project_avatar, avatar);
    let r = run_search(&h, "auth-serv", None, None).await;
    assert_eq!(r.projects[0].avatar, Some(avatar_path(&dir, "4-b.svg")));
}

#[tokio::test]
async fn assigned_items_carry_their_projects_avatar() {
    let (h, dir) = dormant_handlers();
    seed_assigned_issues(&h);
    seed_assigned_mrs(&h);
    seed_avatars(&h);

    let avatars = |project_avatars: Vec<(i64, Option<String>)>| -> Vec<(i64, Option<String>)> {
        let mut sorted = project_avatars;
        sorted.sort();
        sorted.dedup();
        sorted
    };
    let expected = [(1, Some(avatar_path(&dir, "1-a.png"))), (2, None)];
    let issues = assigned_work_items(&h, None).await;
    assert_eq!(
        avatars(
            issues
                .into_iter()
                .map(|i| (i.project_id.unwrap(), i.project_avatar))
                .collect()
        ),
        expected
    );
    let mrs = assigned_mrs(&h, None).await;
    assert_eq!(
        avatars(
            mrs.into_iter()
                .map(|m| (m.project_id, m.project_avatar))
                .collect()
        ),
        expected
    );
}

// ── History ────────────────────────────────────────────────────────────

#[tokio::test]
async fn get_history_merges_queued_and_synced_newest_first() {
    let (h, _dir) = unreachable_handlers();
    let now = now_secs();
    seed(
        &h,
        &[
            model::Timelog {
                id: 1,
                spent_at: now - 3 * 86_400,
                kind: Issuable::Issue,
                project_id: 7,
                iid: 1,
                title: "older".into(),
                time_spent: 5400,
                ..Default::default()
            },
            model::Timelog {
                id: 2,
                spent_at: now - 3600,
                kind: Issuable::MergeRequest,
                project_id: 7,
                iid: 2,
                title: "newer".into(),
                time_spent: 1800,
                ..Default::default()
            },
            model::Timelog {
                id: 3,
                spent_at: now - 30 * 86_400,
                iid: 1,
                ..Default::default()
            },
        ],
    );
    seed(
        &h,
        &[mr(
            7,
            5,
            "queued mr",
            "https://gl/g/p/-/merge_requests/5",
            1,
        )],
    );
    assert_eq!(post_time(&h, 7, 5, IssuableKind::merge_request).await, None);

    let events = history(&h, Some(7)).await;
    let titles: Vec<&str> = events.iter().filter_map(|e| e.title.as_deref()).collect();
    assert_eq!(
        titles,
        ["queued mr", "newer", "older"],
        "30 days back is outside"
    );
    assert_eq!(events[0].source, HistorySource::queued);
    assert_eq!(
        events[0].web_url.as_deref(),
        Some("https://gl/g/p/-/merge_requests/5")
    );
    // As it was given: the daemon doesn't know GitLab's day or week.
    assert_eq!(
        (events[0].time_spent, events[0].duration.as_deref()),
        (None, Some("30m"))
    );
    assert_eq!(
        (events[1].time_spent, events[1].duration.as_deref()),
        (Some(1800), None)
    );
    assert_eq!(events[1].kind, IssuableKind::merge_request);
    assert_eq!(events[2].time_spent, Some(5400));
    let json = serde_json::to_value(&events[0]).unwrap();
    assert!(json.get("time_spent").is_none(), "{json}");
    let json = serde_json::to_value(&events[2]).unwrap();
    assert!(json.get("duration").is_none(), "{json}");
}

/// A queued entry whose item isn't stored has nothing to name it by, and one
/// without a summary has none: those fields are left out.
#[tokio::test]
async fn a_queued_entry_leaves_out_what_the_daemon_does_not_know() {
    let (h, _dir) = unreachable_handlers();
    assert_eq!(post_time(&h, 8, 1, IssuableKind::work_item).await, None);
    let events = history(&h, Some(1)).await;
    assert_eq!(events.len(), 1);
    let json = serde_json::to_value(&events[0]).unwrap();
    for absent in ["title", "web_url", "summary", "time_spent"] {
        assert!(json.get(absent).is_none(), "{absent} in {json}");
    }
    assert_eq!(json["duration"], "30m");
}

// ── Activity ───────────────────────────────────────────────────────────

fn event_at(id: i64, project_id: i64, action: &str, created_at: u64) -> model::Event {
    model::Event {
        id,
        project_id,
        action_name: action.into(),
        created_at,
        ..Default::default()
    }
}

/// Served from the store alone, also while dormant; the project and the
/// stored item give each event its path and link.
#[tokio::test]
async fn get_activity_is_newest_first_in_the_window_and_linked() {
    let (h, _dir) = dormant_handlers();
    let now = now_secs();
    seed(
        &h,
        &[model::Project {
            id: 1,
            path_with_namespace: "team/api".into(),
            web_url: "https://gl/team/api".into(),
            ..Default::default()
        }],
    );
    seed(&h, &[issue(1, 1, "api", "https://gl/team/api/-/issues/1")]);
    let mut closed = event_at(1, 1, "closed", now - 3 * 86_400);
    closed.target_type = "Issue".into();
    closed.target_iid = 1;
    closed.target_title = "api".into();
    let mut pushed = event_at(2, 1, "pushed to", now - 3600);
    pushed.push_data = model::PushData {
        git_ref: "main".into(),
        commit_count: 2,
        commit_title: "Fix it".into(),
    };
    let mut elsewhere = event_at(3, 9, "commented on", now - 60);
    elsewhere.note = model::NoteRef {
        body: "lgtm".into(),
        noteable_type: "MergeRequest".into(),
        noteable_iid: 4,
    };
    seed(
        &h,
        &[
            closed,
            pushed,
            elsewhere,
            event_at(4, 1, "opened", now - 30 * 86_400),
        ],
    );
    mark_synced(&h, &[Job::Events]);

    let events = activity(&h, None).await;
    let actions: Vec<&str> = events.iter().map(|e| e.action.as_str()).collect();
    assert_eq!(
        actions,
        ["commented on", "pushed to", "closed"],
        "30 days back is outside the default week"
    );
    assert_eq!(events[0].project_path, None, "project 9 is not stored");
    assert_eq!(events[0].web_url, None);
    assert_eq!(events[0].target_iid, Some(4));
    assert_eq!(events[0].description.as_deref(), Some("lgtm"));
    assert_eq!(events[1].r#ref.as_deref(), Some("main"));
    assert_eq!(events[1].description.as_deref(), Some("Fix it"));
    assert_eq!(events[2].description, None);
    assert_eq!(events[1].commit_count, Some(2));
    assert_eq!(events[1].project_path.as_deref(), Some("team/api"));
    assert_eq!(
        events[2].web_url.as_deref(),
        Some("https://gl/team/api/-/issues/1")
    );
    assert_eq!(events[2].target_title.as_deref(), Some("api"));

    assert_eq!(activity(&h, Some(60)).await.len(), 4);
    assert_eq!(activity(&h, Some(0)).await.len(), 0);
}

/// Never synced: dormant says why, connected replies empty — and neither
/// reads GitLab.
#[tokio::test]
async fn get_activity_never_reads_through() {
    let (h, _dir) = dormant_handlers();
    let mut call = AsyncCall::default();
    h.get_activity(&mut call as &mut dyn Call_GetActivity, None)
        .await
        .unwrap();
    assert_eq!(reply_error(&mut call).as_deref(), Some(NOT_AUTHENTICATED));

    let fake = Arc::new(FakeGitlab::default());
    fake.serve("events", vec![event_json(1, 7, "opened", now_secs())]);
    let (h, _dir) = connected_handlers(&fake);
    assert!(activity(&h, None).await.is_empty());
    assert_eq!(fake.read_calls(), 0);
}

// ── ClearCache ─────────────────────────────────────────────────────────

fn timelog_at(id: u64, spent_at: u64) -> model::Timelog {
    model::Timelog {
        id,
        spent_at,
        iid: 1,
        ..Default::default()
    }
}

#[tokio::test]
async fn clear_cache_history_bands_split_at_the_windows() {
    let (h, _dir) = dormant_handlers();
    let now = now_secs();
    let seed_bands = |h: &Handlers| {
        seed(
            h,
            &[
                timelog_at(1, now - 3600),
                timelog_at(2, now - 10 * 86_400),
                timelog_at(3, now - 100 * 86_400),
            ],
        )
    };
    let ids = |h: &Handlers| -> Vec<u64> {
        h.sync
            .store()
            .timelogs
            .scan(RowScope::All)
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect()
    };

    seed_bands(&h);
    clear_cache(&h, Some(vec![CacheScope::quick])).await;
    assert_eq!(ids(&h), [3, 2]);

    seed_bands(&h);
    clear_cache(&h, Some(vec![CacheScope::slow])).await;
    assert_eq!(ids(&h), [3, 1]);

    seed_bands(&h);
    clear_cache(&h, Some(vec![CacheScope::stale])).await;
    assert_eq!(ids(&h), [2, 1]);
}

#[tokio::test]
async fn clear_cache_scopes_drop_their_slice_and_reset_its_jobs() {
    let (h, _dir) = dormant_handlers();
    seed_assigned_issues(&h);
    seed_recent_issues(&h);
    seed_corpus(&h);

    clear_cache(&h, Some(vec![CacheScope::assigned])).await;
    assert!(h.sync.store().view(ASSIGNED_ISSUES).unwrap().is_none());
    assert!(!h.sync.has_synced(Job::AssignedIssues));
    for (job, name) in [
        (Job::RecentAuthoredIssues, RECENT_AUTHORED_ISSUES),
        (Job::RecentAssignedIssues, RECENT_ASSIGNED_ISSUES),
    ] {
        assert!(h.sync.store().view(name).unwrap().is_none(), "{name}");
        assert!(!h.sync.has_synced(job), "{name}");
    }
    assert!(h.sync.has_synced(Job::MemberProjects), "search untouched");

    clear_cache(&h, Some(vec![CacheScope::search])).await;
    assert!(
        h.sync
            .store()
            .issues
            .scan(RowScope::All)
            .unwrap()
            .is_empty()
    );
    assert!(
        h.sync
            .store()
            .projects
            .scan(RowScope::All)
            .unwrap()
            .is_empty()
    );
    assert!(!h.sync.has_synced(Job::MemberProjects));
}

#[tokio::test]
async fn clear_cache_usage_only_when_listed() {
    let (h, _dir) = dormant_handlers();
    run_record_open(&h, 1, 10, IssuableKind::work_item).await;
    clear_cache(&h, None).await;
    assert!(
        !h.usage.snapshot().unwrap().entries.is_empty(),
        "user data, not a cache"
    );
    clear_cache(&h, Some(vec![CacheScope::usage])).await;
    assert!(h.usage.snapshot().unwrap().entries.is_empty());
}

/// Connected: the reply waits for the foreground views and the history to
/// refill, so a `forskap sync refresh` followed by `forskap issue list` shows fresh data.
#[tokio::test]
async fn clear_cache_refills_the_foreground_before_replying() {
    let fake = Arc::new(FakeGitlab::default());
    fake.serve("issues", vec![issue_json(7, 1, "fresh")]);
    let (h, _dir) = connected_handlers(&fake);
    seed_assigned_issues(&h);

    clear_cache(&h, None).await;
    let issues = assigned_work_items(&h, None).await;
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].title, "fresh");
    assert_eq!(fake.calls_to("merge_requests").len(), 1);
    assert_eq!(fake.timelog_calls().len(), 2, "recent and full history");
}

/// Only the refill of what a scope cleared is awaited: open statistics are
/// not synced, so clearing them makes no call.
#[tokio::test]
async fn clear_cache_refills_only_what_it_cleared() {
    let fake = Arc::new(FakeGitlab::default());
    let (h, _dir) = connected_handlers(&fake);

    clear_cache(&h, Some(vec![CacheScope::usage])).await;
    assert_eq!(fake.read_calls(), 0);

    clear_cache(&h, Some(vec![CacheScope::assigned])).await;
    assert_eq!(fake.calls_to("issues").len(), 1);
    assert!(fake.timelog_calls().is_empty(), "history untouched");
}

/// Board columns of freshly assigned projects land before the refill
/// replies, so `forskap issue list` right after `forskap sync refresh` shows them.
#[tokio::test]
async fn clear_cache_waits_for_new_board_columns() {
    let fake = Arc::new(FakeGitlab::default());
    let mut doing = issue_json(7, 1, "wip");
    doing["labels"] = serde_json::json!(["Doing"]);
    fake.serve("issues", vec![doing]);
    fake.serve(
        "projects/7/boards",
        vec![serde_json::json!({"id": 3, "lists": [{"label": {"name": "Doing"}}]})],
    );
    let (h, _dir) = connected_handlers(&fake);

    clear_cache(&h, Some(vec![CacheScope::assigned])).await;
    let issues = assigned_work_items(&h, None).await;
    assert_eq!(issues[0].graph_status, "Doing");
}

// ── Dead letters ───────────────────────────────────────────────────────

/// An id the dead-letter store doesn't hold is the daemon's, not GitLab's.
#[tokio::test]
async fn an_unknown_failure_id_is_not_found() {
    let (h, _dir) = dormant_handlers();
    let mut call = AsyncCall::default();
    h.retry_failure(&mut call as &mut dyn Call_RetryFailure, 999)
        .await
        .unwrap();
    assert_eq!(reply_error(&mut call).as_deref(), Some(NOT_FOUND));
    let mut call = AsyncCall::default();
    h.dismiss_failure(&mut call as &mut dyn Call_DismissFailure, 999)
        .await
        .unwrap();
    let (name, args) = reply_error_with(&mut call).unwrap();
    assert_eq!(name, NOT_FOUND);
    assert_eq!(args["message"], "no failed task with id 999");
}

// ── Session ────────────────────────────────────────────────────────────

/// Without a keychain both are switched off: the daemon's own refusal,
/// before GitLab is asked.
#[tokio::test]
async fn login_and_logout_without_a_keychain_are_internal() {
    let fake = Arc::new(FakeGitlab::default());
    let (h, _dir) = connected_handlers(&fake);
    let mut call = AsyncCall::default();
    h.login(
        &mut call as &mut dyn Call_Login,
        "gitlab.invalid".into(),
        "glpat-x".into(),
    )
    .await
    .unwrap();
    let (name, args) = reply_error_with(&mut call).unwrap();
    assert_eq!(name, INTERNAL);
    assert!(
        args["message"]
            .as_str()
            .unwrap()
            .starts_with("logging in is disabled"),
        "{args}"
    );
    let mut call = AsyncCall::default();
    h.logout(&mut call as &mut dyn Call_Logout).await.unwrap();
    assert_eq!(reply_error(&mut call).as_deref(), Some(INTERNAL));
    assert!(matches!(&*h.session.read().await, ConnState::Connected(_)));
    assert_eq!(fake.read_calls(), 0);
}

// ── WhoAmI ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn get_sync_jobs_lists_the_plan_while_dormant() {
    let (h, _dir) = dormant_handlers();
    let mut call = AsyncCall::default();
    h.get_sync_jobs(&mut call as &mut dyn Call_GetSyncJobs)
        .await
        .unwrap();
    let reply = reply::<GetSyncJobs_Reply>(&mut call);

    assert_eq!(reply.paused_until, None);
    let job = reply
        .jobs
        .iter()
        .find(|j| j.key == ASSIGNED_ISSUES)
        .expect("the assigned issues are always planned");
    assert!(matches!(job.status, SyncJobStatus::due));
    // Never ran: no times to report rather than the epoch.
    assert_eq!(
        (job.last_ok, job.next_due, job.running_since),
        (None, None, None)
    );
    assert_eq!((job.failures, job.last_error.as_deref()), (0, None));
    assert_eq!(job.unavailable, Some(false));
    // Progress is a running job's.
    assert_eq!((job.full, job.fetched, job.expected), (None, None, None));
}

/// A fetch in flight says how far it is: the rows GitLab announced, none of
/// them here yet. The assigned list has no delta, so no `full` either.
#[tokio::test]
async fn get_sync_jobs_reports_a_running_jobs_progress() {
    let fake = Arc::new(FakeGitlab::default());
    fake.serve("issues", vec![issue_json(7, 1, "assigned")]);
    let gate = fake.gate("issues");
    let (h, _dir) = connected_handlers(&fake);
    h.sync.refresh_soon(&[Job::AssignedIssues]);
    tokio::time::timeout(Duration::from_secs(2), fake.gated.notified())
        .await
        .expect("the assigned issues fetch starts");

    let mut call = AsyncCall::default();
    h.get_sync_jobs(&mut call as &mut dyn Call_GetSyncJobs)
        .await
        .unwrap();
    let reply = reply::<GetSyncJobs_Reply>(&mut call);
    let job = reply
        .jobs
        .iter()
        .find(|j| j.key == ASSIGNED_ISSUES)
        .expect("the assigned issues");
    assert!(matches!(job.status, SyncJobStatus::running), "{job:?}");
    assert_eq!(
        (job.full, job.fetched, job.expected),
        (None, Some(0), Some(1))
    );
    gate.notify_one();
}

#[tokio::test]
async fn get_sync_jobs_reports_a_failed_job() {
    let fake = Arc::new(FakeGitlab::default());
    fake.fail_next("issues", FakeErr::Rejected);
    let (h, _dir) = connected_handlers(&fake);
    h.sync.refresh_now(&[Job::AssignedIssues]).await;

    let mut call = AsyncCall::default();
    h.get_sync_jobs(&mut call as &mut dyn Call_GetSyncJobs)
        .await
        .unwrap();
    let reply = reply::<GetSyncJobs_Reply>(&mut call);
    let job = reply
        .jobs
        .iter()
        .find(|j| j.key == ASSIGNED_ISSUES)
        .expect("the assigned issues");
    assert!(matches!(job.status, SyncJobStatus::backing_off));
    assert_eq!(job.failures, 1);
    assert!(job.next_due.is_some_and(|at| at > now_secs() as i64));
    assert!(job.last_error.is_some());
    assert_eq!(
        job.unavailable,
        Some(false),
        "an account-wide list never is"
    );
}

/// A job GitLab refused three times in a row is unavailable on the wire:
/// `waiting` for its next try about a day out, its failures and error kept.
/// Every other job says `false`, so a client tells this daemon from one too
/// old to say.
#[tokio::test]
async fn get_sync_jobs_reports_an_unavailable_job() {
    let fake = Arc::new(FakeGitlab::default());
    fake.serve("issues", vec![issue_json(7, 1, "assigned")]);
    for _ in 0..3 {
        fake.fail_next("projects/7/boards", FakeErr::Rejected);
    }
    let (h, _dir) = connected_handlers(&fake);
    // The assigned list plans its project's boards and waits for them: the
    // first refusal.
    h.sync.refresh_now(&[Job::AssignedIssues]).await;
    for _ in 0..2 {
        h.sync.refresh_now(&[Job::ProjectBoards(7)]).await;
    }
    assert_eq!(fake.calls_to("projects/7/boards").len(), 3);

    let mut call = AsyncCall::default();
    h.get_sync_jobs(&mut call as &mut dyn Call_GetSyncJobs)
        .await
        .unwrap();
    let reply = reply::<GetSyncJobs_Reply>(&mut call);
    let find = |key: &str| {
        let job = reply.jobs.iter().find(|j| j.key == key);
        job.unwrap_or_else(|| panic!("{key} in {:?}", reply.jobs))
    };
    let boards = find("project/7/boards");
    assert_eq!(boards.unavailable, Some(true), "{boards:?}");
    assert!(
        matches!(boards.status, SyncJobStatus::waiting),
        "{boards:?}"
    );
    assert_eq!(boards.failures, 3);
    let six_hours = 6 * 3600;
    assert!(
        boards
            .next_due
            .is_some_and(|at| at > now_secs() as i64 + six_hours),
        "{boards:?}"
    );
    assert!(
        boards
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("403"))
    );
    assert_eq!(find(ASSIGNED_ISSUES).unavailable, Some(false));
}

async fn who_am_i(h: &Handlers) -> WhoAmI_Reply {
    let mut call = AsyncCall::default();
    h.who_am_i(&mut call as &mut dyn Call_WhoAmI).await.unwrap();
    reply(&mut call)
}

#[tokio::test]
async fn who_am_i_reports_the_token_once_it_is_known() {
    let fake = Arc::new(FakeGitlab::default());
    let (h, _dir) = connected_handlers(&fake);

    let me = who_am_i(&h).await;
    assert_eq!(
        (me.host.as_str(), me.user_id, me.username.as_str()),
        ("gitlab.test", 1, "tester")
    );
    assert_eq!((me.token_expires_at, me.token_rotates), (None, false));

    let expires = chrono::Utc::now().date_naive() + chrono::Days::new(30);
    let info = crate::gitlab::TokenInfo {
        scopes: vec!["self_rotate".into()],
        created_at: Some(chrono::Utc::now()),
        expires_at: Some(expires),
    };
    let client: Arc<dyn crate::gitlab::GitlabApi> = fake.clone();
    h.rotation.publish(&client, &info);

    let me = who_am_i(&h).await;
    let midnight = expires.and_time(chrono::NaiveTime::MIN).and_utc();
    assert_eq!(me.token_expires_at, Some(midnight.timestamp()));
    assert!(me.token_rotates);

    // Read from the config at each call, so a reload shows at once.
    h.config.write().unwrap().auth.rotate = crate::config::RotatePolicy::Never;
    assert!(!who_am_i(&h).await.token_rotates);
    assert_eq!(fake.read_calls() + fake.token_info_calls(), 0);
}

// ── GetStatus ──────────────────────────────────────────────────────────

async fn get_status(h: &Handlers) -> GetStatus_Reply {
    let mut call = AsyncCall::default();
    h.get_status(&mut call as &mut dyn Call_GetStatus)
        .await
        .unwrap();
    reply(&mut call)
}

#[tokio::test]
async fn get_status_names_the_account_while_connected() {
    let fake = Arc::new(FakeGitlab::default());
    let (h, _dir) = connected_handlers(&fake);
    let status = get_status(&h).await;
    assert_eq!(status.api_version, forskap_api::API_VERSION);
    assert_eq!(status.daemon_version, env!("CARGO_PKG_VERSION"));
    assert!(status.connected);
    assert_eq!(
        (status.host, status.username, status.user_id),
        (Some("gitlab.test".into()), Some("tester".into()), Some(1))
    );
    assert_eq!((status.reason, status.detail), (None, None));
    assert_eq!(fake.read_calls() + fake.token_info_calls(), 0);
}

/// Dormant it says why, as `NotAuthenticated` would, and no account.
#[tokio::test]
async fn get_status_says_why_while_dormant() {
    let (h, _dir) = unreachable_handlers();
    let status = get_status(&h).await;
    assert_eq!(status.api_version, forskap_api::API_VERSION);
    assert!(!status.connected);
    assert_eq!(status.reason, Some(forskap_api::NotAuthReason::unreachable));
    assert_eq!(
        status.detail.as_deref(),
        Some("gitlab.test: connection refused")
    );
    assert_eq!(
        (status.host, status.username, status.user_id),
        (None, None, None)
    );

    let (h, _dir) = dormant_handlers();
    let status = get_status(&h).await;
    assert_eq!(
        (status.reason, status.detail),
        (Some(forskap_api::NotAuthReason::no_credentials), None)
    );
}

// ── Properties ─────────────────────────────────────────────────────────
//
// For arbitrary arguments a handler must always reply (never panic), reject
// invalid input with the documented error, and never let a read touch
// GitLab. proptest bodies are sync, so each case runs on a small runtime;
// every case builds the real handler stack on a tempdir, so counts stay low.

use proptest::prelude::*;

fn prop_rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn search_replies_or_rejects_any_arguments_without_touching_gitlab(
        query in ".{0,12}",
        kinds in proptest::option::of(proptest::collection::vec(
            proptest::sample::select(vec![
                SearchKind::work_items,
                SearchKind::merge_requests,
                SearchKind::projects,
                SearchKind::groups,
            ]),
            0..3,
        )),
        limit in proptest::option::of(any::<i64>()),
        types in proptest::option::of(proptest::collection::vec(
            proptest::sample::select(vec!["issue", "Epic", "task", "", "?"]),
            0..3,
        )),
        excluded in proptest::option::of(proptest::collection::vec(
            proptest::sample::select(vec!["issue", "EPIC", "task", "", "?"]),
            0..3,
        )),
    ) {
        prop_rt().block_on(async {
            let fake = Arc::new(FakeGitlab::default());
            let (h, _dir) = connected_handlers(&fake);
            seed_corpus(&h);

            let names = |list: Option<Vec<&str>>| list.map(|t| t.into_iter().map(str::to_string).collect());
            let (types, excluded) = (names(types), names(excluded));
            let mut call = AsyncCall::default();
            h.search(&mut call as &mut dyn Call_Search, query.clone(), kinds.clone(), limit, None, types, excluded)
                .await
                .unwrap();
            let error = reply_error(&mut call);

            if matches!(limit, Some(n) if n <= 0) {
                assert_eq!(error.as_deref(), Some(INVALID_ARGUMENT), "bad args are rejected eagerly");
            } else {
                assert_eq!(error, None, "valid args succeed");
            }
            assert_eq!(fake.read_calls(), 0, "a read never touches GitLab");
        });
    }

    /// Results come from the stored corpus, every hit matches, newest wins,
    /// the per-kind limit holds.
    #[test]
    fn search_filters_orders_and_limits_against_the_model(
        titles in proptest::collection::vec("[a-e]{2,5}", 1..8),
        needle in "[a-e]{1,3}",
        limit in 1i64..4,
    ) {
        prop_rt().block_on(async {
            let (h, _dir) = dormant_handlers();
            let issues: Vec<model::Issue> = titles
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let mut row = issue(1, i as i64 + 1, t, "https://gl/g/p/-/issues/1");
                    row.updated_at = 1_000 - i as u64;
                    row
                })
                .collect();
            seed(&h, &issues);
            mark_synced(&h, &[Job::MemberProjects]);

            let r = run_search(&h, &needle, Some(vec![SearchKind::work_items]), Some(limit)).await;
            let expected: Vec<i64> = issues
                .iter()
                .filter(|i| i.title.contains(&needle))
                .map(|i| i.id)
                .take(limit as usize)
                .collect();
            assert_eq!(r.work_items.iter().map(|i| i.id).collect::<Vec<_>>(), expected);
        });
    }

    /// Invalid input is rejected eagerly, valid input on a no-credentials
    /// session gets the honest auth error, and neither queues anything.
    #[test]
    fn post_time_never_queues_from_a_no_credentials_session(
        project_id in -2i64..6,
        iid in -2i64..6,
        duration in prop_oneof!["[0-9]{1,3}[smhdw]", "[a-z]{0,4}", "[0-9]{1,2}x"],
    ) {
        prop_rt().block_on(async {
            let (h, _dir) = dormant_handlers();
            let mut call = AsyncCall::default();
            h.post_time(
                &mut call as &mut dyn Call_PostTime,
                project_id,
                iid,
                IssuableKind::work_item,
                duration.clone(),
                None,
            )
            .await
            .unwrap();

            let valid = issue_ref_error(project_id, iid).is_none() && looks_like_duration(&duration);
            let expected = if valid { NOT_AUTHENTICATED } else { INVALID_ARGUMENT };
            assert_eq!(reply_error(&mut call).as_deref(), Some(expected));
            assert!(h.queue.pending().unwrap().is_empty());
        });
    }
}
