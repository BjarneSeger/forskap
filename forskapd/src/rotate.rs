//! Background supervisor that replaces the GitLab token by a fresh one
//! shortly before it expires.
//!
//! While the session is `Connected` it reads the token's scopes and lifetime
//! once, decides whether and when the token rotates ([`plan`]), waits and
//! rotates. It re-evaluates when woken ([`Rotation::reevaluate`]: login,
//! logout, reconnect, config reload) and at least every [`Pacing::recheck`].
//!
//! GitLab revokes the old token the moment it answers the rotation. So the
//! new one goes to the keychain first and the session is swapped after; if
//! the keychain keeps failing, the session is swapped anyway and the write
//! retried in the background.
//!
//! A failed rotation never demotes the session: the sync worker stays the
//! demotion authority. The token itself is never logged.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Days, NaiveDate, NaiveTime, TimeDelta, Utc};
use tokio::sync::Notify;
use tracing::{debug, error, info, warn};

use crate::config::{AuthConfig, RotatePolicy, SharedConfig, next_backoff};
use crate::error::{DormancyReason, Error, Result};
use crate::gitlab::{GitlabApi, GitlabClient, TokenInfo};
use crate::handlers::{ConnState, Handlers, Session, SessionSlot};
use crate::secrets::{Credentials, Token};

/// Widest spread of the rotation point between machines.
const SPREAD_MAX: TimeDelta = TimeDelta::hours(24);

/// Keychain writes tried before the session is swapped without one.
const KEYCHAIN_ATTEMPTS: u32 = 4;

/// The supervisor's waits; zeroed by the tests.
#[derive(Debug, Clone, Copy)]
struct Pacing {
    /// Longest wait between two evaluations. Waiting out days in one sleep
    /// would overshoot: the tokio clock stands still while suspended.
    recheck: Duration,
    retry_base: Duration,
    retry_max: Duration,
    keychain_base: Duration,
}

const PACING: Pacing = Pacing {
    recheck: Duration::from_secs(3600),
    retry_base: Duration::from_secs(30),
    retry_max: Duration::from_secs(3600),
    keychain_base: Duration::from_secs(1),
};

/// What the supervisor shares with the handlers: its wakeup, and what it
/// knows about the session's token (for `WhoAmI`).
#[derive(Default)]
pub struct Rotation {
    signal: Notify,
    status: Mutex<Option<Status>>,
}

struct Status {
    /// The client whose token this is about.
    client: Arc<dyn GitlabApi>,
    info: TokenInfo,
    /// GitLab refused to rotate this token.
    refused: bool,
}

impl Rotation {
    /// The session or the config changed.
    pub fn reevaluate(&self) {
        self.signal.notify_one();
    }

    /// For `WhoAmI`: when the token of `client` expires (unix seconds), and
    /// whether it rotates under `auth`. Unknown reads as `(None, false)`.
    pub fn report(&self, client: &Arc<dyn GitlabApi>, auth: &AuthConfig) -> (Option<i64>, bool) {
        let status = self.status.lock().unwrap();
        let Some(status) = status.as_ref().filter(|s| Arc::ptr_eq(&s.client, client)) else {
            return (None, false);
        };
        let rotates =
            !status.refused && matches!(plan(auth, &status.info, Utc::now(), 0.0), Plan::At(_));
        let expires_at = status.info.expires_at.map(|d| expiry(d).timestamp());
        (expires_at, rotates)
    }

    pub(crate) fn publish(&self, client: &Arc<dyn GitlabApi>, info: &TokenInfo) {
        *self.status.lock().unwrap() = Some(Status {
            client: Arc::clone(client),
            info: info.clone(),
            refused: false,
        });
    }

    fn refuse(&self) {
        if let Some(status) = self.status.lock().unwrap().as_mut() {
            status.refused = true;
        }
    }

    fn clear(&self) {
        *self.status.lock().unwrap() = None;
    }
}

/// Why a token is left alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Why {
    Disabled,
    NoExpiry,
    NoScope,
    /// It could rotate through `api`, but wasn't created for it.
    ApiScopeOnly,
    Expired,
}

impl Why {
    fn describe(self) -> &'static str {
        match self {
            Self::Disabled => "auth.rotate is \"never\"",
            Self::NoExpiry => "it never expires",
            Self::NoScope => "it lacks the self_rotate scope",
            Self::ApiScopeOnly => {
                "it lacks the self_rotate scope; auth.rotate = \"always\" rotates it through api"
            }
            Self::Expired => "it has expired",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plan {
    /// Rotate once this moment has passed.
    At(DateTime<Utc>),
    Idle(Why),
}

/// The moment a token expiring on `date` dies.
fn expiry(date: NaiveDate) -> DateTime<Utc> {
    date.and_time(NaiveTime::MIN).and_utc()
}

/// Whether and when the token rotates: once less than `rotate_before` is
/// left, or a third of its lifetime if that is shorter, so a short-lived
/// token doesn't rotate at every look. `spread` in `[0, 1)` moves it earlier
/// by up to half of that, a day at most: machines sharing the keychain must
/// not rotate at the same moment, the later one would revoke the new token.
fn plan(auth: &AuthConfig, info: &TokenInfo, now: DateTime<Utc>, spread: f64) -> Plan {
    if auth.rotate == RotatePolicy::Never {
        return Plan::Idle(Why::Disabled);
    }
    let Some(expires) = info.expires_at.map(expiry) else {
        return Plan::Idle(Why::NoExpiry);
    };
    let has = |scope: &str| info.scopes.iter().any(|s| s == scope);
    if !has("self_rotate") {
        if !has("api") {
            return Plan::Idle(Why::NoScope);
        }
        if auth.rotate != RotatePolicy::Always {
            return Plan::Idle(Why::ApiScopeOnly);
        }
    }
    if now >= expires {
        return Plan::Idle(Why::Expired);
    }
    let mut lead = TimeDelta::from_std(auth.rotate_before()).unwrap_or(TimeDelta::MAX);
    if let Some(created) = info.created_at.filter(|c| *c < expires) {
        lead = lead.min((expires - created) / 3);
    }
    let width = (lead / 2).min(SPREAD_MAX).num_seconds() as f64;
    let early = TimeDelta::seconds((width * spread.clamp(0.0, 1.0)) as i64);
    Plan::At(
        expires
            .checked_sub_signed(lead)
            .and_then(|at| at.checked_sub_signed(early))
            .unwrap_or(DateTime::<Utc>::MIN_UTC),
    )
}

/// The expiry to ask for: as many days from `today` as the token had from
/// its creation. `None` (GitLab's default) if its lifetime is unknown.
fn new_expiry(info: &TokenInfo, today: NaiveDate) -> Option<NaiveDate> {
    let old = info.expires_at?;
    let days = (old - info.created_at?.date_naive()).num_days().max(1) as u64;
    let same_length = today.checked_add_days(Days::new(days))?;
    // A token living one day would get its own expiry back.
    Some(same_length.max(old.checked_add_days(Days::new(1))?))
}

/// The supervisor's side effects, abstracted so its state machine can be
/// unit-tested without a keychain or a live GitLab.
#[async_trait::async_trait]
trait Env: Send + Sync + 'static {
    fn now(&self) -> DateTime<Utc>;
    async fn load(&self) -> Result<Option<Credentials>>;
    async fn store(&self, creds: &Credentials) -> Result<()>;
    async fn forget(&self) -> Result<()>;
    async fn connect(&self, host: &str, token: &Token) -> Result<Session>;
    /// The session runs on the rotated token now.
    fn swapped(&self);
}

struct Live(Arc<Handlers>);

#[async_trait::async_trait]
impl Env for Live {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }

