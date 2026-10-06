//! A configurable in-memory [`GitlabApi`] for the daemon's tests.
//!
//! Reads are routed by [`route`], a listing's [`Listing::path`] but for the
//! recent issue lists: each path serves a standing set of
//! rows (empty by default), one-shot failures can be queued in front, and a
//! path can be gated to hold its next call until released. Every call is
//! recorded for assertions. Writes succeed unless a failure is queued, and
//! can all be held behind one gate to observe them in flight; creating an
//! issue is one of them, answered with a served row. The token, avatar,
//! epic and description template calls fail like reads, by their path
//! ([`TOKEN_PATH`], [`ROTATE_PATH`], `projects/<id>/avatar`, [`epic_path`],
//! [`template_path`]).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::{Notify, Semaphore};

use crate::error::{Error, Result};
use crate::gitlab::{GitlabApi, Issuable, Listing, NewIssue, Progress, RotatedToken, TokenInfo};
use crate::secrets::Token;
use crate::sync::model::Timelog;

/// Path [`FakeGitlab::fail_next`] fails the token info read by.
pub const TOKEN_PATH: &str = "personal_access_tokens/self";
/// Path [`FakeGitlab::fail_next`] fails a rotation by.
pub const ROTATE_PATH: &str = "personal_access_tokens/self/rotate";

/// What the fake routes the recent issues the user authored by.
pub const RECENT_AUTHORED_PATH: &str = "issues?authored";
/// What the fake routes the recent issues assigned to the user by.
pub const RECENT_ASSIGNED_PATH: &str = "issues?assigned";

/// The path [`FakeGitlab::fail_next`] fails the lookup of an epic by.
pub fn epic_path(group_id: i64, iid: i64) -> String {
    format!("groups/{group_id}/epics/{iid}")
}

/// The path [`FakeGitlab::fail_next`] fails the read of a description
/// template by.
pub fn template_path(project_id: i64, kind: Issuable, key: &str) -> String {
    format!(
        "projects/{project_id}/templates/{}/{key}",
        kind.path_segment()
    )
}

/// The path a read is served, failed, gated and counted by: the listing's
/// own, except for the recent issue lists. They share `issues` with the
/// assigned list, which a test serving or counting that one doesn't mean.
pub fn route(listing: &Listing) -> String {
    match listing {
        Listing::RecentAuthoredIssues { .. } => RECENT_AUTHORED_PATH.into(),
        Listing::RecentAssignedIssues { .. } => RECENT_ASSIGNED_PATH.into(),
        other => other.path(),
    }
}

/// A failure to inject, turned into the matching [`Error`] on use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakeErr {
    /// Network failure: `Error::Transient`.
    Transient,
    /// 429/5xx: `Error::Throttled` with this status.
    Throttled(u16),
    /// A permanent rejection, GitLab's 403 Forbidden: `Error::Rejected`.
    Rejected,
    /// A permanent rejection with this status (404, 400, 422, …):
    /// `Error::Rejected`.
    RejectedWith(u16),
    /// A dead token: `Error::Unauthorized`.
    Unauthorized,
    /// An unusable rotation answer: `Error::RotationLost`.
    Lost,
    /// A bug: the call panics instead of failing.
    Panic,
}

impl FakeErr {
    pub fn error(self) -> Error {
        match self {
            Self::Transient => Error::Transient("offline".into()),
            Self::Throttled(status) => Error::Throttled {
                status,
                retry_after: None,
                detail: "busy".into(),
            },
            Self::Rejected => Self::RejectedWith(403).error(),
            Self::RejectedWith(status) => Error::Rejected {
                status,
                detail: match status {
                    403 => "403 Forbidden".into(),
                    404 => "404 Not Found".into(),
                    400 => "400 Bad Request".into(),
                    _ => format!("{status} refused"),
                },
            },
            Self::Unauthorized => Error::Unauthorized("401 Unauthorized".into()),
            Self::Lost => Error::RotationLost("unreadable answer".into()),
            Self::Panic => panic!("fake panic"),
        }
    }
}

