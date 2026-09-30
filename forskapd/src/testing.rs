//! A configurable in-memory [`GitlabApi`] for the daemon's tests.
//!
//! Reads are routed by [`Listing::path`]: each path serves a standing set of
//! rows (empty by default), one-shot failures can be queued in front, and a
//! path can be gated to hold its next call until released. Every call is
//! recorded for assertions. Writes succeed unless a failure is queued, and
//! can all be held behind one gate to observe them in flight. The token and
//! avatar calls fail like reads, by their path ([`TOKEN_PATH`],
//! [`ROTATE_PATH`], `projects/<id>/avatar`).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::{Notify, Semaphore};

use crate::error::{Error, Result};
use crate::gitlab::{GitlabApi, Issuable, Listing, RotatedToken, TokenInfo};
use crate::secrets::Token;
use crate::sync::model::Timelog;

/// Path [`FakeGitlab::fail_next`] fails the token info read by.
pub const TOKEN_PATH: &str = "personal_access_tokens/self";
/// Path [`FakeGitlab::fail_next`] fails a rotation by.
pub const ROTATE_PATH: &str = "personal_access_tokens/self/rotate";

/// A failure to inject, turned into the matching [`Error`] on use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakeErr {
    /// Network failure: `Error::Transient`.
    Transient,
    /// 429/5xx: `Error::Throttled` with this status.
    Throttled(u16),
    /// A permanent rejection: `Error::Gitlab`.
    Rejected,
    /// A dead token: `Error::Unauthorized`.
    Unauthorized,
    /// An unusable rotation answer: `Error::RotationLost`.
    Lost,
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
            Self::Rejected => Error::Gitlab("403 Forbidden".into()),
            Self::Unauthorized => Error::Unauthorized("401 Unauthorized".into()),
            Self::Lost => Error::RotationLost("unreadable answer".into()),
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
    write_failures: Mutex<VecDeque<FakeErr>>,
    writes: Mutex<Vec<WriteCall>>,
    /// Holds every write while set; see [`FakeGitlab::gate_writes`].
    write_gate: Mutex<Option<Arc<Semaphore>>>,
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
            .filter(|l| l.path() == path)
            .collect()
    }

    /// The row limits the calls to `path` asked for.
    pub fn limits_to(&self, path: &str) -> Vec<Option<usize>> {
        let calls = self.calls.lock().unwrap();
        let limits = self.limits.lock().unwrap();
        calls
            .iter()
            .zip(limits.iter())
            .filter(|(l, _)| l.path() == path)
            .map(|(_, limit)| *limit)
            .collect()
    }

    pub fn timelog_calls(&self) -> Vec<chrono::DateTime<chrono::Utc>> {
        self.timelog_calls.lock().unwrap().clone()
    }

    /// Every read so far: listings, timelog queries and avatar downloads.
    pub fn read_calls(&self) -> usize {
        self.calls.lock().unwrap().len()
            + self.timelog_calls.lock().unwrap().len()
            + self.avatar_calls.lock().unwrap().len()
    }

    pub fn writes(&self) -> Vec<WriteCall> {
        self.writes.lock().unwrap().clone()
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
    async fn list(&self, listing: &Listing, limit: Option<usize>) -> Result<Vec<Value>> {
        let path = listing.path();
        self.calls.lock().unwrap().push(listing.clone());
        self.limits.lock().unwrap().push(limit);
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
        rows.truncate(limit.unwrap_or(usize::MAX));
        Ok(rows)
    }

    async fn list_timelogs(&self, since: chrono::DateTime<chrono::Utc>) -> Result<Vec<Timelog>> {
        self.timelog_calls.lock().unwrap().push(since);
        Ok(self.timelogs.lock().unwrap().clone())
    }

    async fn project_avatar(&self, project_id: i64) -> Result<Option<Vec<u8>>> {
        self.avatar_calls.lock().unwrap().push(project_id);
        if let Some(err) = self.next_failure(&format!("projects/{project_id}/avatar")) {
            return Err(err.error());
        }
        Ok(self.avatars.lock().unwrap().get(&project_id).cloned())
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

/// The smallest thing [`crate::sync::avatars::extension`] takes for a PNG.
pub const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";

/// A member project as GitLab's `simple=true` listing returns it, without an
/// avatar.
pub fn project_json(id: i64) -> Value {
    serde_json::json!({
        "id": id,
        "name": format!("p{id}"),
        "path_with_namespace": format!("g/p{id}"),
        "web_url": format!("https://gitlab.test/g/p{id}"),
        "avatar_url": null,
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
