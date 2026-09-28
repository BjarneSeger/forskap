//! The sync worker: the only task that reads from GitLab and the only writer
//! of the [`SyncStore`].
//!
//! It runs one job at a time: a demanded one first (by priority), else the
//! planned job that is due, lowest priority class first. Per-job jittered
//! due times plus a jittered pause between jobs keep requests spread out. A
//! 429 pauses the whole worker; a 5xx or a rejection backs off only its job;
//! a network error demotes the session and parks the worker until the
//! reconnect supervisor wakes it.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{Notify, mpsc, oneshot};
use tracing::{debug, error, info, warn};

use super::jobs::{self, ASSIGNED_ISSUES, ASSIGNED_MERGE_REQUESTS, FetchCtx, Job, Staged, Windows};
use super::model::{Board, Group, Issue, MergeRequest, Project, Timelog};
use super::now_secs;
use super::planner::{self, Plan};
use super::schedule::{
    self, JobState, RATE_LIMIT_PAUSE_CAP, REJECTED_BACKOFF_CAP, SERVER_BACKOFF_CAP,
};
use super::store::{Identity, RowScope, SyncStore};
use crate::config::SharedConfig;
use crate::error::{Error, Result};
use crate::handlers::{ConnState, Session, SessionSlot};
use crate::write::Write;

/// How often a dormant worker re-checks the session without being woken.
const DORMANT_RECHECK: Duration = Duration::from_secs(60);
/// Longest idle sleep, a safety net under the computed next due time.
const IDLE_RECHECK_SECS: u64 = 600;
/// How long a noted write can hide an item from an assigned view.
const NOTED_WRITE_TTL_SECS: u64 = 86_400;

/// A slice of synced state to drop, for `ClearCache`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clear {
    /// Every synced row, view and job state.
    Everything,
    /// The assigned issue/MR views and the board labels.
    Assigned,
    /// Issues, MRs, projects and groups.
    Corpus,
    /// Timelogs spent in `[from, until)`.
    Timelogs { from: u64, until: u64 },
}

impl Clear {
    /// Whether clearing this slice resets the job with state key `key`.
    fn resets(self, key: &str) -> bool {
        match self {
            Self::Everything => true,
            Self::Assigned => {
                key == ASSIGNED_ISSUES || key == ASSIGNED_MERGE_REQUESTS || key.ends_with("/boards")
            }
            Self::Corpus => {
                key.starts_with("member/")
                    || key.ends_with("/issues")
                    || key.ends_with("/merge_requests")
            }
            Self::Timelogs { .. } => key.starts_with("timelogs/"),
        }
    }
}

enum Command {
    /// Run the job next; the sender (if any) is dropped once it ran, failed,
    /// or can't run (unplanned, dormant).
    Run(Job, Option<oneshot::Sender<()>>),
    Clear(Clear, oneshot::Sender<()>),
    /// The config changed: re-plan with it.
    Reconfigure,
    /// The session may have changed.
    Wake,
}

struct NotedWrite {
    write: Write,
    at: u64,
}

/// The handlers' side of the sync layer: read access to the store plus
/// commands for the worker.
pub struct SyncHandle {
    store: Arc<SyncStore>,
    tx: mpsc::UnboundedSender<Command>,
    noted: Mutex<Vec<NotedWrite>>,
}