/// One recorded write: `(op, kind, project_id, iid)`.
pub type WriteCall = (&'static str, Issuable, i64, i64);

#[derive(Default)]
pub struct FakeGitlab {
    rows: Mutex<HashMap<String, Vec<Value>>>,
    /// One-shot responses served ahead of `rows`.
    next_rows: Mutex<HashMap<String, VecDeque<Vec<Value>>>>,
    failures: Mutex<HashMap<String, VecDeque<FakeErr>>>,
    gates: Mutex<HashMap<String, Arc<Notify>>>,
    timelogs: Mutex<Vec<Timelog>>,
    calls: Mutex<Vec<Listing>>,
    /// The row limit of each call in `calls`.
    limits: Mutex<Vec<Option<usize>>>,
    timelog_calls: Mutex<Vec<chrono::DateTime<chrono::Utc>>>,
    avatars: Mutex<HashMap<i64, Vec<u8>>>,
    /// The project of every avatar download.
    avatar_calls: Mutex<Vec<i64>>,
    /// The epics the lookup finds, by group and number.
    epics: Mutex<HashMap<(i64, i64), Value>>,
    /// The group and number of every epic lookup.
    epic_calls: Mutex<Vec<(i64, i64)>>,
    /// The content of the description templates the read finds, by project,
    /// kind and key.
    description_templates: Mutex<HashMap<(i64, Issuable, String), String>>,
    /// The project, kind and key of every description template read.
    template_calls: Mutex<Vec<(i64, Issuable, String)>>,
    write_failures: Mutex<VecDeque<FakeErr>>,
    writes: Mutex<Vec<WriteCall>>,
    /// Holds every write while set; see [`FakeGitlab::gate_writes`].
    write_gate: Mutex<Option<Arc<Semaphore>>>,
    /// The project and content of every create attempt.
    created: Mutex<Vec<(i64, NewIssue)>>,
    /// The rows the next creates answer with.
    create_rows: Mutex<VecDeque<Value>>,
    /// The token's info; one that never expires by default.
    token: Mutex<Option<TokenInfo>>,
    token_info_calls: Mutex<usize>,
    /// The `expires_at` of every rotation attempt.
    rotations: Mutex<Vec<Option<chrono::NaiveDate>>>,
    /// Signalled when a gated call starts waiting on its gate.
    pub gated: Notify,
}

/// Releases the writes held by [`FakeGitlab::gate_writes`].
pub struct WriteGate(Arc<Semaphore>);

impl WriteGate {
    /// Let every held write through, and every later one at once.
    pub fn release(&self) {
        self.0.close();
    }
}

impl FakeGitlab {
    /// Serve `rows` on every call to `path` from now on.
    pub fn serve(&self, path: &str, rows: Vec<Value>) {
        self.rows.lock().unwrap().insert(path.into(), rows);
    }

    /// Serve `rows` on the next call to `path` only, ahead of the standing
    /// rows.
    pub fn serve_next(&self, path: &str, rows: Vec<Value>) {
        self.next_rows
            .lock()
            .unwrap()
            .entry(path.into())
            .or_default()
            .push_back(rows);
    }

    /// Fail the next call to `path`.
    pub fn fail_next(&self, path: &str, err: FakeErr) {
        self.failures
            .lock()
            .unwrap()
            .entry(path.into())
            .or_default()
            .push_back(err);
    }

    /// Hold the next call to `path` until the returned gate is notified.
    pub fn gate(&self, path: &str) -> Arc<Notify> {
        let gate = Arc::new(Notify::new());
        self.gates
            .lock()
            .unwrap()
            .insert(path.into(), Arc::clone(&gate));
        gate
    }

    pub fn serve_timelogs(&self, rows: Vec<Timelog>) {
        *self.timelogs.lock().unwrap() = rows;
    }

    /// Serve `image` as the project's avatar; a project without one is a 404.
    pub fn serve_avatar(&self, project_id: i64, image: &[u8]) {
        self.avatars
            .lock()
            .unwrap()
            .insert(project_id, image.to_vec());
    }

    /// The project of every avatar download so far.
    pub fn avatar_calls(&self) -> Vec<i64> {
        self.avatar_calls.lock().unwrap().clone()
    }

    /// Let the epic lookup find `row`, an epic as GitLab returns it, by its
    /// group and number; any other epic is a 404.
    pub fn serve_epic(&self, row: Value) {
        let id = |field: &str| row[field].as_i64().unwrap_or_default();
        let key = (id("group_id"), id("iid"));
        self.epics.lock().unwrap().insert(key, row);
    }

    /// The group and number of every epic lookup so far.
    pub fn epic_calls(&self) -> Vec<(i64, i64)> {
        self.epic_calls.lock().unwrap().clone()
    }

    /// Let the description template read find `content` under `key` among
    /// the project's templates of `kind`; any other template is a 404
    /// (`None`).
    pub fn serve_template(&self, project_id: i64, kind: Issuable, key: &str, content: &str) {
        self.description_templates
            .lock()
            .unwrap()
            .insert((project_id, kind, key.to_string()), content.to_string());
    }

    /// The project, kind and key of every description template read so far.
    pub fn template_calls(&self) -> Vec<(i64, Issuable, String)> {
        self.template_calls.lock().unwrap().clone()
    }

    pub fn serve_token(&self, info: TokenInfo) {
        *self.token.lock().unwrap() = Some(info);
    }

    pub fn token_info_calls(&self) -> usize {
        *self.token_info_calls.lock().unwrap()
    }

    /// The `expires_at` of every rotation attempt, failed ones included.
    pub fn rotations(&self) -> Vec<Option<chrono::NaiveDate>> {
        self.rotations.lock().unwrap().clone()
    }

    fn served_token(&self) -> TokenInfo {
        self.token.lock().unwrap().clone().unwrap_or(TokenInfo {
            scopes: Vec::new(),
            created_at: None,
            expires_at: None,
        })
    }

    /// How many rows the next call to `path` answers with.
    fn pending(&self, path: &str) -> usize {
        let next = self.next_rows.lock().unwrap();
        match next.get(path).and_then(VecDeque::front) {
            Some(rows) => rows.len(),
            None => self.rows.lock().unwrap().get(path).map_or(0, Vec::len),
        }
    }

    fn next_failure(&self, path: &str) -> Option<FakeErr> {
        self.failures
            .lock()
            .unwrap()
            .get_mut(path)
            .and_then(VecDeque::pop_front)
    }

    pub fn fail_next_write(&self, err: FakeErr) {
        self.write_failures.lock().unwrap().push_back(err);
    }

    /// Hold every write from now on until the returned gate is released. A
    /// held write is already recorded in [`writes`](Self::writes), so its
    /// length counts the attempts started.
    pub fn gate_writes(&self) -> WriteGate {
        let gate = Arc::new(Semaphore::new(0));
        *self.write_gate.lock().unwrap() = Some(Arc::clone(&gate));
        WriteGate(gate)
    }

    pub fn calls(&self) -> Vec<Listing> {
        self.calls.lock().unwrap().clone()
    }

    pub fn calls_to(&self, path: &str) -> Vec<Listing> {
        self.calls()
            .into_iter()
            .filter(|l| route(l) == path)
            .collect()
    }

    /// The row limits the calls to `path` asked for.
    pub fn limits_to(&self, path: &str) -> Vec<Option<usize>> {
        let calls = self.calls.lock().unwrap();
        let limits = self.limits.lock().unwrap();
        calls
            .iter()
            .zip(limits.iter())
            .filter(|(l, _)| route(l) == path)
            .map(|(_, limit)| *limit)
            .collect()
    }

    pub fn timelog_calls(&self) -> Vec<chrono::DateTime<chrono::Utc>> {
        self.timelog_calls.lock().unwrap().clone()
    }

    /// Every read so far: listings, timelog queries, avatar downloads, epic
    /// lookups and issue template reads.
    pub fn read_calls(&self) -> usize {
        self.calls.lock().unwrap().len()
            + self.timelog_calls.lock().unwrap().len()
            + self.avatar_calls.lock().unwrap().len()
            + self.epic_calls.lock().unwrap().len()
            + self.template_calls.lock().unwrap().len()
    }

    pub fn writes(&self) -> Vec<WriteCall> {
        self.writes.lock().unwrap().clone()
    }

    /// Answer the next create with `row`, the issue as GitLab would return
    /// it. Without one a create answers `issue_json(project, 1, title)`.
    pub fn serve_create(&self, row: Value) {
        self.create_rows.lock().unwrap().push_back(row);
    }

    /// The project and content of every create so far, failed ones included.
    pub fn created(&self) -> Vec<(i64, NewIssue)> {
        self.created.lock().unwrap().clone()
    }

    async fn write(
        &self,
        op: &'static str,
        kind: Issuable,
        project_id: i64,
        iid: i64,
    ) -> Result<()> {
        self.writes
            .lock()
            .unwrap()
            .push((op, kind, project_id, iid));
        // Cloned out so the std lock is not held across the await; a closed
        // gate fails the acquire, which is the release.
        let gate = self.write_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            let _ = gate.acquire().await;
        }
        match self.write_failures.lock().unwrap().pop_front() {
            Some(err) => Err(err.error()),
            None => Ok(()),
        }
    }
}

