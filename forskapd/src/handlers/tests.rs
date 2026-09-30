use super::*;

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Notify, RwLock};

use forskap_api::{
    AsyncCall, Call_ClearCache, Call_Close, Call_GetActivity, Call_GetAssignedIssues,
    Call_GetAssignedMergeRequests, Call_GetHistory, Call_GetSyncJobs, Call_PostTime,
    Call_RecordEpicOpen, Call_RecordOpen, Call_Search, Call_UnassignSelf, Call_WhoAmI,
    GetActivity_Reply, GetAssignedIssues_Reply, GetAssignedMergeRequests_Reply, GetHistory_Reply,
    GetSyncJobs_Reply, IssuableKind, Issue, MergeRequest, Search_Reply, SyncJobStatus,
    VarlinkInterface, WhoAmI_Reply,
};

use crate::config::SharedConfig;
use crate::error::DormancyReason;
use crate::gitlab::Issuable;
use crate::queue::RetryQueue;
use crate::sync::avatars::Avatar;
use crate::sync::jobs::{ASSIGNED_ISSUES, ASSIGNED_MERGE_REQUESTS};
use crate::sync::model::{self, Board, BoardList, LabelRef, RowKey, UserRef};
use crate::sync::schedule::JobState;
use crate::sync::store::{RowScope, Stored, SyncStore, View};
use crate::sync::{Job, SyncHandle};
use crate::testing::{FakeErr, FakeGitlab, event_json, eventually, issue_json};
use crate::write::{Write, WriteOp};

const NOT_AUTHENTICATED: &str = "org.thehoster.forskapd.NotAuthenticated";
const GITLAB_ERROR: &str = "org.thehoster.forskapd.GitlabError";

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

/// Three assigned issues: two under `team` (one in a subgroup), one under
/// `other`, listed in that GitLab order.
fn epic(group_id: i64, iid: i64, title: &str, updated_at: u64) -> model::Epic {
    model::Epic {
        id: group_id * 1000 + iid,
        iid,
        group_id,
        title: title.into(),
        web_url: format!("https://gl/groups/team/-/epics/{iid}"),
        state: "opened".into(),
        updated_at,
        ..Default::default()
    }
}

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

/// Issues "OAuth token refresh" (1/10) and a labeled one (1/20), MR "Fix
/// oauth flow" (1/30), project `team/auth-service`, group `team` with the
/// epics "Identity roadmap" (5/7) and "Billing" (5/8).
fn seed_corpus(h: &Handlers) {
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
    call.take_reply()
        .expect("a reply")
        .error
        .map(|e| e.to_string())
}

async fn assigned_issues(h: &Handlers, groups: Option<Vec<String>>) -> Vec<Issue> {
    let mut call = AsyncCall::default();
    h.get_assigned_issues(&mut call as &mut dyn Call_GetAssignedIssues, groups)
        .await
        .unwrap();
    reply::<GetAssignedIssues_Reply>(&mut call).issues
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
    kinds: Option<Vec<String>>,
    limit: Option<i64>,
) -> Search_Reply {
    let mut call = AsyncCall::default();
    h.search(
        &mut call as &mut dyn Call_Search,
        query.to_string(),
        kinds,
        limit,
    )
    .await
    .unwrap();
    reply(&mut call)
}

async fn run_record_open(h: &Handlers, project_id: i64, iid: i64, kind: IssuableKind) {
    let mut call = AsyncCall::default();
    h.record_open(&mut call as &mut dyn Call_RecordOpen, project_id, iid, kind)
        .await
        .unwrap();
    assert_eq!(reply_error(&mut call), None);
}

