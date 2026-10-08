//! Varlink method implementations.
//!
//! Reads never touch GitLab: they serve whatever the sync layer
//! ([`crate::sync`]) last stored, corrected at read time for writes it hasn't
//! picked up yet. A store read failure is logged and treated as empty so the
//! daemon stays available. Writes try GitLab once and fall back to the retry
//! queue, except creating an issue: that is tried once and never queued.
//!
//! - [`varlink`] — the [`VarlinkInterface`](forskap_api::VarlinkInterface)
//!   method impls plus the write cascade.
//! - [`admin`] — the [admin interface](forskap_api::admin)'s: the session,
//!   the cache, the sync worker's jobs.
//! - [`wire`] — projections of stored rows onto the wire types.
//!
//! This module holds the shared connection types, the helpers every submodule
//! reaches for, and the small pure validators.

use std::sync::Arc;

use tokio::sync::RwLock;

use forskap_api::{IssuableKind, NotAuthReason, VarlinkCallError, WorkItemRef};

use crate::config::SharedConfig;
use crate::error::{DormancyReason, Error, Verdict};
use crate::gitlab::{GitlabApi, GitlabClient};
use crate::queue::RetryQueue;
use crate::reconnect::Reconnect;
use crate::rotate::Rotation;
use crate::secrets::{Keychain, Token};
use crate::sync::{SyncHandle, now_secs};
use crate::usage::{UsageStats, epic_usage_key, usage_key};

mod admin;
mod varlink;
mod wire;

#[cfg(test)]
pub(crate) mod tests;

/// Live GitLab connection. Carries enough state for `WhoAmI` to answer without
/// a round-trip.
#[derive(Clone)]
pub struct Session {
    pub gitlab: Arc<dyn GitlabApi>,
    pub host: String,
    pub user_id: i64,
    pub username: String,
    /// The token `gitlab` authenticates with, to tell it from a newer one in
    /// the keychain.
    pub token: Token,
}

impl Session {
    pub fn from_client(client: GitlabClient) -> Self {
        let host = client.host().to_string();
        let user_id = client.current_user_id();
        let username = client.current_username().to_string();
        let token = client.token().clone();
        Self {
            gitlab: Arc::new(client),
            host,
            user_id,
            username,
            token,
        }
    }
}

/// Connection state the daemon shares between the handlers, the retry queue,
/// and the sync worker.
///
/// `Dormant` carries *why* there is no session (see [`DormancyReason`]) so the
/// CLI can report a specific cause instead of a bare "not authenticated".
pub enum ConnState {
    Connected(Session),
    Dormant(DormancyReason),
}

impl ConnState {
    /// The live GitLab client, if connected. Used by the retry-queue worker,
    /// which only needs the client and treats any dormant state as "defer".
    pub fn gitlab(&self) -> Option<Arc<dyn GitlabApi>> {
        match self {
            Self::Connected(s) => Some(s.gitlab.clone()),
            Self::Dormant(_) => None,
        }
    }
}

pub type SessionSlot = Arc<RwLock<ConnState>>;

pub struct Handlers {
    pub session: SessionSlot,
    /// The sync layer: the store every read serves from, plus the commands
    /// to refresh or clear it.
    pub sync: Arc<SyncHandle>,
    /// Open statistics behind `RecordOpen`; `Search` ranks by them.
    pub usage: Arc<UsageStats>,
    pub queue: RetryQueue,
    /// Live daemon config, read at use time so a hot reload takes effect
    /// without a restart.
    pub config: SharedConfig,
    /// The reconnect supervisor's wakeup — nudged when the sync worker
    /// demotes the session to `Dormant(Unreachable)` (see
    /// [`crate::reconnect::commit_unreachable`]) and when a call finds the
    /// session waiting for a locked keychain — and how the daemon stands
    /// without a session: since when, and what the supervisor is at.
    pub reconnect: Arc<Reconnect>,
    /// What the rotation supervisor knows about the session's token, and its
    /// wakeup.
    pub rotation: Arc<Rotation>,
    /// Where `Login` stores the token and `Logout` removes it; the reconnect
    /// and rotation supervisors read and write it through here too.
    pub keychain: Keychain,
}

impl Handlers {
    /// Resolve the live GitLab client, or `NotAuthenticated` carrying the
    /// dormancy reason.
    async fn gitlab(&self) -> std::result::Result<Arc<dyn GitlabApi>, DormancyReason> {
        self.current_session().await.map(|s| s.gitlab)
    }

    /// Resolve the full session, or `NotAuthenticated` carrying the dormancy
    /// reason.
    async fn current_session(&self) -> std::result::Result<Session, DormancyReason> {
        match &*self.session.read().await {
            ConnState::Connected(s) => Ok(s.clone()),
            ConnState::Dormant(r) => {
                // Someone is asking, so the user is around and may just have
                // unlocked the keychain: the supervisor looks now, not after
                // its back-off. This call still gets the reason it found.
                if matches!(r, DormancyReason::KeychainLocked { .. }) {
                    self.reconnect.notify_one();
                }
                Err(r.clone())
            }
        }
    }
}