#[async_trait::async_trait]
impl GitlabApi for FakeGitlab {
    async fn list(
        &self,
        listing: &Listing,
        limit: Option<usize>,
        progress: &Progress,
    ) -> Result<Vec<Value>> {
        let path = route(listing);
        self.calls.lock().unwrap().push(listing.clone());
        self.limits.lock().unwrap().push(limit);
        // Like GitLab's first page: the total is known while the rows are
        // still to come, so a gated call shows as 0 of them.
        let cap = limit.unwrap_or(usize::MAX);
        progress.expect(Some(self.pending(&path).min(cap) as u64));
        let gate = self.gates.lock().unwrap().remove(&path);
        if let Some(gate) = gate {
            self.gated.notify_one();
            gate.notified().await;
        }
        if let Some(err) = self.next_failure(&path) {
            return Err(err.error());
        }
        let next = self
            .next_rows
            .lock()
            .unwrap()
            .get_mut(&path)
            .and_then(VecDeque::pop_front);
        let mut rows = next.unwrap_or_else(|| {
            self.rows
                .lock()
                .unwrap()
                .get(&path)
                .cloned()
                .unwrap_or_default()
        });
        rows.truncate(cap);
        progress.add(rows.len());
        Ok(rows)
    }

    async fn list_timelogs(
        &self,
        since: chrono::DateTime<chrono::Utc>,
        progress: &Progress,
    ) -> Result<Vec<Timelog>> {
        self.timelog_calls.lock().unwrap().push(since);
        let logs = self.timelogs.lock().unwrap().clone();
        progress.add(logs.len());
        Ok(logs)
    }

