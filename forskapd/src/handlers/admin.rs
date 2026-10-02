//! The [`VarlinkInterface`] of `org.thehoster.forskapd.admin`: the session,
//! the cache and the sync worker's jobs, for the bundled CLI.

use std::time::Duration;

use tracing::{info, instrument, warn};

use forskap_api::admin::{
    CacheScope, Call_ClearCache, Call_GetSyncJobs, Call_Login, Call_Logout, VarlinkCallError,
    VarlinkInterface,
};

use crate::error::{DormancyReason, Error, Verdict};
use crate::gitlab::GitlabClient;
use crate::secrets::{Credentials, Token};
use crate::sync::{Clear, Job};

use super::{ConnState, Handlers, Session, now_secs, wire};

/// How long `GetSyncJobs` waits for the worker, which answers between two
/// awaits even with a fetch in flight.
const SYNC_JOBS_TIMEOUT: Duration = Duration::from_secs(5);

/// How long `ClearCache` waits for the foreground views (and a cleared
/// history) to refill before replying anyway; the rest refills in the
/// background.
const CLEAR_REFILL_TIMEOUT: Duration = Duration::from_secs(30);

/// [`super::reply_failed`] with this interface's errors, which it declares
/// apart: a varlink error belongs to one interface.
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

#[async_trait::async_trait]
impl VarlinkInterface for Handlers {
    #[instrument(skip(self, call))]
    async fn clear_cache(
        &self,
        call: &mut dyn Call_ClearCache,
        scope: Option<Vec<CacheScope>>,
    ) -> ::varlink::Result<()> {
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

    /// Status, not GitLab data: served whatever the session is.
    #[instrument(skip(self, call))]
    async fn get_sync_jobs(&self, call: &mut dyn Call_GetSyncJobs) -> ::varlink::Result<()> {
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

    #[instrument(skip(self, call, token))]
    async fn login(
        &self,
        call: &mut dyn Call_Login,
        host: String,
        token: String,
    ) -> ::varlink::Result<()> {
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
    async fn logout(&self, call: &mut dyn Call_Logout) -> ::varlink::Result<()> {
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
}