    async fn load(&self) -> Result<Option<Credentials>> {
        self.0.keychain.load().await
    }

    async fn store(&self, creds: &Credentials) -> Result<()> {
        self.0.keychain.store(creds).await
    }

    async fn forget(&self) -> Result<()> {
        self.0.keychain.delete().await
    }

    async fn connect(&self, host: &str, token: &Token) -> Result<Session> {
        GitlabClient::connect(host, token)
            .await
            .map(Session::from_client)
    }

    fn swapped(&self) {
        // The account is the same one: nothing to clear, no backoff to lift.
        self.0.queue.drain_waker().notify_one();
        self.0.sync.wake();
    }
}

/// Spawn the rotation supervisor. It lives for the whole daemon run and
/// idles while there is no session or no token to rotate.
pub fn spawn(handlers: Arc<Handlers>) {
    let supervisor = Supervisor {
        session: Arc::clone(&handlers.session),
        config: Arc::clone(&handlers.config),
        rotation: Arc::clone(&handlers.rotation),
        env: Arc::new(Live(handlers)),
        pacing: PACING,
        spread: random_unit(),
        watch: None,
    };
    tokio::spawn(supervisor.run());
}

/// A value in `[0, 1)` differing between daemons.
fn random_unit() -> f64 {
    use std::hash::{BuildHasher, RandomState};
    crate::sync::schedule::unit("rotation", RandomState::new().hash_one(0u8))
}

/// What was last logged about a token, so a recheck doesn't repeat it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Note {
    Idle(Why),
    At(DateTime<Utc>),
    /// The keychain holds other credentials than the session.
    Foreign,
}

/// The supervisor's view of one session's token.
struct Watch {
    client: Arc<dyn GitlabApi>,
    info: Option<TokenInfo>,
    /// GitLab refused for good: nothing more to try with this token.
    refused: bool,
    backoff: Option<Duration>,
    noted: Option<Note>,
}

impl Watch {
    fn new(client: &Arc<dyn GitlabApi>, info: Option<TokenInfo>) -> Self {
        Self {
            client: Arc::clone(client),
            info,
            refused: false,
            backoff: None,
            noted: None,
        }
    }

    /// Whether `note` is news.
    fn note(&mut self, note: Note) -> bool {
        self.noted.replace(note) != Some(note)
    }
}

struct Supervisor<E> {
    session: SessionSlot,
    config: SharedConfig,
    rotation: Arc<Rotation>,
    env: Arc<E>,
    pacing: Pacing,
    /// This daemon's share of the rotation point's spread, in `[0, 1)`.
    spread: f64,
    watch: Option<Watch>,
}