    async fn project_avatar(&self, project_id: i64) -> Result<Option<Vec<u8>>> {
        self.avatar_calls.lock().unwrap().push(project_id);
        if let Some(err) = self.next_failure(&format!("projects/{project_id}/avatar")) {
            return Err(err.error());
        }
        Ok(self.avatars.lock().unwrap().get(&project_id).cloned())
    }

    async fn epic(&self, group_id: i64, iid: i64) -> Result<Value> {
        self.epic_calls.lock().unwrap().push((group_id, iid));
        if let Some(err) = self.next_failure(&epic_path(group_id, iid)) {
            return Err(err.error());
        }
        let served = self.epics.lock().unwrap().get(&(group_id, iid)).cloned();
        served.ok_or_else(|| FakeErr::RejectedWith(404).error())
    }

    async fn description_template(
        &self,
        project_id: i64,
        kind: Issuable,
        key: &str,
    ) -> Result<Option<Value>> {
        self.template_calls
            .lock()
            .unwrap()
            .push((project_id, kind, key.to_string()));
        if let Some(err) = self.next_failure(&template_path(project_id, kind, key)) {
            return Err(err.error());
        }
        let served = self.description_templates.lock().unwrap();
        let content = served.get(&(project_id, kind, key.to_string())).cloned();
        Ok(content.map(|content| serde_json::json!({"name": key, "content": content})))
    }

    async fn add_spent_time(
        &self,
        kind: Issuable,
        project_id: i64,
        iid: i64,
        _duration: &str,
        _summary: Option<&str>,
    ) -> Result<()> {
        self.write("add_spent_time", kind, project_id, iid).await
    }

    async fn create_timelog(
        &self,
        kind: Issuable,
        issuable_id: i64,
        _duration: &str,
        _summary: &str,
        _spent_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        self.write("create_timelog", kind, 0, issuable_id).await
    }

    async fn close(&self, kind: Issuable, project_id: i64, iid: i64) -> Result<()> {
        self.write("close", kind, project_id, iid).await
    }

    async fn assign_self(&self, kind: Issuable, project_id: i64, iid: i64) -> Result<()> {
        self.write("assign_self", kind, project_id, iid).await
    }

    async fn unassign_self(&self, kind: Issuable, project_id: i64, iid: i64) -> Result<()> {
        self.write("unassign_self", kind, project_id, iid).await
    }

    /// A write like the others, logged as `("create_issue", Issue, project, 0)`.
    async fn create_issue(&self, project_id: i64, issue: &NewIssue) -> Result<Value> {
        let sent = (project_id, issue.clone());
        self.created.lock().unwrap().push(sent);
        self.write("create_issue", Issuable::Issue, project_id, 0)
            .await?;
        let served = self.create_rows.lock().unwrap().pop_front();
        Ok(served.unwrap_or_else(|| issue_json(project_id, 1, &issue.title)))
    }