impl SyncHandle {
    /// Start the worker. It stops once the returned handle is dropped.
    pub fn spawn(
        store: Arc<SyncStore>,
        session: SessionSlot,
        config: SharedConfig,
        reconnect_signal: Arc<Notify>,
    ) -> Arc<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        let worker = Worker {
            states: store
                .job_states()
                .unwrap_or_else(|e| {
                    warn!(error = %e, "reading sync job states failed; treating all as never run");
                    Vec::new()
                })
                .into_iter()
                .collect(),
            store: Arc::clone(&store),
            session,
            config,
            reconnect_signal,
            rx,
            plan: Plan::default(),
            identity: None,
            boot: now_secs(),
            demand: BTreeMap::new(),
            paused_until: 0,
            rate_limits: 0,
            runs: 0,
            replan: true,
        };
        tokio::spawn(worker.run());
        Arc::new(Self {
            store,
            tx,
            noted: Mutex::new(Vec::new()),
        })
    }

    pub fn store(&self) -> &SyncStore {
        &self.store
    }

    /// The session may have changed (login, reconnect).
    pub fn wake(&self) {
        let _ = self.tx.send(Command::Wake);
    }

    /// The config was reloaded.
    pub fn reconfigure(&self) {
        let _ = self.tx.send(Command::Reconfigure);
    }

    /// Run `jobs` ahead of the schedule and wait until each ran or can't run
    /// (unplanned, dormant). Callers bound the wait with a timeout: a job
    /// can queue behind a long one.
    pub async fn refresh_now(&self, jobs: &[Job]) {
        let waits: Vec<_> = jobs
            .iter()
            .map(|&job| {
                let (done, wait) = oneshot::channel();
                let _ = self.tx.send(Command::Run(job, Some(done)));
                wait
            })
            .collect();
        for wait in waits {
            let _ = wait.await;
        }
    }

    /// Run `jobs` ahead of the schedule, without waiting.
    pub fn refresh_soon(&self, jobs: &[Job]) {
        for &job in jobs {
            let _ = self.tx.send(Command::Run(job, None));
        }
    }

    /// Drop a slice of synced state; an in-flight fetch into it is cancelled.
    /// The affected jobs become due at once.
    pub async fn clear(&self, what: Clear) {
        let (done, wait) = oneshot::channel();
        if self.tx.send(Command::Clear(what, done)).is_ok() {
            let _ = wait.await;
        }
    }

    /// Whether `job` has completed at least once.
    pub fn has_synced(&self, job: Job) -> bool {
        match self.store.job_state(&job.key()) {
            Ok(s) => s.last_ok > 0,
            Err(e) => {
                warn!(error = %e, "sync job state read failed; treating as never synced");
                false
            }
        }
    }

    /// Remember that `write` just reached GitLab, so views fetched before it
    /// can be corrected at read time (see [`Self::writes_since`]).
    pub fn note_write(&self, write: &Write) {
        let now = now_secs();
        let mut noted = self.noted.lock().unwrap();
        noted.retain(|w| now.saturating_sub(w.at) < NOTED_WRITE_TTL_SECS);
        noted.push(NotedWrite {
            write: write.clone(),
            at: now,
        });
    }

    /// Writes noted at or after `since` (a view's fetch start): the ones that
    /// view may not reflect yet.
    pub fn writes_since(&self, since: u64) -> Vec<Write> {
        self.noted
            .lock()
            .unwrap()
            .iter()
            .filter(|w| w.at >= since)
            .map(|w| w.write.clone())
            .collect()
    }
}

enum Next {
    Run(Job),
    /// Nothing is due before this time.
    Idle(u64),
}

struct Worker {
    store: Arc<SyncStore>,
    session: SessionSlot,
    config: SharedConfig,
    reconnect_signal: Arc<Notify>,
    rx: mpsc::UnboundedReceiver<Command>,
    plan: Plan,
    /// Mirror of the persisted job states; the worker is their only writer.
    states: HashMap<String, JobState>,
    /// Identity already checked against the store this run.
    identity: Option<Identity>,
    boot: u64,
    /// Demanded jobs and whoever waits on them.
    demand: BTreeMap<Job, Vec<oneshot::Sender<()>>>,
    paused_until: u64,
    rate_limits: u32,
    runs: u64,
    replan: bool,
}

impl Worker {
    async fn run(mut self) {
        loop {
            if self.replan {
                self.replan_now();
            }
            if !self.drain() {
                break;
            }
            let session = match &*self.session.read().await {
                ConnState::Connected(s) => Some(s.clone()),
                ConnState::Dormant(_) => None,
            };
            let Some(session) = session else {
                self.demand.clear();
                if !self.wait(Some(DORMANT_RECHECK)).await {
                    break;
                }
                continue;
            };
            if let Err(e) = self.check_identity(&session) {
                warn!(error = %e, "sync identity check failed");
            }
            if self.replan {
                self.replan_now();
            }

            let now = now_secs();
            if self.paused_until > now {
                let pause = Duration::from_secs(self.paused_until - now);
                if !self.wait(Some(pause)).await {
                    break;
                }
                continue;
            }
            match self.next_job(now) {
                Next::Run(job) => {
                    if !self.run_job(job, &session).await {
                        break;
                    }
                    let gap = self.gap();
                    if !gap.is_zero() && !self.wait(Some(gap)).await {
                        break;
                    }
                }
                Next::Idle(until) => {
                    let secs = until.saturating_sub(now).clamp(1, IDLE_RECHECK_SECS);
                    if !self.wait(Some(Duration::from_secs(secs))).await {
                        break;
                    }
                }
            }
        }
        debug!("sync worker stopped");
    }