async fn run_record_epic_open(h: &Handlers, group_id: i64, iid: i64) -> Option<String> {
    let mut call = AsyncCall::default();
    h.record_epic_open(&mut call as &mut dyn Call_RecordEpicOpen, group_id, iid)
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

async fn clear_cache(h: &Handlers, scope: Option<Vec<String>>) {
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

#[tokio::test]
async fn close_rejects_bad_issuable_ref() {
    let (h, _dir) = dormant_handlers();
    assert_eq!(
        close(&h, 0, 42, IssuableKind::issue).await.as_deref(),
        Some(GITLAB_ERROR)
    );
}

// ── Writes ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn post_time_queues_through_an_unreachable_outage() {
    let (h, _dir) = unreachable_handlers();
    assert_eq!(post_time(&h, 7, 42, IssuableKind::issue).await, None);
    assert_eq!(h.queue.pending().unwrap().len(), 1, "drains on reconnect");
}

#[tokio::test]
async fn post_time_rejects_when_dormant_but_not_unreachable() {
    let (h, _dir) = dormant_handlers();
    assert_eq!(
        post_time(&h, 7, 42, IssuableKind::issue).await.as_deref(),
        Some(NOT_AUTHENTICATED),
        "no credentials: queuing wouldn't help"
    );
    assert!(h.queue.pending().unwrap().is_empty());
}

/// A 429 is refused before GitLab does any work, so even a PostTime is safe
/// to queue; a 5xx may already have booked the time, so it is reported.
#[tokio::test]
async fn post_time_queues_rate_limits_but_reports_server_errors() {
    for (status, queued) in [(429, true), (502, false)] {
        let fake = Arc::new(FakeGitlab::default());
        fake.fail_next_write(FakeErr::Throttled(status));
        let (h, _dir) = connected_handlers(&fake);
        let error = post_time(&h, 7, 42, IssuableKind::issue).await;
        assert_eq!(error.is_none(), queued, "{status}");
        assert_eq!(
            h.queue.pending().unwrap().len(),
            usize::from(queued),
            "{status}"
        );
    }
}

#[tokio::test]
async fn close_queues_through_a_server_error() {
    let fake = Arc::new(FakeGitlab::default());
    fake.fail_next_write(FakeErr::Throttled(503));
    let (h, _dir) = connected_handlers(&fake);
    assert_eq!(close(&h, 7, 42, IssuableKind::issue).await, None);
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

    assert_eq!(post_time(&h, 7, 42, IssuableKind::issue).await, None);
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

    assert_eq!(close(&h, 7, 1, IssuableKind::issue).await, None);
    assert_eq!(fake.writes(), [("close", Issuable::Issue, 7, 1)]);
    eventually("the assigned-issue refresh", || {
        !fake.calls_to("issues").is_empty()
    })
    .await;
}

// ── Read-time overlay of writes ────────────────────────────────────────

#[tokio::test]
async fn a_queued_close_hides_the_issue_until_it_settles() {
    let (h, _dir) = unreachable_handlers();
    seed_assigned_issues(&h);

    assert_eq!(close(&h, 1, 1, IssuableKind::issue).await, None);
    let iids: Vec<i64> = assigned_issues(&h, None)
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
    let iids = |v: Vec<Issue>| v.iter().map(|i| i.iid).collect::<Vec<_>>();
    assert_eq!(iids(assigned_issues(&h, None).await), [3, 2]);

    // A view fetched after the write reflects GitLab, including the write.
    seed_view(
        &h,
        ASSIGNED_ISSUES,
        &[(2, 3), (1, 1), (1, 2)],
        now_secs() + 5,
    );
    assert_eq!(iids(assigned_issues(&h, None).await), [3, 1, 2]);
}

#[tokio::test]
async fn a_queued_mr_unassign_hides_the_merge_request() {
    let (h, _dir) = unreachable_handlers();
    seed_assigned_mrs(&h);

    let mut call = AsyncCall::default();
    h.unassign_self(
        &mut call as &mut dyn Call_UnassignSelf,
        2,
        11,
        IssuableKind::merge_request,
    )
    .await
    .unwrap();
    assert_eq!(reply_error(&mut call), None);

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

    let iids: Vec<i64> = assigned_issues(&h, None)
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
async fn get_assigned_issues_serves_the_view_while_dormant_grouped_by_namespace() {
    let (h, _dir) = dormant_handlers();
    seed_assigned_issues(&h);
    let iids: Vec<i64> = assigned_issues(&h, None)
        .await
        .iter()
        .map(|i| i.iid)
        .collect();
    assert_eq!(iids, [3, 1, 2], "other/x, team/api, team/sub/web");
}

#[tokio::test]
async fn get_assigned_issues_filters_by_group_and_subgroups() {
    let (h, _dir) = dormant_handlers();
    seed_assigned_issues(&h);
    let iids = |v: Vec<Issue>| v.iter().map(|i| i.iid).collect::<Vec<_>>();
    assert_eq!(
        iids(assigned_issues(&h, Some(vec!["team".into()])).await),
        [1, 2]
    );
    assert_eq!(
        iids(assigned_issues(&h, Some(vec!["team/sub".into(), "team".into()])).await),
        [1, 2],
        "overlapping groups list each issue once"
    );
    assert_eq!(
        iids(assigned_issues(&h, Some(vec!["tea".into()])).await),
        Vec::<i64>::new(),
        "a shared prefix is not a group"
    );
}

#[tokio::test]
async fn get_assigned_issues_never_synced_is_honest_about_the_session() {
    let (h, _dir) = dormant_handlers();
    let mut call = AsyncCall::default();
    h.get_assigned_issues(&mut call as &mut dyn Call_GetAssignedIssues, None)
        .await
        .unwrap();
    assert_eq!(reply_error(&mut call).as_deref(), Some(NOT_AUTHENTICATED));

    let fake = Arc::new(FakeGitlab::default());
    let (h, _dir) = connected_handlers(&fake);
    assert!(
        assigned_issues(&h, None).await.is_empty(),
        "connected: the first sync is pending"
    );
    assert_eq!(fake.read_calls(), 0, "reads never fetch");
}

#[tokio::test]
async fn get_assigned_issues_overlays_open_counts_and_board_status() {
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
    run_record_open(&h, 1, 1, IssuableKind::issue).await;

    let issues = assigned_issues(&h, None).await;
    let api = issues.iter().find(|i| i.iid == 1).unwrap();
    assert_eq!(api.open_count, 1);
    assert_eq!(api.graph_status, "Doing");
    let other = issues.iter().find(|i| i.iid == 3).unwrap();
    assert_eq!(other.graph_status, "", "project 2's boards never synced");
}

// ── Assigned merge requests ────────────────────────────────────────────

#[tokio::test]
async fn get_assigned_merge_requests_serves_newest_first_with_group_filter() {
    let (h, _dir) = dormant_handlers();
    seed_assigned_mrs(&h);
    let all = assigned_mrs(&h, None).await;
    assert_eq!(all.iter().map(|m| m.iid).collect::<Vec<_>>(), [11, 10]);
    assert_eq!(all[0].assignees, ["me"]);
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
    assert_eq!(r.issues.iter().map(|i| i.iid).collect::<Vec<_>>(), [10]);
    assert_eq!(r.merge_requests.len(), 1);
    assert!(r.projects.is_empty() && r.groups.is_empty());

    let r = run_search(&h, "backend", None, None).await;
    assert_eq!(
        r.issues.iter().map(|i| i.iid).collect::<Vec<_>>(),
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
    assert!(r.issues.is_empty());
    assert_eq!(r.merge_requests[0].iid, 30);
}

#[tokio::test]
async fn search_finds_epics_by_title_label_and_reference() {
    let (h, _dir) = dormant_handlers();
    seed_corpus(&h);
    let mut labeled = epic(6, 7, "Q3", 50);
    labeled.labels = vec!["Roadmap".into()];
    seed(&h, &[labeled]);

    let r = run_search(&h, "ROADMAP", None, None).await;
    let hits: Vec<_> = r.epics.iter().map(|e| (e.group_id, e.iid)).collect();
    assert_eq!(hits, [(5, 7), (6, 7)], "title and label, newest first");
    assert_eq!(r.epics[0].web_url, "https://gl/groups/team/-/epics/7");
    assert!(r.issues.is_empty() && r.groups.is_empty());

    // `&7` is the epic reference; `#7` stays with issues and MRs.
    let r = run_search(&h, "&7", None, None).await;
    assert_eq!(r.epics.len(), 2, "one per group");
    assert!(r.issues.is_empty() && r.merge_requests.is_empty());
    assert!(run_search(&h, "#7", None, None).await.epics.is_empty());

    let r = run_search(&h, "i", Some(vec!["epics".into()]), Some(1)).await;
    assert_eq!(r.epics.iter().map(|e| e.iid).collect::<Vec<_>>(), [8]);
    assert!(r.issues.is_empty() && r.projects.is_empty());
    let r = run_search(&h, "i", Some(vec!["issues".into()]), None).await;
    assert!(r.epics.is_empty(), "not asked for");
}

/// An epic's opens are counted by its group, apart from the issue of a
/// project sharing id and number.
#[tokio::test]
async fn record_epic_open_ranks_the_epic_and_nothing_else() {
    let (h, _dir) = dormant_handlers();
    seed_corpus(&h);
    seed(
        &h,
        &[issue(5, 7, "same ids", "https://gl/team/p/-/issues/7")],
    );
    assert_eq!(run_record_epic_open(&h, 5, 7).await, None);

    let r = run_search(&h, "", None, None).await;
    assert_eq!(r.epics.iter().map(|e| e.iid).collect::<Vec<_>>(), [7]);
    assert_eq!(r.epics[0].open_count, 1);
    assert!(r.issues.is_empty(), "the issue 5/7 was never opened");

    let r = run_search(&h, "i", Some(vec!["epics".into()]), None).await;
    assert_eq!(
        r.epics.iter().map(|e| e.iid).collect::<Vec<_>>(),
        [7, 8],
        "the opened older epic outranks the newer one"
    );

    for (group_id, iid) in [(0, 7), (5, 0), (-1, 1)] {
        let error = run_record_epic_open(&h, group_id, iid).await;
        assert_eq!(error.as_deref(), Some(GITLAB_ERROR));
    }
}

#[tokio::test]
async fn search_kinds_filter_and_limit_apply_per_kind() {
    let (h, _dir) = dormant_handlers();
    seed_corpus(&h);
    let r = run_search(&h, "t", Some(vec!["issues".into()]), Some(1)).await;
    assert_eq!(
        r.issues.iter().map(|i| i.iid).collect::<Vec<_>>(),
        [20],
        "newest first, limited"
    );
    assert!(r.merge_requests.is_empty() && r.projects.is_empty() && r.groups.is_empty());
}

#[tokio::test]
async fn search_ranks_frequently_opened_first() {
    let (h, _dir) = dormant_handlers();
    seed_corpus(&h);
    run_record_open(&h, 1, 10, IssuableKind::issue).await;
    run_record_open(&h, 1, 10, IssuableKind::issue).await;

    let r = run_search(&h, "", None, None).await;
    assert_eq!(
        r.issues.iter().map(|i| i.iid).collect::<Vec<_>>(),
        [10],
        "an empty query lists only opened items"
    );
    assert_eq!(r.issues[0].open_count, 2);
    assert!(r.merge_requests.is_empty() && r.projects.is_empty());

    let r = run_search(&h, "t", Some(vec!["issues".into()]), None).await;
    assert_eq!(
        r.issues.iter().map(|i| i.iid).collect::<Vec<_>>(),
        [10, 20],
        "the opened older issue outranks the newer one"
    );
}

#[tokio::test]
async fn search_never_synced_is_honest_about_the_session() {
    let (h, _dir) = dormant_handlers();
    let mut call = AsyncCall::default();
    h.search(&mut call as &mut dyn Call_Search, "x".into(), None, None)
        .await
        .unwrap();
    assert_eq!(reply_error(&mut call).as_deref(), Some(NOT_AUTHENTICATED));

    let fake = Arc::new(FakeGitlab::default());
    let (h, _dir) = connected_handlers(&fake);
    let r = run_search(&h, "x", None, None).await;
    assert!(r.issues.is_empty() && r.projects.is_empty());
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

/// The rows name the files, so a read works without them on disk.
#[tokio::test]
async fn search_hits_carry_their_projects_avatar() {
    let (h, dir) = dormant_handlers();
    seed_corpus(&h);
    seed_avatars(&h);

    let r = run_search(&h, "oauth", None, None).await;
    assert_eq!(r.issues[0].project_avatar, avatar_path(&dir, "1-a.png"));
    assert_eq!(
        r.merge_requests[0].project_avatar,
        avatar_path(&dir, "1-a.png")
    );
    let r = run_search(&h, "auth-serv", None, None).await;
    assert_eq!(r.projects[0].avatar, avatar_path(&dir, "4-b.svg"));
}

#[tokio::test]
async fn assigned_items_carry_their_projects_avatar() {
    let (h, dir) = dormant_handlers();
    seed_assigned_issues(&h);
    seed_assigned_mrs(&h);
    seed_avatars(&h);

    let avatars = |project_avatars: Vec<(i64, String)>| -> Vec<(i64, String)> {
        let mut sorted = project_avatars;
        sorted.sort();
        sorted.dedup();
        sorted
    };
    let expected = [(1, avatar_path(&dir, "1-a.png")), (2, String::new())];
    let issues = assigned_issues(&h, None).await;
    assert_eq!(
        avatars(
            issues
                .into_iter()
                .map(|i| (i.project_id, i.project_avatar))
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
    let titles: Vec<&str> = events.iter().map(|e| e.title.as_str()).collect();
    assert_eq!(
        titles,
        ["queued mr", "newer", "older"],
        "30 days back is outside"
    );
    assert_eq!(events[0].source, "queued");
    assert_eq!(events[0].web_url, "https://gl/g/p/-/merge_requests/5");
    assert_eq!(events[1].duration, "30m");
    assert_eq!(events[1].kind, IssuableKind::merge_request);
    assert_eq!(events[2].duration, "1h 30m");
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
    let mut elsewhere = event_at(3, 9, "opened", now - 60);
    elsewhere.target_type = "MergeRequest".into();
    elsewhere.target_iid = 4;
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
        ["opened", "pushed to", "closed"],
        "30 days back is outside the default week"
    );
    assert_eq!(events[0].project_path, None, "project 9 is not stored");
    assert_eq!(events[0].web_url, None);
    assert_eq!(events[0].target_iid, Some(4));
    assert_eq!(events[1].r#ref.as_deref(), Some("main"));
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
    clear_cache(&h, Some(vec!["quick".into()])).await;
    assert_eq!(ids(&h), [3, 2]);

    seed_bands(&h);
    clear_cache(&h, Some(vec!["slow".into()])).await;
    assert_eq!(ids(&h), [3, 1]);

    seed_bands(&h);
    clear_cache(&h, Some(vec!["stale".into()])).await;
    assert_eq!(ids(&h), [2, 1]);
}

#[tokio::test]
async fn clear_cache_scopes_drop_their_slice_and_reset_its_jobs() {
    let (h, _dir) = dormant_handlers();
    seed_assigned_issues(&h);
    seed_corpus(&h);

    clear_cache(&h, Some(vec!["issues".into()])).await;
    assert!(h.sync.store().view(ASSIGNED_ISSUES).unwrap().is_none());
    assert!(!h.sync.has_synced(Job::AssignedIssues));
    assert!(h.sync.has_synced(Job::MemberProjects), "search untouched");

    clear_cache(&h, Some(vec!["search".into()])).await;
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
    run_record_open(&h, 1, 10, IssuableKind::issue).await;
    clear_cache(&h, None).await;
    assert!(
        !h.usage.snapshot().unwrap().entries.is_empty(),
        "user data, not a cache"
    );
    clear_cache(&h, Some(vec!["usage".into()])).await;
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
    let issues = assigned_issues(&h, None).await;
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

    clear_cache(&h, Some(vec!["usage".into()])).await;
    assert_eq!(fake.read_calls(), 0);

    clear_cache(&h, Some(vec!["issues".into()])).await;
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

    clear_cache(&h, Some(vec!["issues".into()])).await;
    let issues = assigned_issues(&h, None).await;
    assert_eq!(issues[0].graph_status, "Doing");
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
    assert_eq!((me.host.as_str(), me.user_id), ("gitlab.test", 1));
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
        kinds in proptest::option::of(proptest::collection::vec("[a-z_]{1,14}", 0..3)),
        limit in proptest::option::of(any::<i64>()),
    ) {
        prop_rt().block_on(async {
            let fake = Arc::new(FakeGitlab::default());
            let (h, _dir) = connected_handlers(&fake);
            seed_corpus(&h);

            let mut call = AsyncCall::default();
            h.search(&mut call as &mut dyn Call_Search, query.clone(), kinds.clone(), limit)
                .await
                .unwrap();
            let error = reply_error(&mut call);

            let invalid = matches!(limit, Some(n) if n <= 0)
                || kinds.iter().flatten().any(|k| {
                    !["issues", "merge_requests", "projects", "groups", "epics"]
                        .contains(&k.as_str())
                });
            if invalid {
                assert_eq!(error.as_deref(), Some(GITLAB_ERROR), "bad args are rejected eagerly");
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

            let r = run_search(&h, &needle, Some(vec!["issues".into()]), Some(limit)).await;
            let expected: Vec<i64> = issues
                .iter()
                .filter(|i| i.title.contains(&needle))
                .map(|i| i.id)
                .take(limit as usize)
                .collect();
            assert_eq!(r.issues.iter().map(|i| i.id).collect::<Vec<_>>(), expected);
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
                IssuableKind::issue,
                duration.clone(),
                None,
            )
            .await
            .unwrap();

            let valid = issue_ref_error(project_id, iid).is_none() && looks_like_duration(&duration);
            let expected = if valid { NOT_AUTHENTICATED } else { GITLAB_ERROR };
            assert_eq!(reply_error(&mut call).as_deref(), Some(expected));
            assert!(h.queue.pending().unwrap().is_empty());
        });
    }
}