    async fn token_info(&self) -> Result<TokenInfo> {
        *self.token_info_calls.lock().unwrap() += 1;
        match self.next_failure(TOKEN_PATH) {
            Some(err) => Err(err.error()),
            None => Ok(self.served_token()),
        }
    }

    /// The n-th rotation attempt yields the token `rotated-n`, living a week
    /// unless `expires_at` says otherwise.
    async fn rotate_token(&self, expires_at: Option<chrono::NaiveDate>) -> Result<RotatedToken> {
        let n = {
            let mut rotations = self.rotations.lock().unwrap();
            rotations.push(expires_at);
            rotations.len()
        };
        if let Some(err) = self.next_failure(ROTATE_PATH) {
            return Err(err.error());
        }
        let now = chrono::Utc::now();
        Ok(RotatedToken {
            token: Token::new(format!("rotated-{n}")),
            info: Some(TokenInfo {
                scopes: self.served_token().scopes,
                created_at: Some(now),
                expires_at: expires_at.or(Some(now.date_naive() + chrono::Days::new(7))),
            }),
        })
    }
}

/// Poll `cond` every 10 ms until it holds, failing the test after 2 s.
pub async fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while !cond() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// An issue as GitLab's REST API returns it.
pub fn issue_json(project_id: i64, iid: i64, title: &str) -> Value {
    serde_json::json!({
        "id": project_id * 1000 + iid,
        "iid": iid,
        "project_id": project_id,
        "title": title,
        "web_url": format!("https://gitlab.test/g/p{project_id}/-/issues/{iid}"),
        "state": "opened",
        "labels": [],
        "updated_at": "2026-07-01T10:00:00Z",
    })
}

/// An epic of the group `group_id` as GitLab's REST API returns it; its
/// work item id is its legacy id plus 900 000.
pub fn epic_json(group_id: i64, iid: i64, title: &str) -> Value {
    serde_json::json!({
        "id": group_id * 1000 + iid,
        "iid": iid,
        "group_id": group_id,
        "work_item_id": 900_000 + group_id * 1000 + iid,
        "title": title,
        "web_url": format!("https://gitlab.test/groups/g{group_id}/-/epics/{iid}"),
        "state": "opened",
        "labels": [],
        "updated_at": "2026-07-01T10:00:00Z",
    })
}

/// The smallest thing [`crate::sync::avatars::extension`] takes for a PNG.
pub const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";

/// A member project as GitLab's listing returns it (the fields the daemon
/// mirrors), without an avatar, not archived and with every feature on.
pub fn project_json(id: i64) -> Value {
    serde_json::json!({
        "id": id,
        "name": format!("p{id}"),
        "path_with_namespace": format!("g/p{id}"),
        "web_url": format!("https://gitlab.test/g/p{id}"),
        "avatar_url": null,
        "archived": false,
        "issues_enabled": true,
        "merge_requests_enabled": true,
        "issues_access_level": "enabled",
        "merge_requests_access_level": "enabled",
        "repository_access_level": "enabled",
    })
}

/// [`project_json`] with the feature `feature` (`"issues"`,
/// `"merge_requests"`, `"repository"`) switched off, as GitLab shows it.
pub fn project_json_without(id: i64, feature: &str) -> Value {
    let mut project = project_json(id);
    project[format!("{feature}_access_level")] = "disabled".into();
    if feature != "repository" {
        project[format!("{feature}_enabled")] = false.into();
    }
    project
}

/// A member group at `full_path` as GitLab's listing returns it.
pub fn group_json(id: i64, full_path: &str) -> Value {
    serde_json::json!({
        "id": id,
        "name": full_path.rsplit('/').next().unwrap_or(full_path),
        "full_path": full_path,
        "web_url": format!("https://gitlab.test/groups/{full_path}"),
    })
}

/// [`project_json`] with the avatar `file` uploaded.
pub fn project_json_with_avatar(id: i64, file: &str) -> Value {
    let mut project = project_json(id);
    project["avatar_url"] =
        format!("https://gitlab.test/uploads/-/system/project/avatar/{id}/{file}").into();
    project
}

/// One of the user's contribution events, created `created_at` (unix secs).
pub fn event_json(id: i64, project_id: i64, action: &str, created_at: u64) -> Value {
    let at = chrono::DateTime::from_timestamp(created_at as i64, 0).unwrap();
    serde_json::json!({
        "id": id,
        "project_id": project_id,
        "action_name": action,
        "target_type": null,
        "target_iid": null,
        "target_title": null,
        "created_at": at.to_rfc3339(),
    })
}