impl<E: Env> Supervisor<E> {
    async fn run(mut self) {
        loop {
            let wait = self.engage().await;
            // A wakeup during `engage` left a permit, so none is lost.
            tokio::select! {
                _ = self.rotation.signal.notified() => {}
                _ = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// One evaluation: rotate if the session's token is due. Returns how
    /// long to wait for the next one at most.
    async fn engage(&mut self) -> Duration {
        let session = match &*self.session.read().await {
            ConnState::Connected(s) => Some(s.clone()),
            ConnState::Dormant(_) => None,
        };
        let Some(session) = session else {
            self.unwatch();
            return self.pacing.recheck;
        };
        if self
            .watch
            .as_ref()
            .is_some_and(|w| !Arc::ptr_eq(&w.client, &session.gitlab))
        {
            self.unwatch();
        }
        let watch = self
            .watch
            .get_or_insert_with(|| Watch::new(&session.gitlab, None));
        if watch.refused {
            return self.pacing.recheck;
        }
        let info = match watch.info.clone() {
            Some(info) => info,
            None => match session.gitlab.token_info().await {
                Ok(info) => {
                    watch.info = Some(info.clone());
                    watch.backoff = None;
                    self.rotation.publish(&session.gitlab, &info);
                    info
                }
                Err(e) => return self.failed(e, "reading the GitLab token's expiry", false),
            },
        };

        let auth = self.config.read().unwrap().auth;
        let now = self.env.now();
        let at = match plan(&auth, &info, now, self.spread) {
            Plan::At(at) => at,
            Plan::Idle(why) => {
                if watch.note(Note::Idle(why)) {
                    info!(reason = why.describe(), "the GitLab token is not rotated");
                }
                return self.pacing.recheck;
            }
        };
        if now < at {
            if watch.note(Note::At(at)) {
                info!(at = %at, "GitLab token rotation scheduled");
            }
            let left = (at - now).to_std().unwrap_or(self.pacing.recheck);
            return left.min(self.pacing.recheck);
        }
        self.rotate(&session, &info, now).await
    }

    async fn rotate(
        &mut self,
        session: &Session,
        info: &TokenInfo,
        now: DateTime<Utc>,
    ) -> Duration {
        // A machine sharing the keychain may have rotated already; rotating
        // with the token it revoked could cost its successor too.
        match self.env.load().await {
            Ok(Some(c)) if c.host == session.host && c.token == session.token => {}
            Ok(_) => {
                if self.watch.as_mut().is_some_and(|w| w.note(Note::Foreign)) {
                    info!(
                        "the keychain holds other credentials than the session; not rotating its token"
                    );
                }
                return self.pacing.recheck;
            }
            // Unreadable is unwritable: the new token would have no home.
            Err(e) => {
                let wait = self.retry_later();
                warn!(error = %e, retry_secs = wait.as_secs(), "keychain read failed; postponing the token rotation");
                return wait;
            }
        }

        let wanted = new_expiry(info, now.date_naive());
        let rotated = match session.gitlab.rotate_token(wanted).await {
            // Most likely past the instance's maximum lifetime.
            Err(Error::Gitlab(detail)) if wanted.is_some() => {
                info!(error = %detail, "GitLab refused the token's lifetime; rotating with its default one");
                session.gitlab.rotate_token(None).await
            }
            other => other,
        };
        let mut rotated = match rotated {
            Ok(rotated) => rotated,
            Err(e) => return self.failed(e, "rotating the GitLab token", true),
        };
        // Without it the lead isn't capped, and a short default lifetime
        // would be due again at once.
        if let Some(info) = &mut rotated.info {
            info.created_at.get_or_insert(now);
        }
        let expires_at = rotated.info.as_ref().and_then(|i| i.expires_at);
        info!(?expires_at, "rotated the GitLab token");

        let creds = Credentials {
            host: session.host.clone(),
            token: rotated.token,
        };
        if !self.persist(&creds).await {
            error!(
                "storing the rotated GitLab token keeps failing: the keychain holds a revoked \
                 token, so after a daemon restart `forskap auth login` with a new token is \
                 needed; still retrying"
            );
            tokio::spawn(keep_storing(
                Arc::clone(&self.env),
                Arc::clone(&self.session),
                creds.clone(),
                self.pacing,
            ));
        }

        let swapped = match self.connect(session, &creds).await {
            Some(fresh) => {
                let client = Arc::clone(&fresh.gitlab);
                commit_rotated(&self.session, session, fresh)
                    .await
                    .then_some(client)
            }
            None => None,
        };
        if let Some(client) = swapped {
            info!("session switched to the rotated GitLab token");
            // Without the lifetime the next evaluation reads it.
            if let Some(info) = &rotated.info {
                self.rotation.publish(&client, info);
            }
            self.watch = Some(Watch::new(&client, rotated.info));
            self.env.swapped();
            return Duration::ZERO;
        }
        if replaceable(&*self.session.read().await, session) {
            // The sync worker's next 401 reconnects from the keychain.
            if let Some(watch) = &mut self.watch {
                watch.refused = true;
            }
            return self.pacing.recheck;
        }
        info!("a login or logout overtook the token rotation");
        self.reconcile().await;
        self.unwatch();
        Duration::ZERO
    }

    /// Write `creds` to the keychain, with a few retries.
    async fn persist(&self, creds: &Credentials) -> bool {
        let mut delay = self.pacing.keychain_base;
        for attempt in 1..=KEYCHAIN_ATTEMPTS {
            match self.env.store(creds).await {
                Ok(()) => return true,
                Err(e) => warn!(attempt, error = %e, "storing the rotated GitLab token failed"),
            }
            if attempt < KEYCHAIN_ATTEMPTS {
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2);
            }
        }
        false
    }

    /// A session on the rotated token; `None` once GitLab refuses it or a
    /// login or logout took the slot.
    async fn connect(&self, old: &Session, creds: &Credentials) -> Option<Session> {
        let mut delay = self.pacing.retry_base;
        loop {
            match self.env.connect(&creds.host, &creds.token).await {
                Ok(fresh) => return Some(fresh),
                Err(e) if e.is_retryable(true) => {
                    warn!(error = %e, delay_secs = delay.as_secs(), "connecting with the rotated GitLab token failed; retrying");
                }
                Err(e) => {
                    error!(error = %e, "connecting with the rotated GitLab token was refused");
                    return None;
                }
            }
            tokio::time::sleep(delay).await;
            delay = next_backoff(delay, self.pacing.retry_max);
            if !replaceable(&*self.session.read().await, old) {
                return None;
            }
        }
    }

    /// The rotation lost the slot after it wrote the keychain: make the
    /// keychain the winner's again.
    async fn reconcile(&self) {
        let winner = match &*self.session.read().await {
            ConnState::Connected(s) => Some(Credentials {
                host: s.host.clone(),
                token: s.token.clone(),
            }),
            ConnState::Dormant(_) => None,
        };
        let restored = match &winner {
            Some(creds) => self.env.store(creds).await,
            None => self.env.forget().await,
        };
        if let Err(e) = restored {
            warn!(error = %e, "restoring the keychain after an overtaken token rotation failed");
        }
    }

    fn unwatch(&mut self) {
        self.watch = None;
        self.rotation.clear();
    }

    /// Step the backoff and return the wait it asks for.
    fn retry_later(&mut self) -> Duration {
        let Some(watch) = &mut self.watch else {
            return self.pacing.retry_base;
        };
        let wait = watch.backoff.unwrap_or(self.pacing.retry_base);
        watch.backoff = Some(next_backoff(wait, self.pacing.retry_max));
        wait
    }

    /// Sort a GitLab failure: retry with backoff, leave a dead token to the
    /// sync worker, or give this token up.
    fn failed(&mut self, e: Error, what: &str, loud: bool) -> Duration {
        if e.is_retryable(true) {
            let wait = self.retry_later();
            let wait = e.retry_after().map_or(wait, |r| r.max(wait));
            warn!(error = %e, retry_secs = wait.as_secs(), "{what} failed; retrying");
            return wait;
        }
        if matches!(e, Error::Unauthorized(_)) {
            debug!(error = %e, "{what} failed: the token is dead");
            return self.pacing.recheck;
        }
        if matches!(e, Error::RotationLost(_)) {
            error!(error = %e, "{what} may have revoked it without yielding a new one; `forskap auth login` with a new token is needed if GitLab rejects it from now on");
        } else if loud {
            warn!(error = %e, "{what} was refused; not rotating this token");
        } else {
            info!(error = %e, "{what} was refused; not rotating this token");
        }
        if let Some(watch) = &mut self.watch {
            watch.refused = true;
        }
        self.rotation.refuse();
        self.pacing.recheck
    }
}

/// Whether the slot still is the rotated session's to replace: it holds that
/// session, or what a failure of its revoked token left. A login or logout
/// since is not.
fn replaceable(state: &ConnState, rotated: &Session) -> bool {
    match state {
        ConnState::Connected(s) => Arc::ptr_eq(&s.gitlab, &rotated.gitlab),
        ConnState::Dormant(
            DormancyReason::Unreachable { host, .. } | DormancyReason::TokenRejected { host, .. },
        ) => *host == rotated.host,
        ConnState::Dormant(_) => false,
    }
}

/// Compare-and-set the slot to the session on the rotated token (see
/// [`replaceable`]). Returns whether it committed.
async fn commit_rotated(slot: &SessionSlot, rotated: &Session, fresh: Session) -> bool {
    let mut slot = slot.write().await;
    if !replaceable(&slot, rotated) {
        return false;
    }
    *slot = ConnState::Connected(fresh);
    true
}

/// Keep writing `creds` to the keychain until it works or a login or logout
/// made them obsolete.
async fn keep_storing<E: Env>(
    env: Arc<E>,
    session: SessionSlot,
    creds: Credentials,
    pacing: Pacing,
) {
    let mut delay = pacing.retry_base;
    loop {
        tokio::time::sleep(delay).await;
        delay = next_backoff(delay, pacing.retry_max);
        let obsolete = match &*session.read().await {
            ConnState::Connected(s) => s.token != creds.token,
            ConnState::Dormant(r) => {
                matches!(r, DormancyReason::LoggedOut | DormancyReason::NoCredentials)
            }
        };
        if obsolete {
            return;
        }
        match env.store(&creds).await {
            Ok(()) => {
                info!("stored the rotated GitLab token after all");
                return;
            }
            Err(e) => debug!(error = %e, "storing the rotated GitLab token still fails"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    use tokio::sync::RwLock;

    use crate::testing::{FakeErr, FakeGitlab, ROTATE_PATH, TOKEN_PATH, eventually};

    const HOST: &str = "gitlab.test";

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().to_utc()
    }

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    /// A token with `scopes`, created and expiring as given.
    fn token(scopes: &[&str], created: &str, expires: Option<&str>) -> TokenInfo {
        TokenInfo {
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            created_at: Some(at(created)),
            expires_at: expires.map(day),
        }
    }

    /// A year-long `self_rotate` token expiring 2026-12-31.
    fn yearly() -> TokenInfo {
        token(&["self_rotate"], "2025-12-31T10:00:00Z", Some("2026-12-31"))
    }

    fn auth(rotate: RotatePolicy) -> AuthConfig {
        AuthConfig {
            rotate,
            ..crate::config::defaults().auth
        }
    }

    type Hook = Box<dyn FnOnce() + Send>;

    struct FakeEnv {
        now: DateTime<Utc>,
        keychain: Mutex<Option<Credentials>>,
        load_failures: AtomicUsize,
        store_failures: AtomicUsize,
        connect_failures: Mutex<VecDeque<FakeErr>>,
        /// Runs inside the next connect: whatever overtakes the rotation.
        during_connect: Mutex<Option<Hook>>,
        swapped: AtomicUsize,
    }

    impl FakeEnv {
        /// At `now`, the keychain holding the token `old`.
        fn new(now: &str) -> Arc<Self> {
            Arc::new(Self {
                now: at(now),
                keychain: Mutex::new(Some(Credentials {
                    host: HOST.into(),
                    token: Token::new("old"),
                })),
                load_failures: AtomicUsize::new(0),
                store_failures: AtomicUsize::new(0),
                connect_failures: Mutex::default(),
                during_connect: Mutex::default(),
                swapped: AtomicUsize::new(0),
            })
        }

        fn stored(&self) -> Option<String> {
            let keychain = self.keychain.lock().unwrap();
            keychain.as_ref().map(|c| c.token.expose().to_string())
        }
    }

    /// Take one of `failures`, if any are left.
    fn take(failures: &AtomicUsize) -> bool {
        let mut left = failures.load(SeqCst);
        while left > 0 {
            match failures.compare_exchange(left, left - 1, SeqCst, SeqCst) {
                Ok(_) => return true,
                Err(now) => left = now,
            }
        }
        false
    }

    #[async_trait::async_trait]
    impl Env for FakeEnv {
        fn now(&self) -> DateTime<Utc> {
            self.now
        }

        async fn load(&self) -> Result<Option<Credentials>> {
            if take(&self.load_failures) {
                return Err(Error::Secrets("keyring locked".into()));
            }
            Ok(self.keychain.lock().unwrap().clone())
        }

        async fn store(&self, creds: &Credentials) -> Result<()> {
            if take(&self.store_failures) {
                return Err(Error::Secrets("keyring locked".into()));
            }
            *self.keychain.lock().unwrap() = Some(creds.clone());
            Ok(())
        }

        async fn forget(&self) -> Result<()> {
            *self.keychain.lock().unwrap() = None;
            Ok(())
        }

        async fn connect(&self, host: &str, token: &Token) -> Result<Session> {
            let hook = self.during_connect.lock().unwrap().take();
            if let Some(hook) = hook {
                hook();
            }
            if let Some(err) = self.connect_failures.lock().unwrap().pop_front() {
                return Err(err.error());
            }
            Ok(Session {
                gitlab: Arc::new(FakeGitlab::default()),
                host: host.into(),
                user_id: 42,
                username: "tester".into(),
                token: token.clone(),
            })
        }

        fn swapped(&self) {
            self.swapped.fetch_add(1, SeqCst);
        }
    }

    fn session_on(fake: &Arc<FakeGitlab>, token: &str) -> Session {
        Session {
            gitlab: Arc::clone(fake) as Arc<dyn GitlabApi>,
            host: HOST.into(),
            user_id: 42,
            username: "tester".into(),
            token: Token::new(token),
        }
    }

    struct Rig {
        supervisor: Supervisor<FakeEnv>,
        fake: Arc<FakeGitlab>,
        env: Arc<FakeEnv>,
        session: SessionSlot,
        config: SharedConfig,
        rotation: Arc<Rotation>,
    }

    /// A session on the token `old` with `info`, looked at at `now`; no
    /// waits.
    fn rig(info: TokenInfo, now: &str) -> Rig {
        let fake = Arc::new(FakeGitlab::default());
        fake.serve_token(info);
        let session: SessionSlot =
            Arc::new(RwLock::new(ConnState::Connected(session_on(&fake, "old"))));
        let config: SharedConfig = Arc::new(std::sync::RwLock::new(crate::config::defaults()));
        let rotation = Arc::new(Rotation::default());
        let env = FakeEnv::new(now);
        let supervisor = Supervisor {
            session: Arc::clone(&session),
            config: Arc::clone(&config),
            rotation: Arc::clone(&rotation),
            env: Arc::clone(&env),
            pacing: Pacing {
                recheck: Duration::from_secs(3600),
                retry_base: Duration::ZERO,
                retry_max: Duration::ZERO,
                keychain_base: Duration::ZERO,
            },
            spread: 0.0,
            watch: None,
        };
        Rig {
            supervisor,
            fake,
            env,
            session,
            config,
            rotation,
        }
    }

    /// The token the slot's session runs on; `None` while dormant.
    async fn live_token(session: &SessionSlot) -> Option<String> {
        match &*session.read().await {
            ConnState::Connected(s) => Some(s.token.expose().to_string()),
            ConnState::Dormant(_) => None,
        }
    }

    // ── The plan ───────────────────────────────────────────────────────

    #[test]
    fn a_long_lived_token_rotates_rotate_before_its_expiry() {
        let plan = plan(
            &auth(RotatePolicy::Scoped),
            &yearly(),
            at("2026-06-01T00:00:00Z"),
            0.0,
        );
        assert_eq!(plan, Plan::At(at("2026-12-24T00:00:00Z")));
    }

    #[test]
    fn a_short_lived_token_rotates_with_a_third_of_its_lifetime_left() {
        let info = token(&["self_rotate"], "2026-06-01T00:00:00Z", Some("2026-06-07"));
        let plan = plan(
            &auth(RotatePolicy::Scoped),
            &info,
            at("2026-06-01T00:00:00Z"),
            0.0,
        );
        assert_eq!(plan, Plan::At(at("2026-06-05T00:00:00Z")));
    }

    #[test]
    fn the_spread_moves_the_rotation_earlier_by_a_day_or_half_the_lead() {
        let scoped = auth(RotatePolicy::Scoped);
        let now = at("2026-06-01T00:00:00Z");
        assert_eq!(
            plan(&scoped, &yearly(), now, 0.5),
            Plan::At(at("2026-12-23T12:00:00Z"))
        );
        // A lead of two days spreads over one.
        let weekly = token(&["self_rotate"], "2026-06-01T00:00:00Z", Some("2026-06-07"));
        assert_eq!(
            plan(&scoped, &weekly, now, 0.5),
            Plan::At(at("2026-06-04T12:00:00Z"))
        );
        let unit = random_unit();
        assert!((0.0..1.0).contains(&unit), "{unit}");
    }

    #[test]
    fn the_policy_and_the_scopes_decide_whether_a_token_rotates() {
        let now = at("2026-06-01T00:00:00Z");
        let with = |scopes: &[&str]| token(scopes, "2025-12-31T10:00:00Z", Some("2026-12-31"));
        let cases = [
            (RotatePolicy::Scoped, vec!["self_rotate"], None),
            (RotatePolicy::Scoped, vec!["api", "self_rotate"], None),
            (RotatePolicy::Scoped, vec!["api"], Some(Why::ApiScopeOnly)),
            (RotatePolicy::Scoped, vec!["read_api"], Some(Why::NoScope)),
            (RotatePolicy::Always, vec!["api"], None),
            (RotatePolicy::Always, vec!["self_rotate"], None),
            (RotatePolicy::Always, vec!["read_api"], Some(Why::NoScope)),
            (
                RotatePolicy::Never,
                vec!["self_rotate"],
                Some(Why::Disabled),
            ),
        ];
        for (policy, scopes, idle) in cases {
            let got = plan(&auth(policy), &with(&scopes), now, 0.0);
            match idle {
                Some(why) => assert_eq!(got, Plan::Idle(why), "{policy:?} {scopes:?}"),
                None => assert!(matches!(got, Plan::At(_)), "{policy:?} {scopes:?}"),
            }
        }
    }

    #[test]
    fn a_token_without_an_expiry_or_past_it_is_left_alone() {
        let forever = token(&["self_rotate"], "2025-12-31T10:00:00Z", None);
        let scoped = auth(RotatePolicy::Scoped);
        assert_eq!(
            plan(&scoped, &forever, at("2026-06-01T00:00:00Z"), 0.0),
            Plan::Idle(Why::NoExpiry)
        );
        assert_eq!(
            plan(&scoped, &yearly(), at("2026-12-31T00:00:00Z"), 0.0),
            Plan::Idle(Why::Expired)
        );
    }

    #[test]
    fn a_huge_lead_saturates_instead_of_panicking() {
        let mut auth = auth(RotatePolicy::Scoped);
        auth.rotate_before_days = u64::MAX;
        let mut info = yearly();
        info.created_at = None;
        assert!(matches!(
            plan(&auth, &info, at("2026-06-01T00:00:00Z"), 0.0),
            Plan::At(_)
        ));
    }

    #[test]
    fn the_new_token_lives_as_long_as_the_old_one() {
        assert_eq!(
            new_expiry(&yearly(), day("2026-12-24")),
            Some(day("2027-12-24"))
        );
        // Never the old expiry again, or a one-day token would gain nothing.
        let daily = token(&["self_rotate"], "2026-06-01T08:00:00Z", Some("2026-06-02"));
        assert_eq!(
            new_expiry(&daily, day("2026-06-01")),
            Some(day("2026-06-03"))
        );
        let mut unknown = yearly();
        unknown.created_at = None;
        assert_eq!(new_expiry(&unknown, day("2026-12-24")), None);
    }

    // ── The supervisor ─────────────────────────────────────────────────

    #[tokio::test]
    async fn a_token_not_yet_due_is_read_once_and_waited_for() {
        let mut rig = rig(yearly(), "2026-12-23T23:30:00Z");

        assert_eq!(rig.supervisor.engage().await, Duration::from_secs(1800));
        assert_eq!(rig.supervisor.engage().await, Duration::from_secs(1800));

        assert_eq!(rig.fake.token_info_calls(), 1);
        assert!(rig.fake.rotations().is_empty());
        let client: Arc<dyn GitlabApi> = rig.fake.clone();
        assert_eq!(
            rig.rotation.report(&client, &auth(RotatePolicy::Scoped)),
            (Some(at("2026-12-31T00:00:00Z").timestamp()), true)
        );
        assert_eq!(
            rig.rotation.report(&client, &auth(RotatePolicy::Never)),
            (Some(at("2026-12-31T00:00:00Z").timestamp()), false)
        );
    }

    #[tokio::test]
    async fn a_distant_rotation_is_rechecked_instead_of_slept_out() {
        let mut rig = rig(yearly(), "2026-06-01T00:00:00Z");
        assert_eq!(rig.supervisor.engage().await, Duration::from_secs(3600));
    }

    #[tokio::test]
    async fn a_token_that_does_not_rotate_costs_one_read() {
        let api_only = token(&["api"], "2025-12-31T10:00:00Z", Some("2026-12-31"));
        let mut rig = rig(api_only, "2026-12-30T00:00:00Z");

        for _ in 0..3 {
            assert_eq!(rig.supervisor.engage().await, Duration::from_secs(3600));
        }

        assert_eq!(rig.fake.token_info_calls(), 1);
        assert!(rig.fake.rotations().is_empty());
        let client: Arc<dyn GitlabApi> = rig.fake.clone();
        let (expires_at, rotates) = rig.rotation.report(&client, &auth(RotatePolicy::Scoped));
        assert!(expires_at.is_some() && !rotates);
    }

    #[tokio::test]
    async fn a_due_token_is_rotated_stored_and_swapped_in() {
        let mut rig = rig(yearly(), "2026-12-25T09:00:00Z");

        assert_eq!(rig.supervisor.engage().await, Duration::ZERO);

        assert_eq!(rig.fake.rotations(), [Some(day("2027-12-25"))]);
        assert_eq!(rig.env.stored().as_deref(), Some("rotated-1"));
        assert_eq!(live_token(&rig.session).await.as_deref(), Some("rotated-1"));
        assert_eq!(rig.env.swapped.load(SeqCst), 1);

        // The new token is known from the rotation: it isn't due, and
        // nothing is read again.
        assert_eq!(rig.supervisor.engage().await, Duration::from_secs(3600));
        assert_eq!(rig.fake.rotations().len(), 1);
        let ConnState::Connected(fresh) = &*rig.session.read().await else {
            panic!("connected");
        };
        assert_eq!(
            rig.rotation
                .report(&fresh.gitlab, &auth(RotatePolicy::Scoped)),
            (Some(at("2027-12-25T00:00:00Z").timestamp()), true)
        );
    }

    /// A fetch in flight on the revoked token fails with a 401 after the
    /// swap: it must not take the new session down.
    #[tokio::test]
    async fn a_stale_401_of_the_rotated_token_cannot_park_the_new_session() {
        let mut rig = rig(yearly(), "2026-12-25T09:00:00Z");
        rig.supervisor.engage().await;

        let old: Arc<dyn GitlabApi> = rig.fake.clone();
        crate::reconnect::commit_token_rejected(&rig.session, &old, "401".into()).await;
        let signal = Notify::new();
        crate::reconnect::commit_token_replaced(&rig.session, &signal, &old).await;
        crate::reconnect::commit_unreachable(&rig.session, &signal, &old, "stale".into()).await;

        assert_eq!(live_token(&rig.session).await.as_deref(), Some("rotated-1"));
    }

    #[tokio::test]
    async fn a_refused_lifetime_falls_back_to_gitlabs_default() {
        let mut rig = rig(yearly(), "2026-12-25T09:00:00Z");
        rig.fake.fail_next(ROTATE_PATH, FakeErr::Rejected);

        assert_eq!(rig.supervisor.engage().await, Duration::ZERO);

        assert_eq!(rig.fake.rotations(), [Some(day("2027-12-25")), None]);
        assert_eq!(live_token(&rig.session).await.as_deref(), Some("rotated-2"));
        assert_eq!(rig.env.stored().as_deref(), Some("rotated-2"));
    }

    /// The token may be revoked already: using it for another attempt
    /// could cost its successor.
    #[tokio::test]
    async fn an_unusable_rotation_answer_is_not_followed_by_another_attempt() {
        let mut rig = rig(yearly(), "2026-12-25T09:00:00Z");
        rig.fake.fail_next(ROTATE_PATH, FakeErr::Lost);

        rig.supervisor.engage().await;
        rig.supervisor.engage().await;

        assert_eq!(rig.fake.rotations().len(), 1);
        assert!(matches!(
            &*rig.session.read().await,
            ConnState::Connected(_)
        ));
    }

    #[tokio::test]
    async fn a_transient_or_throttled_failure_retries_without_demoting() {
        for err in [
            FakeErr::Transient,
            FakeErr::Throttled(429),
            FakeErr::Throttled(503),
        ] {
            let mut rig = rig(yearly(), "2026-12-25T09:00:00Z");
            rig.fake.fail_next(TOKEN_PATH, err);
            rig.fake.fail_next(ROTATE_PATH, err);

            // The read fails, then the rotation, then both work.
            assert_eq!(rig.supervisor.engage().await, Duration::ZERO, "{err:?}");
            assert!(rig.fake.rotations().is_empty());
            assert_eq!(rig.supervisor.engage().await, Duration::ZERO, "{err:?}");
            assert_eq!(live_token(&rig.session).await.as_deref(), Some("old"));
            assert_eq!(rig.env.stored().as_deref(), Some("old"));

            rig.supervisor.engage().await;
            assert_eq!(
                live_token(&rig.session).await.as_deref(),
                Some("rotated-2"),
                "{err:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_retry_backs_off_and_honours_retry_after() {
        let mut rig = rig(yearly(), "2026-12-25T09:00:00Z");
        rig.supervisor.pacing.retry_base = Duration::from_secs(30);
        rig.supervisor.pacing.retry_max = Duration::from_secs(100);
        for _ in 0..3 {
            rig.fake.fail_next(ROTATE_PATH, FakeErr::Transient);
        }

        for secs in [30, 60, 100] {
            assert_eq!(rig.supervisor.engage().await, Duration::from_secs(secs));
        }

        let throttled = Error::Throttled {
            status: 429,
            retry_after: Some(Duration::from_secs(500)),
            detail: "busy".into(),
        };
        assert_eq!(
            rig.supervisor.failed(throttled, "rotating", true),
            Duration::from_secs(500)
        );
    }

    #[tokio::test]
    async fn a_rejection_gives_the_token_up_without_demoting() {
        let mut rig = rig(yearly(), "2026-12-25T09:00:00Z");
        // Both the asked-for and the default lifetime.
        rig.fake.fail_next(ROTATE_PATH, FakeErr::Rejected);
        rig.fake.fail_next(ROTATE_PATH, FakeErr::Rejected);

        assert_eq!(rig.supervisor.engage().await, Duration::from_secs(3600));
        assert_eq!(rig.supervisor.engage().await, Duration::from_secs(3600));

        assert_eq!(
            rig.fake.rotations().len(),
            2,
            "no attempt after the refusal"
        );
        assert_eq!(live_token(&rig.session).await.as_deref(), Some("old"));
        assert_eq!(rig.env.stored().as_deref(), Some("old"));
        let client: Arc<dyn GitlabApi> = rig.fake.clone();
        let (_, rotates) = rig.rotation.report(&client, &auth(RotatePolicy::Scoped));
        assert!(!rotates);
    }

    /// A dead token is the sync worker's to park.
    #[tokio::test]
    async fn a_401_is_left_to_the_sync_worker() {
        let mut rig = rig(yearly(), "2026-12-25T09:00:00Z");
        rig.fake.fail_next(ROTATE_PATH, FakeErr::Unauthorized);

        assert_eq!(rig.supervisor.engage().await, Duration::from_secs(3600));

        assert_eq!(rig.fake.rotations().len(), 1, "no default-lifetime retry");
        assert_eq!(live_token(&rig.session).await.as_deref(), Some("old"));
    }

    #[tokio::test]
    async fn an_unreadable_token_is_not_rotated() {
        let mut rig = rig(yearly(), "2026-12-25T09:00:00Z");
        rig.fake.fail_next(TOKEN_PATH, FakeErr::Rejected);

        assert_eq!(rig.supervisor.engage().await, Duration::from_secs(3600));
        assert_eq!(rig.supervisor.engage().await, Duration::from_secs(3600));

        assert_eq!(rig.fake.token_info_calls(), 1);
        assert!(rig.fake.rotations().is_empty());
        let client: Arc<dyn GitlabApi> = rig.fake.clone();
        assert_eq!(
            rig.rotation.report(&client, &auth(RotatePolicy::Scoped)),
            (None, false)
        );
    }

    #[tokio::test]
    async fn a_keychain_hiccup_delays_the_swap_not_the_token() {
        let mut rig = rig(yearly(), "2026-12-25T09:00:00Z");
        rig.env
            .store_failures
            .store(KEYCHAIN_ATTEMPTS as usize - 1, SeqCst);

        rig.supervisor.engage().await;

        assert_eq!(rig.env.stored().as_deref(), Some("rotated-1"));
        assert_eq!(live_token(&rig.session).await.as_deref(), Some("rotated-1"));
    }

    #[tokio::test]
    async fn a_failing_keychain_still_swaps_and_is_retried_in_the_background() {
        let mut rig = rig(yearly(), "2026-12-25T09:00:00Z");
        rig.supervisor.pacing.retry_base = Duration::from_millis(20);
        rig.supervisor.pacing.retry_max = Duration::from_millis(20);
        rig.env
            .store_failures
            .store(KEYCHAIN_ATTEMPTS as usize + 2, SeqCst);

        rig.supervisor.engage().await;

        assert_eq!(
            live_token(&rig.session).await.as_deref(),
            Some("rotated-1"),
            "the daemon keeps working on the new token"
        );
        assert_eq!(rig.env.stored().as_deref(), Some("old"));
        eventually("the background keychain write", || {
            rig.env.stored().as_deref() == Some("rotated-1")
        })
        .await;
    }

    #[tokio::test]
    async fn the_background_write_yields_to_a_later_login() {
        let mut rig = rig(yearly(), "2026-12-25T09:00:00Z");
        rig.supervisor.pacing.retry_base = Duration::from_millis(20);
        rig.supervisor.pacing.retry_max = Duration::from_millis(20);
        rig.env
            .store_failures
            .store(KEYCHAIN_ATTEMPTS as usize, SeqCst);

        rig.supervisor.engage().await;
        let login = session_on(&Arc::new(FakeGitlab::default()), "pasted");
        *rig.session.write().await = ConnState::Connected(login);
        *rig.env.keychain.lock().unwrap() = Some(Credentials {
            host: HOST.into(),
            token: Token::new("pasted"),
        });

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(rig.env.stored().as_deref(), Some("pasted"));
    }

    #[tokio::test]
    async fn an_unreadable_keychain_postpones_the_rotation() {
        let mut rig = rig(yearly(), "2026-12-25T09:00:00Z");
        rig.env.load_failures.store(1, SeqCst);

        assert_eq!(rig.supervisor.engage().await, Duration::ZERO);
        assert!(rig.fake.rotations().is_empty());

        rig.supervisor.engage().await;
        assert_eq!(rig.fake.rotations().len(), 1);
    }

    /// Another machine sharing the keychain rotated first.
    #[tokio::test]
    async fn a_token_the_keychain_no_longer_holds_is_not_rotated() {
        let mut rig = rig(yearly(), "2026-12-25T09:00:00Z");
        *rig.env.keychain.lock().unwrap() = Some(Credentials {
            host: HOST.into(),
            token: Token::new("rotated-elsewhere"),
        });

        assert_eq!(rig.supervisor.engage().await, Duration::from_secs(3600));

        assert!(rig.fake.rotations().is_empty());
        assert_eq!(rig.env.stored().as_deref(), Some("rotated-elsewhere"));
        assert_eq!(live_token(&rig.session).await.as_deref(), Some("old"));
    }

    #[tokio::test]
    async fn a_login_during_the_rotation_wins() {
        let mut rig = rig(yearly(), "2026-12-25T09:00:00Z");
        let slot = Arc::clone(&rig.session);
        let hook: Hook = Box::new(move || {
            let login = session_on(&Arc::new(FakeGitlab::default()), "pasted");
            *slot.try_write().unwrap() = ConnState::Connected(login);
        });
        *rig.env.during_connect.lock().unwrap() = Some(hook);

        assert_eq!(rig.supervisor.engage().await, Duration::ZERO);

        assert_eq!(live_token(&rig.session).await.as_deref(), Some("pasted"));
        assert_eq!(
            rig.env.stored().as_deref(),
            Some("pasted"),
            "the keychain is the login's again"
        );
        assert_eq!(rig.env.swapped.load(SeqCst), 0);
    }

    #[tokio::test]
    async fn a_logout_during_the_rotation_wins() {
        let mut rig = rig(yearly(), "2026-12-25T09:00:00Z");
        let slot = Arc::clone(&rig.session);
        let hook: Hook = Box::new(move || {
            *slot.try_write().unwrap() = ConnState::Dormant(DormancyReason::LoggedOut);
        });
        *rig.env.during_connect.lock().unwrap() = Some(hook);

        rig.supervisor.engage().await;

        assert_eq!(live_token(&rig.session).await, None);
        assert!(matches!(
            &*rig.session.read().await,
            ConnState::Dormant(DormancyReason::LoggedOut)
        ));
        assert_eq!(rig.env.stored(), None, "a logout leaves no token behind");
    }

    /// The sync worker ran into the revoked token before the swap.
    #[tokio::test]
    async fn a_session_the_revoked_token_parked_is_revived() {
        let mut rig = rig(yearly(), "2026-12-25T09:00:00Z");
        let slot = Arc::clone(&rig.session);
        let hook: Hook = Box::new(move || {
            *slot.try_write().unwrap() = ConnState::Dormant(DormancyReason::TokenRejected {
                host: HOST.into(),
                detail: "401".into(),
            });
        });
        *rig.env.during_connect.lock().unwrap() = Some(hook);

        rig.supervisor.engage().await;

        assert_eq!(live_token(&rig.session).await.as_deref(), Some("rotated-1"));
    }

    #[tokio::test]
    async fn a_blip_connecting_with_the_new_token_is_retried() {
        let mut rig = rig(yearly(), "2026-12-25T09:00:00Z");
        rig.env
            .connect_failures
            .lock()
            .unwrap()
            .extend([FakeErr::Transient, FakeErr::Throttled(502)]);

        rig.supervisor.engage().await;

        assert_eq!(live_token(&rig.session).await.as_deref(), Some("rotated-1"));
    }

    #[tokio::test]
    async fn a_new_token_that_does_not_connect_is_stored_and_not_rotated_again() {
        let mut rig = rig(yearly(), "2026-12-25T09:00:00Z");
        rig.env
            .connect_failures
            .lock()
            .unwrap()
            .push_back(FakeErr::Rejected);

        assert_eq!(rig.supervisor.engage().await, Duration::from_secs(3600));
        assert_eq!(rig.supervisor.engage().await, Duration::from_secs(3600));

        assert_eq!(rig.fake.rotations().len(), 1);
        assert_eq!(rig.env.stored().as_deref(), Some("rotated-1"));
        assert_eq!(live_token(&rig.session).await.as_deref(), Some("old"));
    }

    #[tokio::test]
    async fn a_dormant_daemon_touches_nothing() {
        let mut rig = rig(yearly(), "2026-12-25T09:00:00Z");
        *rig.session.write().await = ConnState::Dormant(DormancyReason::NoCredentials);

        assert_eq!(rig.supervisor.engage().await, Duration::from_secs(3600));

        assert_eq!(rig.fake.token_info_calls(), 0);
        assert!(rig.fake.rotations().is_empty());
    }

    #[tokio::test]
    async fn a_config_change_applies_at_the_next_look() {
        let mut rig = rig(yearly(), "2026-12-25T09:00:00Z");
        rig.config.write().unwrap().auth.rotate = RotatePolicy::Never;
        rig.supervisor.engage().await;
        assert!(rig.fake.rotations().is_empty());

        rig.config.write().unwrap().auth.rotate = RotatePolicy::Scoped;
        rig.supervisor.engage().await;
        assert_eq!(rig.fake.rotations().len(), 1);
    }

    /// The loop itself: a wakeup makes it look at the session again.
    #[tokio::test]
    async fn the_supervisor_looks_again_when_woken() {
        let rig = rig(yearly(), "2026-06-01T00:00:00Z");
        let (first, rotation, session) = (rig.fake, rig.rotation, rig.session);
        let run = tokio::spawn(rig.supervisor.run());
        eventually("the first token read", || first.token_info_calls() == 1).await;

        let second = Arc::new(FakeGitlab::default());
        second.serve_token(token(
            &["self_rotate"],
            "2026-01-01T00:00:00Z",
            Some("2026-06-03"),
        ));
        *session.write().await = ConnState::Connected(session_on(&second, "old"));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(second.token_info_calls(), 0, "parked until woken");

        rotation.reevaluate();
        eventually("the login's token to rotate", || {
            second.rotations().len() == 1
        })
        .await;
        assert_eq!(first.token_info_calls(), 1);

        run.abort();
    }
}
