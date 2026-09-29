//! Varlink method implementations.
//!
//! Reads never touch GitLab: they serve whatever the sync layer
//! ([`crate::sync`]) last stored, corrected at read time for writes it hasn't
//! picked up yet. A store read failure is logged and treated as empty so the
//! daemon stays available. Writes try GitLab once and fall back to the retry
//! queue.
//!
//! - [`varlink`] — the [`VarlinkInterface`](forskap_api::VarlinkInterface)
//!   method impls plus the write cascade.
//! - [`wire`] — projections of stored rows onto the wire types.
//!
//! This module holds the shared connection types, the helpers every submodule
//! reaches for, and the small pure validators.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::{Notify, RwLock};

use forskap_api::NotAuthReason;

use crate::config::SharedConfig;
use crate::error::DormancyReason;
use crate::gitlab::{GitlabApi, GitlabClient};
use crate::queue::RetryQueue;
use crate::rotate::Rotation;
use crate::secrets::Token;
use crate::sync::SyncHandle;
use crate::usage::UsageStats;

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
    /// The token `gitlab` authenticates with, to tell it from a newer one in
    /// the keychain.
    pub token: Token,
}

impl Session {
    pub fn from_client(client: GitlabClient) -> Self {
        let host = client.host().to_string();
        let user_id = client.current_user_id();
        let token = client.token().clone();
        Self {
            gitlab: Arc::new(client),
            host,
            user_id,
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
    /// Nudged when the sync worker demotes the session to
    /// `Dormant(Unreachable)` (see [`crate::reconnect::commit_unreachable`]),
    /// waking the reconnect supervisor.
    pub reconnect_signal: Arc<Notify>,
    /// What the rotation supervisor knows about the session's token, and its
    /// wakeup.
    pub rotation: Arc<Rotation>,
}

impl Handlers {
    /// Resolve the live GitLab client, or `NotAuthenticated` carrying the
    /// dormancy reason.
    async fn gitlab(&self) -> std::result::Result<Arc<dyn GitlabApi>, DormancyReason> {
        match &*self.session.read().await {
            ConnState::Connected(s) => Ok(s.gitlab.clone()),
            ConnState::Dormant(r) => Err(r.clone()),
        }
    }

    /// Resolve the full session, or `NotAuthenticated` carrying the dormancy
    /// reason.
    async fn current_session(&self) -> std::result::Result<Session, DormancyReason> {
        match &*self.session.read().await {
            ConnState::Connected(s) => Ok(s.clone()),
            ConnState::Dormant(r) => Err(r.clone()),
        }
    }
}

/// Extract the varlink `(reason, detail)` pair from a dormancy error.
fn dormant_args(reason: &DormancyReason) -> (Option<NotAuthReason>, Option<String>) {
    (Some(reason.reason()), reason.detail())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Reject obviously-malformed issue references up front (eager pre-check), so a
/// doomed request is never attempted or queued. Returns the error message when
/// invalid.
fn issue_ref_error(project_id: i64, iid: i64) -> Option<String> {
    (project_id <= 0 || iid <= 0)
        .then(|| format!("invalid issue/MR reference (project {project_id}, iid {iid})"))
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