/// Extract the varlink `(reason, detail)` pair from a dormancy error.
fn dormant_args(reason: &DormancyReason) -> (Option<NotAuthReason>, Option<String>) {
    (Some(reason.reason()), reason.detail())
}

/// An argument value refused before anything is sent or stored: which
/// argument, and why.
#[derive(Debug)]
struct Invalid {
    argument: &'static str,
    message: String,
}

impl Invalid {
    fn new(argument: &'static str, message: impl Into<String>) -> Self {
        Self {
            argument,
            message: message.into(),
        }
    }

    fn reply<C: VarlinkCallError + ?Sized>(self, call: &mut C) -> ::varlink::Result<()> {
        call.reply_invalid_argument(self.argument.into(), self.message)
    }
}

/// Reply to a call `e` failed by what it leaves the caller: GitLab refused,
/// GitLab was away, or the daemon failed on its own.
fn reply_failed<C: VarlinkCallError + ?Sized>(
    call: &mut C,
    e: &Error,
    message: String,
) -> ::varlink::Result<()> {
    match e.verdict() {
        Verdict::Refused(status) => call.reply_gitlab_error(message, status.map(i64::from)),
        Verdict::Unavailable => call.reply_gitlab_unavailable(message),
        Verdict::Internal => call.reply_internal(message),
    }
}

/// Reject obviously-malformed issue references up front (eager pre-check), so a
/// doomed request is never attempted or queued.
fn issue_ref_error(project_id: i64, iid: i64) -> Option<Invalid> {
    let argument = match (project_id, iid) {
        (..=0, _) => "project_id",
        (_, ..=0) => "iid",
        _ => return None,
    };
    let message = format!("invalid issue/MR reference (project {project_id}, iid {iid})");
    Some(Invalid::new(argument, message))
}

/// The usage key `RecordOpen` counts an open under, or why the reference is
/// malformed. A work item is addressed by its project or its group (an
/// epic), never both; a merge request only by its project.
fn open_key(
    kind: &IssuableKind,
    iid: i64,
    project_id: Option<i64>,
    group_id: Option<i64>,
) -> Result<String, Invalid> {
    match (project_id, group_id) {
        (Some(project_id), None) => match issue_ref_error(project_id, iid) {
            Some(invalid) => Err(invalid),
            None => Ok(usage_key(wire::internal_kind(kind), project_id, iid)),
        },
        (None, Some(_)) if *kind == IssuableKind::merge_request => Err(Invalid::new(
            "group_id",
            "a merge request is addressed by its project_id, not a group_id",
        )),
        (None, Some(group_id)) if group_id <= 0 || iid <= 0 => Err(Invalid::new(
            if group_id <= 0 { "group_id" } else { "iid" },
            format!("invalid work item reference (group {group_id}, iid {iid})"),
        )),
        (None, Some(group_id)) => Ok(epic_usage_key(group_id, iid)),
        // Named after the one too many, or the one missing.
        (Some(_), Some(_)) => Err(Invalid::new(
            "group_id",
            "give exactly one of project_id and group_id",
        )),
        (None, None) => Err(Invalid::new(
            "project_id",
            "give exactly one of project_id and group_id",
        )),
    }
}

/// The `(group_id, iid)` of the epic a new work item's `parent` names, or
/// why it names none: only an epic can be a parent here.
fn parent_epic(parent: &WorkItemRef) -> Result<(i64, i64), Invalid> {
    let epic = parent
        .r#type
        .as_deref()
        .is_none_or(|t| t.eq_ignore_ascii_case("epic"));
    match (parent.project_id, parent.group_id) {
        (None, Some(group_id)) if epic && group_id > 0 && parent.iid > 0 => {
            Ok((group_id, parent.iid))
        }
        _ => Err(Invalid::new(
            "item.parent",
            "invalid parent: name an epic by its group_id and iid",
        )),
    }
}

/// Reject a new issue GitLab would refuse or misread, before anything is
/// sent. A comma can't be part of a label: GitLab takes the labels as one
/// comma-separated list.
fn new_issue_error(project_id: i64, title: &str, labels: &[String]) -> Option<Invalid> {
    if project_id <= 0 {
        return Some(Invalid::new(
            "project_id",
            format!("invalid project: {project_id}"),
        ));
    }
    if title.trim().is_empty() {
        return Some(Invalid::new("item.title", "an issue needs a title"));
    }
    let split = labels.iter().find(|l| l.contains(','));
    split.map(|label| {
        let message = format!("invalid label {label:?}: a label can't contain a comma");
        Invalid::new("item.labels", message)
    })
}

/// Permissive sanity check for a GitLab time-tracking duration (`30m`,
/// `1h30m`, `1.5h`, `2d`). Rejects empties and obvious typos (`abc`, `1x`)
/// without trying to be a full GitLab-compatible parser — valid syntax is never
/// refused, so the only false negatives would need a unit GitLab doesn't use.
fn looks_like_duration(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() {
        return false;
    }
    let mut has_digit = false;
    for c in s.chars() {
        if c.is_ascii_digit() {
            has_digit = true;
        } else if c != '.'
            && !c.is_whitespace()
            && !matches!(c.to_ascii_lowercase(), 's' | 'm' | 'h' | 'd' | 'w' | 'o')
        {
            return false;
        }
    }
    has_digit
}