    /// Handle every queued command; false once all handles are gone.
    fn drain(&mut self) -> bool {
        loop {
            match self.rx.try_recv() {
                Ok(cmd) => self.handle(cmd),
                Err(mpsc::error::TryRecvError::Empty) => return true,
                Err(mpsc::error::TryRecvError::Disconnected) => return false,
            }
        }
    }

    /// Wait up to `timeout` for a command and handle it; false once all
    /// handles are gone.
    async fn wait(&mut self, timeout: Option<Duration>) -> bool {
        let cmd = match timeout {
            Some(t) => match tokio::time::timeout(t, self.rx.recv()).await {
                Ok(cmd) => cmd,
                Err(_) => return true,
            },
            None => self.rx.recv().await,
        };
        match cmd {
            Some(cmd) => {
                self.handle(cmd);
                true
            }
            None => false,
        }
    }

    fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::Run(job, done) => {
                if self.plan.jobs.contains(&job) {
                    self.demand.entry(job).or_default().extend(done);
                }
            }
            Command::Clear(what, done) => {
                self.apply_clear(what);
                let _ = done.send(());
            }
            Command::Reconfigure => self.replan = true,
            Command::Wake => {}
        }
    }

    fn next_job(&self, now: u64) -> Next {
        if let Some(&job) = self.demand.keys().min_by_key(|j| (j.priority(), **j)) {
            return Next::Run(job);
        }
        let cfg = self.config.read().unwrap();
        let (jitter, spread) = (cfg.sync.jitter, cfg.sync.startup_spread_secs);
        let mut best: Option<(u8, u64, Job)> = None;
        let mut soonest = u64::MAX;
        for &job in &self.plan.jobs {
            let key = job.key();
            let state = self.states.get(&key).copied().unwrap_or_default();
            let mut at = schedule::due_at(
                &key,
                &state,
                job.cadence(&cfg),
                job.fingerprint(&cfg),
                jitter,
            );
            if job.priority() > 0 {
                at = at.max(self.boot + schedule::startup_offset(&key, spread));
            }
            if at <= now {
                let candidate = (job.priority(), at, job);
                if best.is_none_or(|b| candidate < b) {
                    best = Some(candidate);
                }
            } else {
                soonest = soonest.min(at);
            }
        }
        match best {
            Some((_, _, job)) => Next::Run(job),
            None => Next::Idle(soonest),
        }
    }

    /// Run one job to completion (or cancellation); false once all handles
    /// are gone.
    async fn run_job(&mut self, job: Job, session: &Session) -> bool {
        let waiters = self.demand.remove(&job);
        let key = job.key();
        let state = self.states.get(&key).copied().unwrap_or_default();
        let started = now_secs();
        let (full, fingerprint, windows) = {
            let cfg = self.config.read().unwrap();
            let fingerprint = job.fingerprint(&cfg);
            let full = schedule::run_is_full(
                &key,
                &state,
                job.cadence(&cfg),
                fingerprint,
                cfg.sync.jitter,
                started,
            );
            (full, fingerprint, Windows::from_config(&cfg))
        };
        debug!(job = %key, full, "sync job starting");
        self.runs += 1;

        // Spawned so a panic is a failed job, not a dead worker; commands
        // keep flowing while it runs, and a clear cancels it before it
        // commits anything.
        let mut fetch = tokio::spawn(jobs::fetch(
            job,
            FetchCtx {
                gitlab: Arc::clone(&session.gitlab),
                full,
                state,
                started,
                windows,
            },
        ));
        let joined = loop {
            tokio::select! {
                joined = &mut fetch => break Some(joined),
                cmd = self.rx.recv() => match cmd {
                    Some(Command::Clear(what, done)) => {
                        fetch.abort();
                        self.apply_clear(what);
                        let _ = done.send(());
                        break None;
                    }
                    Some(cmd) => self.handle(cmd),
                    None => {
                        fetch.abort();
                        return false;
                    }
                },
            }
        };
        match joined {
            None => info!(job = %key, "sync job cancelled by a cache clear"),
            Some(Ok(Ok(staged))) => {
                self.commit(job, &key, state, staged, started, full, fingerprint)
            }
            Some(Ok(Err(e))) => self.on_error(&key, state, e, session).await,
            Some(Err(e)) => {
                error!(job = %key, error = %e, "sync job panicked");
                self.back_off(&key, state, REJECTED_BACKOFF_CAP);
            }
        }
        // Released only now, so a waiter reads the committed rows.
        drop(waiters);
        true
    }

    #[allow(clippy::too_many_arguments)]
    fn commit(
        &mut self,
        job: Job,
        key: &str,
        state: JobState,
        staged: Staged,
        started: u64,
        full: bool,
        fingerprint: u64,
    ) {
        let fresh = JobState {
            last_ok: started,
            last_full: if full { started } else { state.last_full },
            failures: 0,
            retry_at: 0,
            fingerprint,
        };
        let committed = (|| -> Result<usize> {
            let mut c = self.store.begin();
            let rows = staged.apply(&mut c)?;
            c.set_job(key, &fresh)?;
            c.commit()?;
            Ok(rows)
        })();
        match committed {
            Ok(rows) => {
                self.states.insert(key.to_string(), fresh);
                self.rate_limits = 0;
                if full {
                    info!(job = %key, rows, "synced (full)");
                } else {
                    debug!(job = %key, rows, "synced (delta)");
                }
                if job.feeds_plan() {
                    self.replan = true;
                }
            }
            Err(e) => {
                warn!(job = %key, error = %e, "storing sync result failed");
                self.back_off(key, state, SERVER_BACKOFF_CAP);
            }
        }
    }

    async fn on_error(&mut self, key: &str, state: JobState, e: Error, session: &Session) {
        match &e {
            Error::Transient(detail) => {
                warn!(job = %key, error = %e, "sync fetch failed; GitLab unreachable");
                crate::reconnect::commit_unreachable(
                    &self.session,
                    &self.reconnect_signal,
                    &session.gitlab,
                    detail.clone(),
                )
                .await;
            }
            Error::Throttled {
                status: 429,
                retry_after,
                ..
            } => {
                self.rate_limits += 1;
                let pause = retry_after.map_or_else(
                    || schedule::backoff(self.rate_limits, RATE_LIMIT_PAUSE_CAP),
                    |d| d.as_secs(),
                );
                self.paused_until = now_secs() + pause.max(1);
                warn!(job = %key, pause_secs = pause, "GitLab rate limit hit; pausing the sync");
            }
            Error::Throttled { .. } => {
                warn!(job = %key, error = %e, "GitLab failed the sync fetch; backing off");
                self.back_off(key, state, SERVER_BACKOFF_CAP);
            }
            _ => {
                warn!(job = %key, error = %e, "GitLab rejected the sync fetch; backing off");
                self.back_off(key, state, REJECTED_BACKOFF_CAP);
            }
        }
    }

    fn back_off(&mut self, key: &str, state: JobState, cap: u64) {
        let jitter = self.config.read().unwrap().sync.jitter;
        let failures = state.failures.saturating_add(1);
        let delay = schedule::jittered(
            schedule::backoff(failures, cap),
            key,
            u64::from(failures),
            jitter,
        );
        let next = JobState {
            failures,
            retry_at: now_secs() + delay.max(1),
            ..state
        };
        let persisted = (|| -> Result<()> {
            let mut c = self.store.begin();
            c.set_job(key, &next)?;
            c.commit()
        })();
        if let Err(e) = persisted {
            warn!(job = %key, error = %e, "storing sync backoff failed");
        }
        self.states.insert(key.to_string(), next);
    }

    fn gap(&self) -> Duration {
        let cfg = self.config.read().unwrap();
        let ms = schedule::jittered(cfg.sync.job_gap_ms, "gap", self.runs, cfg.sync.jitter);
        Duration::from_millis(ms)
    }

    fn apply_clear(&mut self, what: Clear) {
        let reset: Vec<String> = self
            .states
            .keys()
            .filter(|k| what.resets(k))
            .cloned()
            .collect();
        let cleared = (|| -> Result<()> {
            let mut c = self.store.begin();
            match what {
                Clear::Everything => c.wipe()?,
                Clear::Assigned => {
                    c.remove_view(ASSIGNED_ISSUES);
                    c.remove_view(ASSIGNED_MERGE_REQUESTS);
                    c.remove_where::<Board>(RowScope::All, |_| false)?;
                }
                Clear::Corpus => {
                    c.remove_where::<Issue>(RowScope::All, |_| false)?;
                    c.remove_where::<MergeRequest>(RowScope::All, |_| false)?;
                    c.remove_where::<Project>(RowScope::All, |_| false)?;
                    c.remove_where::<Group>(RowScope::All, |_| false)?;
                }
                Clear::Timelogs { from, until } => {
                    c.remove_where::<Timelog>(RowScope::Since(from), |k| k.0 >= until)?;
                }
            }
            for key in &reset {
                c.remove_job(key);
            }
            c.commit()
        })();
        match cleared {
            Ok(()) => info!(?what, jobs_reset = reset.len(), "synced data cleared"),
            Err(e) => warn!(?what, error = %e, "clearing synced data failed"),
        }
        for key in &reset {
            self.states.remove(key);
        }
        self.replan = true;
    }

    /// Make sure the store holds `session`'s account; another account's data
    /// is wiped first.
    fn check_identity(&mut self, session: &Session) -> Result<()> {
        let me = Identity {
            host: session.host.clone(),
            user_id: session.user_id,
        };
        if self.identity.as_ref() == Some(&me) {
            return Ok(());
        }
        let mut c = self.store.begin();
        if let Some(known) = self.store.identity()?.filter(|k| *k != me) {
            info!(
                from_host = %known.host,
                from_user = known.user_id,
                host = %me.host,
                user_id = me.user_id,
                "GitLab account changed; dropping the previous account's synced data"
            );
            c.wipe()?;
            self.states.clear();
            self.replan = true;
        }
        c.set_identity(&me)?;
        c.commit()?;
        self.identity = Some(me);
        Ok(())
    }

    fn replan_now(&mut self) {
        self.replan = false;
        let (population, window) = {
            let cfg = self.config.read().unwrap();
            (
                cfg.search.population,
                cfg.search.tracked_retention().as_secs(),
            )
        };
        let since = now_secs().saturating_sub(window);
        let plan = match planner::plan(&self.store, population, since) {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, "sync planning failed; keeping the previous plan");
                return;
            }
        };
        if plan.jobs == self.plan.jobs {
            self.plan = plan;
            return;
        }
        let dropped: Vec<Job> = self.plan.jobs.difference(&plan.jobs).copied().collect();
        let added = plan.jobs.difference(&self.plan.jobs).count();
        let collected = (|| -> Result<usize> {
            let mut c = self.store.begin();
            for job in &dropped {
                c.remove_job(&job.key());
            }
            let removed = planner::collect_garbage(&mut c, &self.store, &plan)?;
            c.commit()?;
            Ok(removed)
        })();
        for job in &dropped {
            self.states.remove(&job.key());
        }
        let removed_rows = collected.unwrap_or_else(|e| {
            warn!(error = %e, "dropping rows of unplanned jobs failed");
            0
        });
        info!(
            jobs = plan.jobs.len(),
            tracked = plan.tracked.len(),
            from_assignments = plan.evidence.assigned,
            from_events = plan.evidence.events,
            from_timelogs = plan.evidence.timelogs,
            added,
            dropped = dropped.len(),
            removed_rows,
            "sync plan updated"
        );
        self.plan = plan;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::DormancyReason;
    use crate::gitlab::{GitlabApi, Issuable, Listing};
    use crate::testing::{FakeErr, FakeGitlab, event_json, eventually, issue_json};
    use crate::write::WriteOp;

    /// What an empty store plans before any evidence arrives.
    const BASE: [Job; 7] = [
        Job::AssignedIssues,
        Job::AssignedMergeRequests,
        Job::RecentTimelogs,
        Job::AllTimelogs,
        Job::Events,
        Job::MemberProjects,
        Job::MemberGroups,
    ];

    struct Env {
        sync: Arc<SyncHandle>,
        session: SessionSlot,
        reconnect: Arc<Notify>,
        store: Arc<SyncStore>,
        _dir: Option<tempfile::TempDir>,
    }

    fn open_store() -> (Arc<SyncStore>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = fjall::Database::builder(dir.path().join("db"))
            .open()
            .unwrap();
        (Arc::new(SyncStore::open(&db).unwrap()), dir)
    }

    fn connected(fake: &Arc<FakeGitlab>, user_id: i64) -> ConnState {
        ConnState::Connected(Session {
            gitlab: Arc::clone(fake) as Arc<dyn GitlabApi>,
            host: "gitlab.test".into(),
            user_id,
        })
    }

    /// No gap between jobs and no startup spread, so tests don't wait.
    fn instant_config() -> SharedConfig {
        let mut cfg = crate::config::defaults();
        cfg.sync.job_gap_ms = 0;
        cfg.sync.startup_spread_secs = 0;
        Arc::new(std::sync::RwLock::new(cfg))
    }

    fn start_on(store: Arc<SyncStore>, state: ConnState) -> Env {
        let session: SessionSlot = Arc::new(tokio::sync::RwLock::new(state));
        let reconnect = Arc::new(Notify::new());
        let sync = SyncHandle::spawn(
            Arc::clone(&store),
            Arc::clone(&session),
            instant_config(),
            Arc::clone(&reconnect),
        );
        Env {
            sync,
            session,
            reconnect,
            store,
            _dir: None,
        }
    }

    fn start(state: ConnState) -> Env {
        let (store, dir) = open_store();
        Env {
            _dir: Some(dir),
            ..start_on(store, state)
        }
    }

    fn state(env: &Env, job: Job) -> JobState {
        env.store.job_state(&job.key()).unwrap()
    }

    #[tokio::test]
    async fn a_projects_first_run_is_full_and_later_ones_delta() {
        let fake = Arc::new(FakeGitlab::default());
        fake.serve("events", vec![event_json(1, 7, "pushed to", now_secs())]);
        fake.serve("projects/7/issues", vec![issue_json(7, 1, "one")]);
        let env = start(connected(&fake, 1));

        env.sync.refresh_now(&[Job::Events]).await;
        env.sync.refresh_now(&[Job::ProjectIssues(7)]).await;
        env.sync.refresh_now(&[Job::ProjectIssues(7)]).await;

        let calls = fake.calls_to("projects/7/issues");
        assert!(calls.len() >= 2, "{calls:?}");
        assert!(matches!(
            calls[0],
            Listing::ProjectIssues {
                updated_after: None,
                ..
            }
        ));
        let Listing::ProjectIssues {
            updated_after: Some(cursor),
            ..
        } = calls[calls.len() - 1]
        else {
            panic!("the repeat run must be a delta: {calls:?}");
        };
        assert!(cursor.timestamp() as u64 <= state(&env, Job::ProjectIssues(7)).last_full);
        assert!(env.store.issues.get((7, 1)).unwrap().is_some());
    }

    /// Job states persist, so a restart inside every interval costs GitLab
    /// nothing.
    #[tokio::test]
    async fn a_restart_inside_the_intervals_makes_no_calls() {
        let (store, _dir) = open_store();
        let fake = Arc::new(FakeGitlab::default());
        let first = start_on(Arc::clone(&store), connected(&fake, 1));
        first.sync.refresh_now(&BASE).await;
        drop(first);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let fresh = Arc::new(FakeGitlab::default());
        let _second = start_on(store, connected(&fresh, 1));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(fresh.read_calls(), 0, "{:?}", fresh.calls());
    }

    #[tokio::test]
    async fn a_network_error_demotes_the_session_and_keeps_the_job_due() {
        let fake = Arc::new(FakeGitlab::default());
        fake.fail_next("issues", FakeErr::Transient);
        let env = start(connected(&fake, 1));

        tokio::time::timeout(Duration::from_secs(2), env.reconnect.notified())
            .await
            .expect("the reconnect supervisor is woken");
        assert!(matches!(
            &*env.session.read().await,
            ConnState::Dormant(DormancyReason::Unreachable { .. })
        ));
        assert_eq!(state(&env, Job::AssignedIssues), JobState::default());
    }

    #[tokio::test]
    async fn a_rate_limit_pauses_everything_without_demoting() {
        let fake = Arc::new(FakeGitlab::default());
        fake.fail_next("issues", FakeErr::Throttled(429));
        let env = start(connected(&fake, 1));

        eventually("the rate-limited call", || {
            !fake.calls_to("issues").is_empty()
        })
        .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(matches!(
            &*env.session.read().await,
            ConnState::Connected(_)
        ));
        assert_eq!(fake.read_calls(), 1, "paused: {:?}", fake.calls());
    }

    #[tokio::test]
    async fn a_rejecting_project_backs_off_without_blocking_others() {
        let fake = Arc::new(FakeGitlab::default());
        let now = now_secs();
        fake.serve(
            "events",
            vec![
                event_json(1, 7, "opened", now),
                event_json(2, 8, "opened", now),
            ],
        );
        fake.fail_next("projects/7/issues", FakeErr::Rejected);
        let env = start(connected(&fake, 1));

        eventually("both projects' issue jobs", || {
            state(&env, Job::ProjectIssues(7)).failures == 1
                && state(&env, Job::ProjectIssues(8)).last_ok > 0
        })
        .await;
        let backed_off = state(&env, Job::ProjectIssues(7));
        assert_eq!(backed_off.last_ok, 0);
        assert!(backed_off.retry_at > now, "{backed_off:?}");
        assert!(matches!(
            &*env.session.read().await,
            ConnState::Connected(_)
        ));
    }

    #[tokio::test]
    async fn a_clear_cancels_the_fetch_in_flight() {
        let fake = Arc::new(FakeGitlab::default());
        fake.serve("events", vec![event_json(1, 7, "opened", now_secs())]);
        fake.serve("projects/7/issues", vec![issue_json(7, 1, "late")]);
        let gate = fake.gate("projects/7/issues");
        let env = start(connected(&fake, 1));

        tokio::time::timeout(Duration::from_secs(2), fake.gated.notified())
            .await
            .expect("the project fetch starts");
        // Nothing re-tracks project 7 after the wipe.
        fake.serve("events", Vec::new());
        tokio::time::timeout(Duration::from_secs(2), env.sync.clear(Clear::Everything))
            .await
            .expect("the clear doesn't wait for the fetch");
        gate.notify_one();
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert!(env.store.issues.get((7, 1)).unwrap().is_none());
        assert!(env.store.events.scan(RowScope::All).unwrap().is_empty());
    }

    #[tokio::test]
    async fn another_account_wipes_the_previous_ones_data() {
        let (store, _dir) = open_store();
        let mut c = store.begin();
        c.set_identity(&Identity {
            host: "gitlab.test".into(),
            user_id: 99,
        })
        .unwrap();
        c.upsert(&[crate::sync::model::Issue {
            id: 1,
            iid: 1,
            project_id: 1,
            ..Default::default()
        }])
        .unwrap();
        c.commit().unwrap();

        let fake = Arc::new(FakeGitlab::default());
        let env = start_on(Arc::clone(&store), connected(&fake, 1));
        env.sync.refresh_now(&[Job::AssignedIssues]).await;
        assert_eq!(store.identity().unwrap().unwrap().user_id, 1);
        assert!(store.issues.get((1, 1)).unwrap().is_none());
    }

    #[tokio::test]
    async fn commands_are_served_while_dormant() {
        let env = start(ConnState::Dormant(DormancyReason::NoCredentials));
        tokio::time::timeout(Duration::from_secs(1), async {
            env.sync.clear(Clear::Everything).await;
            env.sync.refresh_now(&[Job::AssignedIssues]).await;
        })
        .await
        .expect("no command blocks on a missing session");
        assert!(!env.sync.has_synced(Job::AssignedIssues));
    }

    #[tokio::test]
    async fn noted_writes_are_visible_to_views_fetched_before_them() {
        let env = start(ConnState::Dormant(DormancyReason::NoCredentials));
        let close = Write {
            kind: Issuable::Issue,
            project_id: 7,
            iid: 1,
            op: WriteOp::Close,
        };
        let before = now_secs();
        env.sync.note_write(&close);
        assert_eq!(env.sync.writes_since(before), [close]);
        assert!(env.sync.writes_since(before + 5).is_empty());
    }

    #[test]
    fn clears_reset_the_jobs_whose_data_they_drop() {
        let keys = BASE
            .iter()
            .chain(&[Job::ProjectIssues(7), Job::ProjectBoards(7), Job::AllIssues])
            .map(Job::key)
            .collect::<Vec<_>>();
        let reset = |what: Clear| {
            keys.iter()
                .filter(|k| what.resets(k))
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(
            reset(Clear::Assigned),
            [
                "assigned/issues",
                "assigned/merge_requests",
                "project/7/boards"
            ]
        );
        assert_eq!(
            reset(Clear::Corpus),
            [
                "assigned/issues",
                "assigned/merge_requests",
                "member/projects",
                "member/groups",
                "project/7/issues",
                "all/issues"
            ]
        );
        assert_eq!(
            reset(Clear::Timelogs { from: 0, until: 1 }),
            ["timelogs/recent", "timelogs/all"]
        );
    }
}
