//! The sync worker: the only task that reads from GitLab and the only writer
//! of the [`SyncStore`].
//!
//! Up to `sync.max_in_flight` fetches run at once as spawned tasks, one per
//! [`Lane`] (so one per project); the worker alone commits what they return,
//! in the order they finish. A free slot goes to a demanded job first (by
//! priority), else to the planned job that is due, lowest priority class
//! first. Per-job jittered due times plus a jittered pause between launches
//! keep requests spread out. A 429 stops the launches for its pause; a 5xx
//! or a rejection backs off only its job; a network error backs off its job
//! and demotes the session, parking the worker until the reconnect
//! supervisor wakes it; a 401 parks the session until `forskap auth login`.
//! Fetches still in flight then finish on their own: what they fetched
//! lands, and a failure of the session already given up counts for nothing.
//!
//! A per-project or per-group listing GitLab refuses (403/404) three times
//! in a row is unavailable (an epics listing at its first rejection: the
//! instance has no epics): it rests a day at a time and fails quietly, until
//! a run succeeds, a login clears every backoff or a cache clear resets it.
//!
//! One row comes from outside the fetches: an issue the handlers just
//! created ([`SyncHandle::land_issue`]). The worker stores it like a fetched
//! one and voids the issue lists still in flight, which never saw it.
//!
//! The avatar files are the worker's too: written with their row (unless
//! the file already holds the image), removed once a commit replaced their
//! row, or by a sweep once a commit dropped rows.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::task::{AbortHandle, JoinError, JoinSet};
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

use super::avatars::{Avatar, AvatarDir};
use super::jobs::{
    self, ASSIGNED_ISSUES, ASSIGNED_MERGE_REQUESTS, FetchCtx, Job, Lane, RECENT_ASSIGNED_ISSUES,
    RECENT_AUTHORED_ISSUES, Staged, Windows,
};
use super::model::{
    Board, DescriptionTemplate, Epic, Group, Issue, MergeRequest, Project, Resource, RowKey,
    Timelog,
};
use super::now_secs;
use super::planner::{self, Plan};
use super::schedule::{
    self, Cadence, JobState, RATE_LIMIT_PAUSE_CAP, REJECTED_BACKOFF_CAP, SERVER_BACKOFF_CAP,
    UNAVAILABLE_REST_SECS,
};
use super::store::{Identity, NotedWrite, RowScope, SyncStore};
use crate::config::{Config, SharedConfig};
use crate::error::{Error, Result};
use crate::gitlab::{GitlabApi, Progress};
use crate::handlers::{ConnState, Session, SessionSlot};
use crate::reconnect::{KeychainProbe, Reconnect};
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
    /// The assigned issue/MR views, the recent issue views, the board
    /// labels and the description templates.
    Assigned,
    /// Issues, MRs, epics, projects, groups and the project avatars.
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
                key == ASSIGNED_ISSUES
                    || key == ASSIGNED_MERGE_REQUESTS
                    || key.starts_with("recent/")
                    || key.ends_with("/boards")
                    || key.ends_with("_templates")
            }
            Self::Corpus => {
                key.starts_with("member/")
                    || key.ends_with("/issues")
                    || key.ends_with("/merge_requests")
                    || key.ends_with("/epics")
                    || key.ends_with("/avatar")
            }
            Self::Timelogs { .. } => key.starts_with("timelogs/"),
        }
    }

    /// The jobs refilling this slice that a `ClearCache` waits for; the
    /// rest refill in the background.
    pub fn refill(self) -> &'static [Job] {
        match self {
            Self::Everything => &[
                Job::AssignedIssues,
                Job::AssignedMergeRequests,
                Job::RecentTimelogs,
                Job::AllTimelogs,
            ],
            Self::Assigned | Self::Corpus => &[Job::AssignedIssues, Job::AssignedMergeRequests],
            Self::Timelogs { .. } => &[Job::RecentTimelogs, Job::AllTimelogs],
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
    /// A new login: retry the jobs that were backed off.
    LoggedIn,
    /// Persist a write GitLab applied (see [`SyncHandle::note_write`]).
    Note(NotedWrite),
    /// Store an issue GitLab just created for the account (see
    /// [`SyncHandle::land_issue`]); the sender is answered once that is done
    /// or given up. Boxed: an issue row dwarfs every other command.
    Land(Box<Issue>, Identity, oneshot::Sender<()>),
    /// Report the planned jobs (see [`SyncHandle::jobs`]).
    Snapshot(oneshot::Sender<Snapshot>),
}

/// What keeps a job from starting though its turn has come, or in the case
/// of [`Hold::Startup`] though its interval has passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hold {
    /// No session: nothing runs until the daemon has one.
    Session,
    /// A 429 pauses every launch (see [`Snapshot::paused_until`]).
    RateLimit,
    /// The fetch with this key holds the job's lane.
    Lane(String),
    /// `sync.max_in_flight` fetches are in flight.
    Slots,
    /// A background job waiting out its startup offset.
    Startup,
}

/// Where a planned job stands with the worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    /// Its fetch is in flight.
    Running,
    /// Requested ahead of the schedule; gets a free slot before anything
    /// merely due.
    Demanded,
    /// Its due time passed; it runs once the worker gets to it.
    Due,
    /// Not due yet; for an unavailable job, resting until its next attempt.
    Waiting,
    /// Failed, and held back until its retry time.
    BackingOff,
}

/// One planned job, as the worker sees it right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobInfo {
    pub key: String,
    pub status: JobStatus,
    /// Start of the last successful run; 0 means never.
    pub last_ok: u64,
    /// When the schedule runs it next, the retry time while it backs off.
    /// `None` while it runs or is demanded, for a job that never ran and
    /// for one that is never due again (a fetched avatar).
    pub next_due: Option<u64>,
    /// When the running fetch started.
    pub running_since: Option<u64>,
    /// Whether the running fetch is a full run rather than a delta; `None`
    /// for a job whose every run is full.
    pub full: Option<bool>,
    /// Rows the running fetch has so far.
    pub fetched: Option<u64>,
    /// Rows GitLab announced for it, if it did.
    pub expected: Option<u64>,
    /// Consecutive failures.
    pub failures: u32,
    /// Why the last run failed, until a run succeeds. Stored with the state
    /// of a job that backs off or rests, so it outlives a restart; a failure
    /// that holds the job back no further (a rate limit) is in memory only.
    pub last_error: Option<String>,
    /// GitLab refused it for good (see [`Job::unavailable`]): it rests a day
    /// at a time, and its status is no `BackingOff`.
    pub unavailable: bool,
    /// What holds a demanded or due job back; `None` when it is simply next
    /// (only the gap between launches or the order is in its way). A waiting
    /// job has [`Hold::Startup`] or none.
    pub hold: Option<Hold>,
}

/// The worker's jobs at one moment, in the order it would run them.
/// Several can be running.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub jobs: Vec<JobInfo>,
    /// While set and in the future, a 429 holds every launch back.
    pub paused_until: Option<u64>,
    /// Whether the worker had a session when it answered.
    pub connected: bool,
}

/// The handlers' side of the sync layer: read access to the store plus
/// commands for the worker.
pub struct SyncHandle {
    store: Arc<SyncStore>,
    avatars: AvatarDir,
    tx: mpsc::UnboundedSender<Command>,
    noted: Mutex<Vec<NotedWrite>>,
}

impl SyncHandle {
    /// Start the worker. It stops once the returned handle is dropped.
    pub fn spawn(
        store: Arc<SyncStore>,
        avatars: AvatarDir,
        session: SessionSlot,
        config: SharedConfig,
        reconnect_signal: Arc<Reconnect>,
        keychain_probe: KeychainProbe,
    ) -> Arc<Self> {
        Self::start(
            store,
            avatars,
            session,
            config,
            reconnect_signal,
            keychain_probe,
            true,
        )
    }

    /// A worker that runs only demanded jobs, so a test decides exactly
    /// when GitLab is read.
    #[cfg(test)]
    pub(crate) fn spawn_on_demand(
        store: Arc<SyncStore>,
        avatars: AvatarDir,
        session: SessionSlot,
        config: SharedConfig,
        reconnect_signal: Arc<Reconnect>,
    ) -> Arc<Self> {
        let probe = crate::reconnect::no_keychain_probe();
        Self::start(
            store,
            avatars,
            session,
            config,
            reconnect_signal,
            probe,
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn start(
        store: Arc<SyncStore>,
        avatars: AvatarDir,
        session: SessionSlot,
        config: SharedConfig,
        reconnect_signal: Arc<Reconnect>,
        keychain_probe: KeychainProbe,
        scheduled: bool,
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
            avatars: avatars.clone(),
            session,
            config,
            reconnect_signal,
            keychain_probe,
            rx,
            plan: Plan::default(),
            identity: None,
            boot: now_secs(),
            demand: BTreeMap::new(),
            paused_until: 0,
            rate_limits: 0,
            runs: 0,
            paused_at_run: 0,
            flights: BTreeMap::new(),
            fetches: JoinSet::new(),
            launched: None,
            lost: None,
            errors: store
                .job_errors()
                .unwrap_or_else(|e| {
                    warn!(error = %e, "reading the sync jobs' last errors failed");
                    Vec::new()
                })
                .into_iter()
                .collect(),
            landings: Vec::new(),
            asked: Vec::new(),
            replan: true,
            incomplete: BTreeSet::new(),
            relisted: BTreeSet::new(),
            scheduled,
        };
        let expired = now_secs().saturating_sub(NOTED_WRITE_TTL_SECS);
        let noted = store
            .noted
            .scan(RowScope::Since(expired))
            .unwrap_or_else(|e| {
                warn!(error = %e, "reading noted writes failed");
                Vec::new()
            });
        tokio::spawn(worker.run());
        Arc::new(Self {
            store,
            avatars,
            tx,
            noted: Mutex::new(noted),
        })
    }

    pub fn store(&self) -> &SyncStore {
        &self.store
    }

    /// Where the files the avatar rows name live.
    pub fn avatars(&self) -> &AvatarDir {
        &self.avatars
    }

    /// The session may have changed (login, reconnect).
    pub fn wake(&self) {
        let _ = self.tx.send(Command::Wake);
    }

    /// A new login: whatever backed jobs off may be fixed, so they run now.
    pub fn logged_in(&self) {
        let _ = self.tx.send(Command::LoggedIn);
    }

    /// The config was reloaded.
    pub fn reconfigure(&self) {
        let _ = self.tx.send(Command::Reconfigure);
    }

    /// Run `jobs` ahead of the schedule. The requests are queued before this
    /// returns, in order with other commands; the future resolves once each
    /// job ran or can't run (unplanned, dormant). The assigned issues also
    /// wait for board columns they show that never synced. Callers bound the
    /// wait with a timeout: a job can queue behind a long one.
    pub fn refresh_now(&self, jobs: &[Job]) -> impl Future<Output = ()> + Send + 'static {
        let waits: Vec<_> = jobs
            .iter()
            .map(|&job| {
                let (done, wait) = oneshot::channel();
                let _ = self.tx.send(Command::Run(job, Some(done)));
                wait
            })
            .collect();
        async move {
            for wait in waits {
                let _ = wait.await;
            }
        }
    }

    /// Run `jobs` ahead of the schedule, without waiting.
    pub fn refresh_soon(&self, jobs: &[Job]) {
        for &job in jobs {
            let _ = self.tx.send(Command::Run(job, None));
        }
    }

    /// Drop a slice of synced state; every in-flight fetch into it is
    /// cancelled and the affected jobs become due at once. Queued in order
    /// like [`Self::refresh_now`], so a refresh requested right after it runs
    /// on the cleared store without a scheduled run slipping in between.
    pub fn clear(&self, what: Clear) -> impl Future<Output = ()> + Send + 'static {
        let (done, wait) = oneshot::channel();
        let _ = self.tx.send(Command::Clear(what, done));
        async move {
            let _ = wait.await;
        }
    }

    /// The planned jobs as the worker sees them now, in the order it would
    /// run them. Answered while jobs are in flight and while the session is
    /// dormant, on the worker's next turn: with the session it then finds
    /// and after it started what it could. Empty once the worker is gone.
    pub fn jobs(&self) -> impl Future<Output = Snapshot> + Send + 'static {
        let (reply, wait) = oneshot::channel();
        let _ = self.tx.send(Command::Snapshot(reply));
        async move { wait.await.unwrap_or_default() }
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

    /// Whether a run of `job` that started at `since` or later completed.
    pub fn synced_since(&self, job: Job, since: u64) -> bool {
        match self.store.job_state(&job.key()) {
            Ok(s) => s.last_ok >= since.max(1),
            Err(e) => {
                warn!(error = %e, "sync job state read failed; treating as not synced");
                false
            }
        }
    }

    /// Remember that `write` just reached GitLab, so views fetched before it
    /// can be corrected at read time (see [`Self::writes_since`]).
    pub fn note_write(&self, write: &Write) {
        let now = now_secs();
        let note = NotedWrite {
            write: write.clone(),
            at: now,
        };
        let mut noted = self.noted.lock().unwrap();
        noted.retain(|w| now.saturating_sub(w.at) < NOTED_WRITE_TTL_SECS);
        noted.push(note.clone());
        let _ = self.tx.send(Command::Note(note));
    }

    /// Store `issue`, which GitLab just created for the account `by`, so the
    /// reads show it before any list fetched it: its row, and its key at the
    /// head of the views it belongs to, namely the issues the user authored
    /// and, if GitLab assigned it to them, both assigned ones. A view that
    /// never synced is left alone. The list fetches in flight are void and
    /// run again: they started before the issue existed, and landing would
    /// take it out of the views again. Nothing is stored once the store
    /// holds another account's data. Resolves when the worker is done with
    /// it; callers bound the wait.
    pub fn land_issue(
        &self,
        issue: Issue,
        by: Identity,
    ) -> impl Future<Output = ()> + Send + 'static {
        let (done, wait) = oneshot::channel();
        let _ = self.tx.send(Command::Land(Box::new(issue), by, done));
        async move {
            let _ = wait.await;
        }
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
    /// Nothing can start before this time, unless a fetch frees its lane.
    Idle(u64),
}

/// Whoever waits on a demanded job. Shared where one request waits for
/// several jobs: the wait ends once the last of them let go.
type Waiter = Arc<oneshot::Sender<()>>;

/// A fetch in flight, with what committing its result takes.
struct Flight {
    job: Job,
    key: String,
    /// The job's state when the fetch started.
    state: JobState,
    started: u64,
    full: bool,
    fingerprint: u64,
    /// `None` for a run nobody demanded.
    waiters: Option<Vec<Waiter>>,
    /// The session it reads with.
    session: Session,
    abort: AbortHandle,
    /// Written by the fetch, read for the snapshot.
    progress: Arc<Progress>,
}

struct Worker {
    store: Arc<SyncStore>,
    avatars: AvatarDir,
    session: SessionSlot,
    config: SharedConfig,
    reconnect_signal: Arc<Reconnect>,
    /// Asked on a 401, before the session is parked.
    keychain_probe: KeychainProbe,
    rx: mpsc::UnboundedReceiver<Command>,
    plan: Plan,
    /// Mirror of the persisted job states; the worker is their only writer.
    states: HashMap<String, JobState>,
    /// Identity already checked against the store this run.
    identity: Option<Identity>,
    boot: u64,
    /// Demanded jobs and whoever waits on them.
    demand: BTreeMap<Job, Vec<Waiter>>,
    paused_until: u64,
    rate_limits: u32,
    /// Fetches started so far; the latest one's number.
    runs: u64,
    /// `runs` when the pause was set: fetches up to it ran into the same
    /// rate limit.
    paused_at_run: u64,
    /// The fetches in flight, by their number.
    flights: BTreeMap<u64, Flight>,
    /// Their tasks; each returns its number with the result.
    fetches: JoinSet<(u64, Result<Staged>)>,
    /// When the last fetch started; the next one keeps the gap to it.
    launched: Option<Instant>,
    /// The client whose session a failed fetch already gave up, so its
    /// siblings' failures don't do it again.
    lost: Option<Arc<dyn GitlabApi>>,
    /// Why each job's last run failed, by state key. Stored beside the
    /// [`JobState`] of a job held back for it (see [`Self::rest`]), not in
    /// it: that stays `Copy`.
    errors: HashMap<String, String>,
    /// Created issues waiting to be stored: [`Self::handle`] can run before
    /// the session's account was checked against the store.
    landings: Vec<(Issue, Identity, oneshot::Sender<()>)>,
    /// Whoever asked for the jobs, answered once this turn read the session
    /// and started what it could: only then is it known what holds the rest.
    asked: Vec<oneshot::Sender<Snapshot>>,
    replan: bool,
    /// Plan-feeding jobs a clear reset that haven't synced since. Until they
    /// have, their evidence is missing, so a replan only adds jobs.
    incomplete: BTreeSet<Job>,
    /// Unlisted member projects the member listing already reran for.
    relisted: BTreeSet<i64>,
    /// Whether due jobs run on their own, not only when demanded.
    scheduled: bool,
}

impl Worker {
    async fn run(mut self) {
        self.restore_avatars();
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
            if let Some(session) = &session
                && let Err(e) = self.check_identity(session)
            {
                warn!(error = %e, "sync identity check failed");
            }
            // Only now is it known whose data the store holds.
            for (issue, by, done) in std::mem::take(&mut self.landings) {
                self.land(issue, &by);
                let _ = done.send(());
            }
            let connected = session.is_some();
            let idle = match session {
                Some(session) => {
                    if self.replan {
                        self.replan_now();
                    }
                    self.launch(&session)
                }
                None => {
                    self.demand.clear();
                    DORMANT_RECHECK
                }
            };
            if !self.asked.is_empty() {
                let snapshot = self.snapshot(now_secs(), connected);
                for reply in self.asked.drain(..) {
                    let _ = reply.send(snapshot.clone());
                }
            }
            tokio::select! {
                cmd = self.rx.recv() => match cmd {
                    Some(cmd) => self.handle(cmd),
                    None => break,
                },
                Some(joined) = self.fetches.join_next(), if !self.fetches.is_empty() => {
                    self.settle(joined).await;
                }
                () = tokio::time::sleep(idle) => {}
            }
        }
        // Dropping the worker aborts the fetches still in flight.
        debug!("sync worker stopped");
    }

    /// Start every job that may run now; returns how long nothing else can
    /// start unless a command or a finished fetch changes that.
    fn launch(&mut self, session: &Session) -> Duration {
        // Connected with it again: its failures count once more.
        if self.lost(session) {
            self.lost = None;
        }
        loop {
            let now = now_secs();
            if self.paused_until > now {
                return Duration::from_secs(self.paused_until - now);
            }
            let max = self.config.read().unwrap().sync.max_in_flight();
            if self.flights.len() >= max {
                return Duration::from_secs(IDLE_RECHECK_SECS);
            }
            let job = match self.next_job(now) {
                Next::Run(job) => job,
                Next::Idle(until) => {
                    let secs = until.saturating_sub(now).clamp(1, IDLE_RECHECK_SECS);
                    return Duration::from_secs(secs);
                }
            };
            let next = self.launched.map(|at| at + self.gap());
            let gap = next.map(|at| at.saturating_duration_since(Instant::now()));
            if let Some(gap) = gap.filter(|gap| !gap.is_zero()) {
                return gap;
            }
            self.start(job, session);
            self.launched = Some(Instant::now());
        }
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

    fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::Run(job, done) => {
                if self.plan.jobs.contains(&job) {
                    let waiter = done.map(Arc::new);
                    self.demand.entry(job).or_default().extend(waiter);
                }
            }
            Command::Clear(what, done) => {
                self.apply_clear(what);
                let _ = done.send(());
            }
            Command::Reconfigure => self.replan = true,
            Command::Wake => {}
            Command::LoggedIn => self.unpark(),
            Command::Note(note) => self.persist_note(note),
            Command::Land(issue, by, done) => self.landings.push((*issue, by, done)),
            Command::Snapshot(reply) => self.asked.push(reply),
        }
    }

    /// When `job` is due by its schedule alone, with the state that says so.
    fn scheduled(&self, job: Job, cfg: &Config) -> (u64, JobState, Cadence) {
        let key = job.key();
        let state = self.states.get(&key).copied().unwrap_or_default();
        let cadence = job.cadence(cfg);
        let fingerprint = self.plan.fingerprint(job, cfg);
        let at = schedule::due_at(&key, &state, cadence, fingerprint, cfg.sync.jitter);
        (at, state, cadence)
    }

    /// Until when `job` waits out its startup offset: background jobs are
    /// spread out after the daemon's start. 0 for the others.
    fn spread_until(&self, job: Job, cfg: &Config) -> u64 {
        if job.priority() == 0 {
            return 0;
        }
        self.boot + schedule::startup_offset(&job.key(), cfg.sync.startup_spread_secs)
    }

    /// When `job` is due: by its schedule, and for a background job not
    /// before its startup offset passed.
    fn due(&self, job: Job, cfg: &Config) -> (u64, JobState, Cadence) {
        let (at, state, cadence) = self.scheduled(job, cfg);
        (at.max(self.spread_until(job, cfg)), state, cadence)
    }

    /// What keeps `job` from starting now that its turn has come: the
    /// reasons [`Self::run`] and [`Self::launch`] turn it down for, in their
    /// order. `None` when only the gap between launches or the jobs ahead of
    /// it are in its way.
    fn hold(&self, job: Job, now: u64, connected: bool, max_in_flight: usize) -> Option<Hold> {
        if !connected {
            return Some(Hold::Session);
        }
        if self.paused_until > now {
            return Some(Hold::RateLimit);
        }
        let lane = job.lane();
        if let Some(ahead) = self.flights.values().find(|f| f.job.lane() == lane) {
            return Some(Hold::Lane(ahead.key.clone()));
        }
        (self.flights.len() >= max_in_flight).then_some(Hold::Slots)
    }

    /// The class a due job competes in (see [`Self::next_job`]).
    fn class(job: Job, state: &JobState, cadence: Cadence, overdue: u64) -> u8 {
        // Until the full history first synced, `forskap time history` shows
        // just the recent window: it ranks with the events till then.
        if job == Job::AllTimelogs && state.last_ok == 0 {
            1
        } else {
            schedule::aged(job.priority(), overdue, cadence.every)
        }
    }

    /// Every planned job as of `now`, in the order [`Self::next_job`] would
    /// pick them: the running ones (longest first), the demanded ones, the
    /// due ones, then the rest by due time. `connected` is whether this turn
    /// found a session.
    fn snapshot(&self, now: u64, connected: bool) -> Snapshot {
        let cfg = self.config.read().unwrap();
        let max_in_flight = cfg.sync.max_in_flight();
        let mut jobs: Vec<((u8, u8, u64, Job), JobInfo)> = self
            .plan
            .jobs
            .iter()
            .map(|&job| {
                let (by_schedule, state, cadence) = self.scheduled(job, &cfg);
                let at = by_schedule.max(self.spread_until(job, &cfg));
                let flight = self.flights.values().find(|f| f.job == job);
                let running_since = flight.map(|f| f.started);
                // Only where a delta exists does "full" say anything.
                let delta = cadence.full_every.is_some();
                let unavailable = job.unavailable(&state);
                let (status, order) = if let Some(since) = running_since {
                    (JobStatus::Running, (0, 0, since))
                } else if self.demand.contains_key(&job) {
                    (JobStatus::Demanded, (1, job.priority(), 0))
                } else if at <= now {
                    let class = Self::class(job, &state, cadence, now - at);
                    (JobStatus::Due, (2, class, at))
                } else if state.retry_at > now && !unavailable {
                    (JobStatus::BackingOff, (3, 0, at))
                } else {
                    // An unavailable job rests rather than backs off: it is
                    // no failure waiting to heal.
                    (JobStatus::Waiting, (3, 0, at))
                };
                let scheduled = matches!(
                    status,
                    JobStatus::Due | JobStatus::Waiting | JobStatus::BackingOff
                );
                let hold = match status {
                    JobStatus::Demanded | JobStatus::Due => {
                        self.hold(job, now, connected, max_in_flight)
                    }
                    // Its interval has passed; only the spread keeps it.
                    JobStatus::Waiting if by_schedule <= now => Some(Hold::Startup),
                    _ => None,
                };
                let key = job.key();
                let info = JobInfo {
                    status,
                    last_ok: state.last_ok,
                    next_due: Some(at).filter(|&at| scheduled && at > 0 && at < u64::MAX),
                    running_since,
                    full: flight.filter(|_| delta).map(|f| f.full),
                    fetched: flight.map(|f| f.progress.fetched()),
                    expected: flight.and_then(|f| f.progress.expected()),
                    failures: state.failures,
                    last_error: self.errors.get(&key).cloned(),
                    unavailable,
                    hold,
                    key,
                };
                ((order.0, order.1, order.2, job), info)
            })
            .collect();
        jobs.sort_by_key(|(order, _)| *order);
        Snapshot {
            jobs: jobs.into_iter().map(|(_, info)| info).collect(),
            paused_until: Some(self.paused_until).filter(|&until| until > now),
            connected,
        }
    }

    fn persist_note(&mut self, note: NotedWrite) {
        let expired = note.at.saturating_sub(NOTED_WRITE_TTL_SECS);
        let stored = (|| -> Result<()> {
            let mut c = self.store.begin();
            c.upsert(&[note])?;
            c.remove_where::<NotedWrite>(RowScope::Before(expired), |_| false)?;
            c.commit()
        })();
        if let Err(e) = stored {
            warn!(error = %e, "storing a noted write failed");
        }
    }

    /// Store an issue the account `by` just created (see
    /// [`SyncHandle::land_issue`]).
    fn land(&mut self, issue: Issue, by: &Identity) {
        if self.identity.as_ref() != Some(by) {
            debug!("created issue not stored: the store holds another account's data");
            return;
        }
        if !issue.is_valid() {
            warn!("created issue not stored: GitLab's answer names no issue");
            return;
        }
        let key = issue.key();
        let (project_id, iid) = (issue.project_id, issue.iid);
        // What GitLab made of it, not what was asked for: it ignores the
        // assignee of a user who may not assign.
        let mut views = vec![RECENT_AUTHORED_ISSUES];
        if issue.assignees.iter().any(|a| a.id == by.user_id) {
            views.extend([ASSIGNED_ISSUES, RECENT_ASSIGNED_ISSUES]);
        }
        let landed = (|| -> Result<Vec<&'static str>> {
            let mut c = self.store.begin();
            // A list fetched since may have stored a newer version already.
            let stored = c.get::<Issue>(key)?;
            if stored.is_none_or(|s| s.updated_at <= issue.updated_at) {
                c.upsert(&[issue])?;
            }
            let mut listed = Vec::new();
            for name in views {
                // A view that never synced stays unsynced: one key is not
                // the list.
                let Some(mut view) = c.view(name)? else {
                    continue;
                };
                if !view.keys.contains(&key) {
                    // First: the lists come newest first. `fetched_at`
                    // stays, the writes since that fetch still apply.
                    view.keys.insert(0, key);
                    c.set_view(name, &view)?;
                    listed.push(name);
                }
            }
            c.commit()?;
            Ok(listed)
        })();
        let listed = match landed {
            Ok(listed) => listed,
            Err(e) => {
                warn!(error = %e, project_id, iid, "storing a created issue failed");
                return;
            }
        };
        debug!(project_id, iid, views = ?listed, "created issue stored");
        // The assigned view is evidence for the plan.
        if listed.contains(&ASSIGNED_ISSUES) {
            self.replan = true;
        }
        // An issue list fetched before the create lacks it: landing, it
        // would replace the views without the key, and the row could go
        // with it. No `updated_at` tells such a fetch from a newer one.
        let lists = self
            .flights
            .iter()
            .filter(|(_, f)| f.job.lane() == Lane::Issues);
        let void = lists.map(|(&run, _)| run).collect();
        for flight in self.cancel(void) {
            debug!(job = %flight.key, "sync job restarted: an issue was created under it");
            let waiters = flight.waiters.unwrap_or_default();
            self.demand.entry(flight.job).or_default().extend(waiters);
        }
    }

    /// Clear every job's backoff and refusal count, so all of them are due
    /// by their schedule again: a new token may see what the old one
    /// couldn't.
    fn unpark(&mut self) {
        self.errors.clear();
        let parked: Vec<(String, JobState)> = self
            .states
            .iter()
            .filter(|(_, s)| s.failures > 0 || s.retry_at > 0 || s.rejections > 0)
            .map(|(k, s)| {
                let state = JobState {
                    failures: 0,
                    retry_at: 0,
                    rejections: 0,
                    ..*s
                };
                (k.clone(), state)
            })
            .collect();
        if parked.is_empty() {
            return;
        }
        let persisted = (|| -> Result<()> {
            let mut c = self.store.begin();
            for (key, state) in &parked {
                c.set_job(key, state)?;
            }
            c.commit()
        })();
        if let Err(e) = persisted {
            warn!(error = %e, "storing cleared backoffs failed");
        }
        info!(
            jobs = parked.len(),
            "new login; retrying backed-off sync jobs"
        );
        self.states.extend(parked);
    }

    /// The job a free slot goes to: a demanded one before a due one, and
    /// none whose lane a fetch in flight holds.
    fn next_job(&self, now: u64) -> Next {
        let busy: HashSet<Lane> = self.flights.values().map(|f| f.job.lane()).collect();
        let free = |job: &Job| !busy.contains(&job.lane());
        let demanded = self.demand.keys().filter(|j| free(j));
        if let Some(&job) = demanded.min_by_key(|j| (j.priority(), **j)) {
            return Next::Run(job);
        }
        if !self.scheduled {
            return Next::Idle(u64::MAX);
        }
        let cfg = self.config.read().unwrap();
        let mut best: Option<(u8, u64, Job)> = None;
        let mut soonest = u64::MAX;
        for &job in self.plan.jobs.iter().filter(|j| free(j)) {
            let (at, state, cadence) = self.due(job, &cfg);
            if at <= now {
                let candidate = (Self::class(job, &state, cadence, now - at), at, job);
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

    /// Start `job`'s fetch. Spawned so a panic is a failed job, not a dead
    /// worker, and commands keep flowing while it runs.
    fn start(&mut self, job: Job, session: &Session) {
        let waiters = self.demand.remove(&job);
        let key = job.key();
        let state = self.states.get(&key).copied().unwrap_or_default();
        let started = now_secs();
        let (full, fingerprint, windows) = {
            let cfg = self.config.read().unwrap();
            let fingerprint = self.plan.fingerprint(job, &cfg);
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
        let run = self.runs;
        let progress = Arc::new(Progress::default());
        let ctx = FetchCtx {
            gitlab: Arc::clone(&session.gitlab),
            full,
            state,
            started,
            windows,
            avatars: self.avatars.clone(),
            progress: Arc::clone(&progress),
        };
        let abort = self
            .fetches
            .spawn(async move { (run, jobs::fetch(job, ctx).await) });
        let flight = Flight {
            job,
            key,
            state,
            started,
            full,
            fingerprint,
            waiters,
            session: session.clone(),
            abort,
            progress,
        };
        self.flights.insert(run, flight);
    }

    /// Abort the fetches numbered `runs`: nothing of them is committed.
    fn cancel(&mut self, runs: Vec<u64>) -> Vec<Flight> {
        let flights = runs.into_iter().filter_map(|r| self.flights.remove(&r));
        flights.inspect(|f| f.abort.abort()).collect()
    }

    /// Commit a finished fetch, or count its failure.
    async fn settle(&mut self, joined: std::result::Result<(u64, Result<Staged>), JoinError>) {
        let (run, outcome) = match joined {
            Ok((run, fetched)) => (run, Ok(fetched)),
            // A cancelled fetch left its flight behind already.
            Err(e) if e.is_cancelled() => return,
            Err(e) => {
                let run = self.flights.iter().find(|(_, f)| f.abort.id() == e.id());
                let Some((&run, _)) = run else { return };
                (run, Err(e))
            }
        };
        // Finished just as it was cancelled.
        let Some(flight) = self.flights.remove(&run) else {
            return;
        };
        let Flight {
            job,
            key,
            state,
            started,
            full,
            fingerprint,
            mut waiters,
            session,
            abort: _,
            progress: _,
        } = flight;
        match outcome {
            Ok(Ok(staged)) => {
                let before = job.view().map(|name| self.view_keys(name));
                let replaced = self.avatar_file(job);
                if self.commit(job, &key, state, staged, started, full, fingerprint) {
                    // A fetch from before the pause says nothing about the
                    // rate limit being over.
                    if run > self.paused_at_run {
                        self.rate_limits = 0;
                    }
                    self.after_commit(job, before, &mut waiters);
                    // Only now does no row name the previous file any more.
                    if let Some(old) =
                        replaced.filter(|old| self.avatar_file(job).as_ref() != Some(old))
                    {
                        self.avatars.remove(&old);
                    }
                }
            }
            Ok(Err(e)) => {
                self.errors.insert(key.clone(), e.to_string());
                self.on_error(job, &key, state, e, &session, run).await;
            }
            Err(e) => {
                error!(job = %key, error = %e, "sync job panicked");
                self.errors
                    .insert(key.clone(), format!("the job panicked: {e}"));
                self.back_off(&key, state, REJECTED_BACKOFF_CAP);
            }
        }
        // Released only now, so a waiter reads the committed rows.
        drop(waiters);
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
    ) -> bool {
        let fresh = JobState {
            last_ok: started,
            last_full: if full { started } else { state.last_full },
            failures: 0,
            retry_at: 0,
            fingerprint,
            rejections: 0,
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
                self.errors.remove(key);
                self.incomplete.remove(&job);
                if full {
                    info!(job = %key, rows, "synced (full)");
                } else {
                    debug!(job = %key, rows, "synced (delta)");
                }
                if job.feeds_plan() {
                    self.replan = true;
                }
                true
            }
            Err(e) => {
                warn!(job = %key, error = %e, "storing sync result failed");
                self.errors
                    .insert(key.to_string(), format!("storing the result failed: {e}"));
                // GitLab served it: whatever it refused before, it doesn't now.
                let served = JobState {
                    rejections: 0,
                    ..state
                };
                self.back_off(key, served, SERVER_BACKOFF_CAP);
                false
            }
        }
    }

    /// The keys the view `name` lists, empty on a read failure.
    fn view_keys(&self, name: &str) -> Vec<RowKey> {
        match self.store.view(name) {
            Ok(view) => view.unwrap_or_default().keys,
            Err(e) => {
                warn!(error = %e, view = name, "reading a view failed");
                Vec::new()
            }
        }
    }

    /// What a fresh `job` result sets off: the replan it may call for, for
    /// a view (listing `before` until now) dropping the rows that left it,
    /// and for the assigned issues fetching the board columns they show
    /// that never synced, which `waiters` then wait for too.
    fn after_commit(
        &mut self,
        job: Job,
        before: Option<Vec<RowKey>>,
        waiters: &mut Option<Vec<Waiter>>,
    ) {
        if self.replan {
            self.replan_now();
        }
        if let (Some(name), Some(before)) = (job.view(), before) {
            self.drop_unviewed(name, &before);
        }
        if job != Job::AssignedIssues {
            return;
        }
        let boards = self.missing_boards();
        if boards.is_empty() {
            return;
        }
        // The boards finish in any order, so each holds the waiters.
        let waiters = waiters.take().unwrap_or_default();
        for board in boards {
            match self.flights.values_mut().find(|f| f.job == board) {
                // Already being fetched: no second run for it.
                Some(flight) => flight
                    .waiters
                    .get_or_insert_default()
                    .extend(waiters.iter().cloned()),
                None => self
                    .demand
                    .entry(board)
                    .or_default()
                    .extend(waiters.iter().cloned()),
            }
        }
    }

    fn drop_unviewed(&mut self, name: &str, before: &[RowKey]) {
        let dropped = (|| -> Result<usize> {
            let mut c = self.store.begin();
            let n = planner::drop_unviewed(&mut c, &self.store, &self.plan, name, before)?;
            if n > 0 {
                c.commit()?;
            }
            Ok(n)
        })();
        match dropped {
            Ok(0) => {}
            Ok(rows) => debug!(view = name, rows, "dropped rows that left the view"),
            Err(e) => warn!(error = %e, view = name, "dropping rows that left the view failed"),
        }
    }

    /// The file `job`'s avatar row names, if it is an avatar job with one.
    fn avatar_file(&self, job: Job) -> Option<String> {
        let Job::ProjectAvatar(project) = job else {
            return None;
        };
        let row = self.store.avatars.get((project.max(0) as u64, 0));
        row.ok().flatten().map(|a| a.file).filter(|f| !f.is_empty())
    }

    /// Remove the avatar files no row names (any more).
    fn sweep_avatars(&self) {
        match self.store.avatars.scan(RowScope::All) {
            Ok(rows) => {
                let keep = rows.into_iter().map(|a| a.file).collect();
                let removed = self.avatars.sweep(&keep);
                if removed > 0 {
                    debug!(removed, "removed avatar files without a row");
                }
            }
            Err(e) => warn!(error = %e, "reading the avatars failed; keeping every file"),
        }
    }

    /// Line rows and files up after a restart: an avatar whose file is gone
    /// (a wiped cache directory) is fetched again, a file without a row (a
    /// crash before the commit) is removed.
    fn restore_avatars(&mut self) {
        let rows = self.store.avatars.scan(RowScope::All).unwrap_or_else(|e| {
            warn!(error = %e, "reading the avatars failed");
            Vec::new()
        });
        let lost: Vec<Avatar> = rows
            .into_iter()
            .filter(|a| !a.file.is_empty() && !self.avatars.exists(&a.file))
            .collect();
        if !lost.is_empty() {
            let keys: Vec<String> = lost
                .iter()
                .map(|a| Job::ProjectAvatar(a.project_id).key())
                .collect();
            let mut c = self.store.begin();
            for (avatar, key) in lost.iter().zip(&keys) {
                c.remove::<Avatar>(avatar.key());
                c.remove_job(key);
            }
            match c.commit() {
                Ok(()) => {
                    info!(
                        avatars = lost.len(),
                        "avatar files are gone; fetching them again"
                    );
                    for key in &keys {
                        self.states.remove(key);
                    }
                }
                Err(e) => warn!(error = %e, "dropping avatars without a file failed"),
            }
        }
        self.sweep_avatars();
    }

    /// Planned board jobs of the assigned issues' projects that never
    /// synced and aren't backed off, in job order.
    fn missing_boards(&self) -> Vec<Job> {
        let now = now_secs();
        let boards: BTreeSet<Job> = self
            .view_keys(ASSIGNED_ISSUES)
            .iter()
            .map(|k| Job::ProjectBoards(k.0 as i64))
            .filter(|job| self.plan.jobs.contains(job))
            .filter(|job| {
                let state = self.states.get(&job.key()).copied().unwrap_or_default();
                state.last_ok == 0 && state.retry_at <= now
            })
            .collect();
        boards.into_iter().collect()
    }

    /// Whether a failed fetch already cost `session` its place: its
    /// sibling's failure then is the same outage, not a new one.
    fn lost(&self, session: &Session) -> bool {
        let lost = self.lost.as_ref();
        lost.is_some_and(|client| Arc::ptr_eq(client, &session.gitlab))
    }

    async fn on_error(
        &mut self,
        job: Job,
        key: &str,
        state: JobState,
        e: Error,
        session: &Session,
        run: u64,
    ) {
        match &e {
            // A single-flight worker would not have run this job on the
            // dead session at all: no backoff, no second demotion.
            Error::Transient(_) | Error::Unauthorized(_) if self.lost(session) => {
                debug!(job = %key, error = %e, "sync fetch failed with its session");
            }
            Error::Transient(detail) => {
                warn!(job = %key, error = %e, "sync fetch failed; GitLab unreachable");
                // Backed off too: if only this job's requests fail, rerunning
                // it right after each reconnect would flap the session.
                self.back_off(key, state, SERVER_BACKOFF_CAP);
                crate::reconnect::commit_unreachable(
                    &self.session,
                    &self.reconnect_signal,
                    &session.gitlab,
                    detail.clone(),
                )
                .await;
                self.lost = Some(Arc::clone(&session.gitlab));
            }
            // The token, not the job: park the session until `forskap auth login`,
            // unless the keychain holds a token to reconnect with.
            Error::Unauthorized(detail) => {
                let reconnects = self.config.read().unwrap().reconnect.enabled;
                if reconnects && (self.keychain_probe)(session.clone()).await {
                    crate::reconnect::commit_token_replaced(
                        &self.session,
                        &self.reconnect_signal,
                        &session.gitlab,
                    )
                    .await;
                } else {
                    crate::reconnect::commit_token_rejected(
                        &self.session,
                        &self.reconnect_signal,
                        &session.gitlab,
                        detail.clone(),
                    )
                    .await;
                }
                self.lost = Some(Arc::clone(&session.gitlab));
            }
            Error::Throttled {
                status: 429,
                retry_after,
                ..
            } => {
                // Started before the pause: it ran into the limit that set
                // it, so the pause neither climbs nor starts over.
                let sibling = run <= self.paused_at_run;
                if !sibling {
                    self.rate_limits += 1;
                }
                let pause = retry_after
                    .map_or_else(
                        || schedule::backoff(self.rate_limits, RATE_LIMIT_PAUSE_CAP),
                        |d| d.as_secs(),
                    )
                    .clamp(1, RATE_LIMIT_PAUSE_CAP);
                let until = now_secs().saturating_add(pause);
                if sibling {
                    self.paused_until = self.paused_until.max(until);
                    debug!(job = %key, "GitLab rate limit hit by a fetch from before the pause");
                } else {
                    self.paused_until = until;
                    self.paused_at_run = self.runs;
                    warn!(job = %key, pause_secs = pause, "GitLab rate limit hit; pausing the sync");
                }
            }
            // Neither these nor the network errors above say anything about
            // whether GitLab serves the listing: a refusal count goes on
            // past them.
            Error::Throttled { .. } => {
                warn!(job = %key, error = %e, "GitLab failed the sync fetch; backing off");
                self.back_off(key, state, SERVER_BACKOFF_CAP);
            }
            _ => self.rejected(job, key, state, &e),
        }
    }

    /// GitLab answered the fetch and won't serve it. A job that can be
    /// unavailable counts the refusals in a row (403/404, see
    /// [`Job::refused_by`]); from [`Job::unavailable_after`] of them on it
    /// is unavailable: GitLab refuses it for good (a feature switched off
    /// in the project, epics without Premium), so it rests a day at a time
    /// and fails at debug level. Before that, and for every other
    /// rejection, it backs off and warns: that may be something to fix.
    /// A rejection that is no refusal starts the count over.
    fn rejected(&mut self, job: Job, key: &str, state: JobState, e: &Error) {
        let Some(after) = job.unavailable_after() else {
            warn!(job = %key, error = %e, "GitLab rejected the sync fetch; backing off");
            self.back_off(key, state, REJECTED_BACKOFF_CAP);
            return;
        };
        if !job.refused_by(e) {
            warn!(job = %key, error = %e, "GitLab rejected the sync fetch; backing off");
            let answered = JobState {
                rejections: 0,
                ..state
            };
            self.back_off(key, answered, REJECTED_BACKOFF_CAP);
            return;
        }
        let rejections = state.rejections.saturating_add(1);
        let counted = JobState {
            rejections,
            ..state
        };
        if rejections < after {
            warn!(job = %key, error = %e, rejections, "GitLab rejected the sync fetch; backing off");
            self.back_off(key, counted, REJECTED_BACKOFF_CAP);
            return;
        }
        if rejections == after && after > 1 {
            info!(
                job = %key,
                error = %e,
                rejections,
                rest_secs = UNAVAILABLE_REST_SECS,
                "GitLab keeps refusing this sync fetch; treating it as unavailable and asking once a day"
            );
        } else {
            debug!(job = %key, error = %e, rejections, "GitLab doesn't serve this here; resting the job");
        }
        self.rest(key, counted, UNAVAILABLE_REST_SECS);
    }

    fn back_off(&mut self, key: &str, state: JobState, cap: u64) {
        let failures = state.failures.saturating_add(1);
        self.rest(key, state, schedule::backoff(failures, cap));
    }

    /// Count a failure and hold the job back for about `secs`.
    fn rest(&mut self, key: &str, state: JobState, secs: u64) {
        let jitter = self.config.read().unwrap().sync.jitter;
        let failures = state.failures.saturating_add(1);
        let delay = schedule::jittered(secs, key, u64::from(failures), jitter);
        let next = JobState {
            failures,
            retry_at: now_secs().saturating_add(delay.max(1)),
            ..state
        };
        let persisted = (|| -> Result<()> {
            let mut c = self.store.begin();
            // With why, so the job still says it after a restart.
            match self.errors.get(key) {
                Some(error) => c.set_failed_job(key, &next, error)?,
                None => c.set_job(key, &next)?,
            }
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
        // Only a fetch into the slice being dropped is void.
        let void = self.flights.iter().filter(|(_, f)| what.resets(&f.key));
        let void = void.map(|(&run, _)| run).collect();
        for flight in self.cancel(void) {
            info!(job = %flight.key, "sync job cancelled by a cache clear");
            // Its waiters still want it, now on the cleared store.
            if let Some(waiters) = flight.waiters {
                self.demand.entry(flight.job).or_default().extend(waiters);
            }
        }
        let persisted = self.store.job_states().unwrap_or_else(|e| {
            warn!(error = %e, "reading job states for a clear failed");
            Vec::new()
        });
        let mut reset: Vec<String> = persisted
            .into_iter()
            .map(|(key, _)| key)
            .chain(self.states.keys().cloned())
            .filter(|k| what.resets(k))
            .collect();
        reset.sort_unstable();
        reset.dedup();
        let cleared = (|| -> Result<()> {
            let mut c = self.store.begin();
            match what {
                Clear::Everything => c.wipe()?,
                Clear::Assigned => {
                    c.remove_view(ASSIGNED_ISSUES);
                    c.remove_view(ASSIGNED_MERGE_REQUESTS);
                    c.remove_view(RECENT_AUTHORED_ISSUES);
                    c.remove_view(RECENT_ASSIGNED_ISSUES);
                    c.remove_where::<Board>(RowScope::All, |_| false)?;
                    c.remove_where::<DescriptionTemplate>(RowScope::All, |_| false)?;
                }
                Clear::Corpus => {
                    c.remove_where::<Issue>(RowScope::All, |_| false)?;
                    c.remove_where::<MergeRequest>(RowScope::All, |_| false)?;
                    c.remove_where::<Project>(RowScope::All, |_| false)?;
                    c.remove_where::<Group>(RowScope::All, |_| false)?;
                    c.remove_where::<Epic>(RowScope::All, |_| false)?;
                    c.remove_where::<Avatar>(RowScope::All, |_| false)?;
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
            Ok(()) => {
                info!(?what, jobs_reset = reset.len(), "synced data cleared");
                if matches!(what, Clear::Everything | Clear::Corpus) {
                    self.sweep_avatars();
                }
            }
            Err(e) => warn!(?what, error = %e, "clearing synced data failed"),
        }
        for key in &reset {
            self.states.remove(key);
            self.errors.remove(key);
        }
        // A full wipe leaves no corpus a replan could throw away.
        if what != Clear::Everything {
            self.incomplete.extend(
                self.plan
                    .jobs
                    .iter()
                    .filter(|j| j.feeds_plan() && what.resets(&j.key())),
            );
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
        let changed = self.store.identity()?.filter(|k| *k != me);
        if let Some(known) = &changed {
            info!(
                from_host = %known.host,
                from_user = known.user_id,
                host = %me.host,
                user_id = me.user_id,
                "GitLab account changed; dropping the previous account's synced data"
            );
            c.wipe()?;
            self.states.clear();
            self.errors.clear();
            // Nor may what is still being fetched for it land here.
            for flight in std::mem::take(&mut self.flights).into_values() {
                flight.abort.abort();
            }
            // Nothing of the other account's plan may survive the replan.
            self.plan = Plan::default();
            self.incomplete.clear();
            self.relisted.clear();
            self.replan = true;
        }
        c.set_identity(&me)?;
        c.commit()?;
        if changed.is_some() {
            self.sweep_avatars();
        }
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
        let mut plan = match planner::plan(&self.store, population, since) {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, "sync planning failed; keeping the previous plan");
                self.replan = true;
                if self.plan.jobs.is_empty() {
                    self.plan = Plan::base();
                }
                return;
            }
        };
        let unseen: Vec<i64> = plan.unlisted.difference(&self.relisted).copied().collect();
        if !unseen.is_empty() {
            // Joined or created since the daily member listing: rerun it now
            // rather than leave them out of search for up to a day.
            info!(projects = ?unseen, "activity in projects the member listing lacks; re-listing");
            self.relisted.extend(unseen);
            self.demand.entry(Job::MemberProjects).or_default();
        }
        if !self.incomplete.is_empty() {
            // The dropped evidence is about to come back; dropping jobs now
            // would throw away corpora that must then be refetched in full.
            plan.jobs.extend(self.plan.jobs.iter().copied());
            for (&project, &url) in &self.plan.avatars {
                plan.avatars.entry(project).or_insert(url);
            }
        }
        if plan.jobs == self.plan.jobs {
            self.plan = plan;
            return;
        }
        let added = plan.jobs.difference(&self.plan.jobs).count();
        let dropped = self.plan.jobs.difference(&plan.jobs).count();
        let (states_removed, rows_removed) = if self.incomplete.is_empty() {
            self.prune(&plan)
        } else {
            (0, 0)
        };
        info!(
            jobs = plan.jobs.len(),
            tracked = plan.tracked.len(),
            corpus = plan.corpus,
            epic_groups = plan.epic_groups,
            avatars = plan.avatars.len(),
            from_assignments = plan.evidence.assigned,
            from_events = plan.evidence.events,
            from_timelogs = plan.evidence.timelogs,
            switched_off = plan.switched_off.len(),
            added,
            dropped,
            states_removed,
            rows_removed,
            "sync plan updated"
        );
        if plan.switched_off != self.plan.switched_off {
            let keys: Vec<String> = plan.switched_off.iter().map(Job::key).collect();
            debug!(jobs = ?keys, "not planned: their projects have the feature switched off");
        }
        self.plan = plan;
        let planned = &self.plan.jobs;
        self.demand.retain(|job, _| planned.contains(job));
        // Its rows and state would outlive the plan.
        let unplanned = self
            .flights
            .iter()
            .filter(|(_, f)| !planned.contains(&f.job));
        let unplanned = unplanned.map(|(&run, _)| run).collect();
        for flight in self.cancel(unplanned) {
            debug!(job = %flight.key, "sync job cancelled: no longer planned");
        }
    }

    /// Drop what no job in `plan` keeps fresh: the states of unplanned jobs,
    /// whether they left the plan now or before a restart, and their rows.
    /// Returns how many states and rows went.
    fn prune(&mut self, plan: &Plan) -> (usize, usize) {
        let planned: HashSet<String> = plan.jobs.iter().map(Job::key).collect();
        let stale: Vec<String> = self
            .states
            .keys()
            .filter(|k| !planned.contains(*k))
            .cloned()
            .collect();
        let collected = (|| -> Result<usize> {
            let mut c = self.store.begin();
            for key in &stale {
                c.remove_job(key);
            }
            let removed = planner::collect_garbage(&mut c, &self.store, plan)?;
            c.commit()?;
            Ok(removed)
        })();
        match collected {
            Ok(rows) => {
                for key in &stale {
                    self.states.remove(key);
                    self.errors.remove(key);
                }
                self.sweep_avatars();
                (stale.len(), rows)
            }
            Err(e) => {
                warn!(error = %e, "dropping rows of unplanned jobs failed");
                (0, 0)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::DormancyReason;
    use crate::gitlab::{GitlabApi, Issuable, Listing};
    use crate::sync::jobs::ISSUE_VIEWS;
    use crate::sync::model::{Issue, Project, UserRef};
    use crate::sync::store::View;
    use crate::testing::{
        FakeErr, FakeGitlab, PNG, RECENT_ASSIGNED_PATH, RECENT_AUTHORED_PATH, epic_json,
        event_json, eventually, group_json, issue_json, project_json, project_json_with_avatar,
    };
    use crate::write::WriteOp;

    /// What an empty store plans before any evidence arrives.
    const BASE: [Job; 9] = [
        Job::AssignedIssues,
        Job::AssignedMergeRequests,
        Job::RecentTimelogs,
        Job::AllTimelogs,
        Job::Events,
        Job::MemberProjects,
        Job::MemberGroups,
        Job::RecentAuthoredIssues,
        Job::RecentAssignedIssues,
    ];

    struct Env {
        sync: Arc<SyncHandle>,
        config: SharedConfig,
        session: SessionSlot,
        reconnect: Arc<Reconnect>,
        store: Arc<SyncStore>,
        avatars: AvatarDir,
        _dir: Option<tempfile::TempDir>,
        _avatar_tmp: Option<tempfile::TempDir>,
    }

    impl Env {
        fn keeping(mut self, avatars: tempfile::TempDir) -> Self {
            self._avatar_tmp = Some(avatars);
            self
        }
    }

    fn open_store() -> (Arc<SyncStore>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = fjall::Database::builder(dir.path().join("db"))
            .open()
            .unwrap();
        (Arc::new(SyncStore::open(&db).unwrap()), dir)
    }

    /// The avatar directory of a store opened by [`open_store`] in `dir`.
    fn avatar_dir(dir: &tempfile::TempDir) -> AvatarDir {
        AvatarDir::new(dir.path().join("avatars"))
    }

    fn connected(fake: &Arc<FakeGitlab>, user_id: i64) -> ConnState {
        ConnState::Connected(Session {
            gitlab: Arc::clone(fake) as Arc<dyn GitlabApi>,
            host: "gitlab.test".into(),
            user_id,
            username: "tester".into(),
            token: Default::default(),
        })
    }

    /// No gap between launches and no startup spread, so tests don't wait.
    fn instant_config() -> SharedConfig {
        let mut cfg = crate::config::defaults();
        cfg.sync.job_gap_ms = 0;
        cfg.sync.startup_spread_secs = 0;
        Arc::new(std::sync::RwLock::new(cfg))
    }

    /// [`instant_config`] with up to `max` fetches in flight.
    fn flying(max: u64) -> SharedConfig {
        let config = instant_config();
        config.write().unwrap().sync.max_in_flight = max;
        config
    }

    fn start_on(store: Arc<SyncStore>, state: ConnState) -> Env {
        start_probing(store, state, crate::reconnect::no_keychain_probe())
    }

    fn start_probing(store: Arc<SyncStore>, state: ConnState, probe: KeychainProbe) -> Env {
        let tmp = tempfile::tempdir().unwrap();
        start_in(store, AvatarDir::new(tmp.path()), state, probe).keeping(tmp)
    }

    /// A worker keeping its avatars in `dir`, next to the store's database.
    fn start_with_avatars(store: Arc<SyncStore>, dir: &tempfile::TempDir, state: ConnState) -> Env {
        let probe = crate::reconnect::no_keychain_probe();
        start_in(store, avatar_dir(dir), state, probe)
    }

    fn start_in(
        store: Arc<SyncStore>,
        avatars: AvatarDir,
        state: ConnState,
        probe: KeychainProbe,
    ) -> Env {
        start_configured(store, avatars, state, probe, instant_config())
    }

    /// A worker with `max` fetches in flight at most, on a store in `dir`.
    fn start_flying(
        max: u64,
        store: Arc<SyncStore>,
        dir: &tempfile::TempDir,
        state: ConnState,
    ) -> Env {
        let probe = crate::reconnect::no_keychain_probe();
        start_configured(store, avatar_dir(dir), state, probe, flying(max))
    }

    fn start_configured(
        store: Arc<SyncStore>,
        avatars: AvatarDir,
        state: ConnState,
        probe: KeychainProbe,
        config: SharedConfig,
    ) -> Env {
        let session: SessionSlot = Arc::new(tokio::sync::RwLock::new(state));
        let reconnect = Arc::new(Reconnect::default());
        let sync = SyncHandle::spawn(
            Arc::clone(&store),
            avatars.clone(),
            Arc::clone(&session),
            Arc::clone(&config),
            Arc::clone(&reconnect),
            probe,
        );
        Env {
            sync,
            config,
            session,
            reconnect,
            store,
            avatars,
            _dir: None,
            _avatar_tmp: None,
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

    /// Mark `jobs` as synced just now, so none of them is due.
    fn mark_synced(store: &SyncStore, jobs: &[Job]) {
        let now = now_secs();
        // For the avatar URLs the fingerprints cover.
        let plan = planner::plan(store, crate::config::SearchPopulation::Tracked, 0).unwrap();
        let mut c = store.begin();
        for job in jobs {
            let state = JobState {
                last_ok: now,
                last_full: now,
                fingerprint: plan.fingerprint(*job, &crate::config::defaults()),
                ..Default::default()
            };
            c.set_job(&job.key(), &state).unwrap();
        }
        c.commit().unwrap();
    }

    fn issue_row(project_id: i64, iid: i64) -> Issue {
        Issue {
            id: project_id * 1000 + iid,
            iid,
            project_id,
            state: "opened".into(),
            ..Default::default()
        }
    }

    /// Project 7 with issues #1 and #2, tracked only through the assigned
    /// #1, everything synced just now.
    fn seed_assigned_project(store: &SyncStore) {
        let mut c = store.begin();
        c.upsert(&[Project {
            id: 7,
            ..Default::default()
        }])
        .unwrap();
        c.upsert(&[issue_row(7, 1), issue_row(7, 2)]).unwrap();
        c.set_view(
            ASSIGNED_ISSUES,
            &crate::sync::store::View {
                keys: vec![(7, 1)],
                fetched_at: now_secs(),
            },
        )
        .unwrap();
        c.commit().unwrap();
        let mut jobs = BASE.to_vec();
        jobs.extend([
            Job::ProjectIssues(7),
            Job::ProjectMergeRequests(7),
            Job::ProjectBoards(7),
            Job::ProjectTemplates(7, crate::gitlab::Issuable::Issue),
            Job::ProjectTemplates(7, crate::gitlab::Issuable::MergeRequest),
        ]);
        mark_synced(store, &jobs);
    }

    #[tokio::test]
    async fn a_projects_first_run_is_full_and_later_ones_delta() {
        let fake = Arc::new(FakeGitlab::default());
        fake.serve("events", vec![event_json(1, 7, "pushed to", now_secs())]);
        fake.serve("projects", vec![project_json(7)]);
        fake.serve("projects/7/issues", vec![issue_json(7, 1, "one")]);
        let env = start(connected(&fake, 1));

        env.sync
            .refresh_now(&[Job::Events, Job::MemberProjects])
            .await;
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
    async fn a_network_error_demotes_the_session_and_backs_off_the_job() {
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
        let failed = state(&env, Job::AssignedIssues);
        assert_eq!((failed.last_ok, failed.failures), (0, 1));
        assert!(failed.retry_at > now_secs(), "{failed:?}");
    }

    /// A 401 is the token's fault, not the job's: the session parks as
    /// rejected, and a login then runs everything that was backed off.
    #[tokio::test]
    async fn a_dead_token_parks_the_session_and_a_login_unparks_the_jobs() {
        let fake = Arc::new(FakeGitlab::default());
        fake.fail_next("issues", FakeErr::Rejected);
        fake.fail_next("merge_requests", FakeErr::Unauthorized);
        let env = start(connected(&fake, 1));

        eventually("the token rejection", || {
            matches!(
                env.session.try_read().as_deref(),
                Ok(ConnState::Dormant(DormancyReason::TokenRejected { .. }))
            )
        })
        .await;
        assert_eq!(state(&env, Job::AssignedMergeRequests), JobState::default());
        assert!(state(&env, Job::AssignedIssues).retry_at > now_secs());
        assert!(
            tokio::time::timeout(Duration::from_millis(100), env.reconnect.notified())
                .await
                .is_err(),
            "nothing to retry without a new login"
        );

        env.sync.logged_in();
        eventually("the backoff to clear", || {
            state(&env, Job::AssignedIssues).retry_at == 0
        })
        .await;
        assert_eq!(state(&env, Job::AssignedIssues).failures, 0);
    }

    /// Another machine sharing the keychain rotated the token: the 401 hands
    /// the session to the reconnect supervisor instead of parking it.
    #[tokio::test]
    async fn a_dead_token_with_a_newer_one_stored_reconnects_instead_of_parking() {
        let fake = Arc::new(FakeGitlab::default());
        fake.fail_next("issues", FakeErr::Unauthorized);
        let (store, _dir) = open_store();
        let probe: KeychainProbe = Arc::new(|_| Box::pin(async { true }));
        let env = start_probing(store, connected(&fake, 1), probe);

        tokio::time::timeout(Duration::from_secs(2), env.reconnect.notified())
            .await
            .expect("the reconnect supervisor is woken");
        assert!(matches!(
            &*env.session.read().await,
            ConnState::Dormant(r) if r.is_auto_retryable()
        ));
        assert_eq!(state(&env, Job::AssignedIssues), JobState::default());
    }

    #[tokio::test]
    async fn a_rate_limit_pauses_everything_without_demoting() {
        let fake = Arc::new(FakeGitlab::default());
        fake.fail_next("issues", FakeErr::Throttled(429));
        // One at a time: nothing else is in flight when the pause starts.
        let (store, dir) = open_store();
        let env = start_flying(1, store, &dir, connected(&fake, 1));

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
        fake.serve("projects", vec![project_json(7), project_json(8)]);
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

    /// `job`'s line in the worker's snapshot.
    async fn info(env: &Env, job: Job) -> JobInfo {
        let key = job.key();
        let snapshot = env.sync.jobs().await;
        let found = snapshot.jobs.into_iter().find(|j| j.key == key);
        found.unwrap_or_else(|| panic!("{key} is not in the snapshot"))
    }

    #[tokio::test]
    async fn the_snapshot_shows_the_job_in_flight_and_the_demand_behind_it() {
        let fake = Arc::new(FakeGitlab::default());
        let gate = fake.gate("issues");
        let before = now_secs();
        // One at a time, so everything else queues behind the gated fetch.
        let (store, dir) = open_store();
        let env = start_flying(1, store, &dir, connected(&fake, 1));
        tokio::time::timeout(Duration::from_secs(2), fake.gated.notified())
            .await
            .expect("the assigned issues fetch starts");
        env.sync.refresh_soon(&[Job::MemberGroups]);

        let snapshot = tokio::time::timeout(Duration::from_secs(2), env.sync.jobs())
            .await
            .expect("the snapshot doesn't wait for the fetch");
        assert_eq!(snapshot.paused_until, None);
        assert_eq!(snapshot.jobs.len(), BASE.len(), "{snapshot:?}");
        let running = &snapshot.jobs[0];
        assert_eq!(running.key, ASSIGNED_ISSUES);
        assert_eq!(running.status, JobStatus::Running);
        assert!(running.running_since.is_some_and(|at| at >= before));
        assert_eq!((running.last_ok, running.next_due), (0, None));
        // No row yet, nothing announced, and no delta to tell a full run from.
        assert_eq!(
            (running.full, running.fetched, running.expected),
            (None, Some(0), None)
        );
        // Demanded runs ahead of the jobs that are merely due.
        let demanded = &snapshot.jobs[1];
        assert_eq!(demanded.key, Job::MemberGroups.key());
        assert_eq!(demanded.status, JobStatus::Demanded);
        for job in &snapshot.jobs[2..] {
            assert_eq!(job.status, JobStatus::Due, "{job:?}");
            assert_eq!(job.running_since, None);
            assert_eq!((job.full, job.fetched, job.expected), (None, None, None));
        }

        gate.notify_one();
        eventually("the assigned issues", || {
            state(&env, Job::AssignedIssues).last_ok > 0
        })
        .await;
        let done = info(&env, Job::AssignedIssues).await;
        assert_eq!(done.status, JobStatus::Waiting);
        assert_eq!(done.running_since, None);
        assert!(done.last_ok >= before);
        assert!(
            done.next_due.is_some_and(|at| at > done.last_ok),
            "{done:?}"
        );
    }

    /// A project's first run walks twice (the listing, then the late pass):
    /// the snapshot follows the rows through both, and forgets them with the
    /// flight.
    #[tokio::test]
    async fn the_snapshot_follows_a_running_fetch() {
        let fake = Arc::new(FakeGitlab::default());
        serve_tracked_project(&fake);
        fake.serve(
            "projects/7/issues",
            vec![issue_json(7, 1, "one"), issue_json(7, 2, "two")],
        );
        let gate = fake.gate("projects/7/issues");
        let env = start(connected(&fake, 1));
        let progress = |j: JobInfo| (j.full, j.fetched, j.expected);

        tokio::time::timeout(Duration::from_secs(2), fake.gated.notified())
            .await
            .expect("the project fetch starts");
        let walking = info(&env, Job::ProjectIssues(7)).await;
        assert_eq!(walking.status, JobStatus::Running);
        assert_eq!(progress(walking), (Some(true), Some(0), Some(2)));

        let late = fake.gate("projects/7/issues");
        gate.notify_one();
        tokio::time::timeout(Duration::from_secs(2), fake.gated.notified())
            .await
            .expect("the late pass starts");
        let again = info(&env, Job::ProjectIssues(7)).await;
        assert_eq!(progress(again), (Some(true), Some(2), Some(4)));

        late.notify_one();
        eventually("the project's issues", || {
            state(&env, Job::ProjectIssues(7)).last_ok > 0
        })
        .await;
        let done = info(&env, Job::ProjectIssues(7)).await;
        assert_eq!(progress(done), (None, None, None));
    }

    #[tokio::test]
    async fn the_snapshot_shows_a_failed_jobs_backoff_and_error() {
        let fake = Arc::new(FakeGitlab::default());
        let now = now_secs();
        fake.serve("events", vec![event_json(1, 7, "opened", now)]);
        fake.serve("projects", vec![project_json(7)]);
        fake.fail_next("projects/7/issues", FakeErr::Rejected);
        let env = start(connected(&fake, 1));
        eventually("the rejected fetch", || {
            state(&env, Job::ProjectIssues(7)).failures == 1
        })
        .await;

        let failed = info(&env, Job::ProjectIssues(7)).await;
        assert_eq!(failed.status, JobStatus::BackingOff);
        assert_eq!(failed.failures, 1);
        assert_eq!(
            failed.next_due,
            Some(state(&env, Job::ProjectIssues(7)).retry_at)
        );
        assert!(failed.last_error.is_some_and(|e| !e.is_empty()));
        // Nothing runs later than a job that backs off for an hour or more.
        eventually("the other jobs", || {
            state(&env, Job::ProjectMergeRequests(7)).last_ok > 0
        })
        .await;
        let merge_requests = info(&env, Job::ProjectMergeRequests(7)).await;
        assert_eq!(merge_requests.status, JobStatus::Waiting);
        assert_eq!(
            (merge_requests.failures, merge_requests.last_error),
            (0, None)
        );

        // A new login retries it, and the success clears the error.
        env.sync.logged_in();
        eventually("the retry", || {
            state(&env, Job::ProjectIssues(7)).last_ok > 0
        })
        .await;
        let retried = info(&env, Job::ProjectIssues(7)).await;
        assert_eq!(retried.status, JobStatus::Waiting);
        assert_eq!((retried.failures, retried.last_error), (0, None));
    }

    #[tokio::test]
    async fn the_snapshot_shows_the_rate_limit_pause() {
        let fake = Arc::new(FakeGitlab::default());
        fake.fail_next("issues", FakeErr::Throttled(429));
        let env = start(connected(&fake, 1));
        eventually("the rate-limited call", || {
            !fake.calls_to("issues").is_empty()
        })
        .await;

        let mut snapshot = env.sync.jobs().await;
        for _ in 0..100 {
            if snapshot.paused_until.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            snapshot = env.sync.jobs().await;
        }
        assert!(snapshot.paused_until.is_some_and(|at| at > now_secs()));
        // Not the job's fault: it stays due and runs first after the pause.
        let limited = &snapshot.jobs[0];
        assert_eq!(limited.key, ASSIGNED_ISSUES);
        assert_eq!((limited.status, limited.failures), (JobStatus::Due, 0));
        assert!(limited.last_error.is_some());
    }

    #[tokio::test]
    async fn the_snapshot_is_served_while_dormant() {
        let env = start(ConnState::Dormant(DormancyReason::NoCredentials));

        let snapshot = tokio::time::timeout(Duration::from_secs(2), env.sync.jobs())
            .await
            .expect("a dormant worker answers");
        let mut keys: Vec<String> = snapshot.jobs.iter().map(|j| j.key.clone()).collect();
        keys.sort();
        let mut planned: Vec<String> = BASE.iter().map(Job::key).collect();
        planned.sort();
        assert_eq!(keys, planned);
        assert!(snapshot.jobs.iter().all(|j| j.running_since.is_none()));
    }

    #[tokio::test]
    async fn a_due_job_waits_for_a_session_while_dormant() {
        let env = start(ConnState::Dormant(DormancyReason::NoCredentials));
        let snapshot = env.sync.jobs().await;
        assert!(!snapshot.connected);
        assert_eq!(snapshot.jobs.len(), BASE.len());
        for job in &snapshot.jobs {
            assert_eq!(
                (job.status, &job.hold),
                (JobStatus::Due, &Some(Hold::Session)),
                "{job:?}"
            );
        }
    }

    /// The pause is no job's fault and every job's hold: nothing starts
    /// before it is over, whatever else would let it.
    #[tokio::test]
    async fn a_rate_limit_holds_every_due_job() {
        let fake = Arc::new(FakeGitlab::default());
        fake.fail_next("issues", FakeErr::Throttled(429));
        let env = start(connected(&fake, 1));
        let paused = |s: &Snapshot| s.paused_until.is_some();
        let snapshot = snapshot_when(&env, "the rate-limit pause", paused).await;
        assert!(snapshot.connected);
        let due = snapshot.jobs.iter().filter(|j| j.status == JobStatus::Due);
        let holds: Vec<&Option<Hold>> = due.map(|j| &j.hold).collect();
        assert!(holds.len() > 1, "{snapshot:?}");
        assert!(
            holds.iter().all(|h| **h == Some(Hold::RateLimit)),
            "{snapshot:?}"
        );
    }

    /// Background jobs are spread over the time after the daemon's start: one
    /// whose interval has long passed says that this is what it waits for.
    #[tokio::test]
    async fn after_a_start_the_background_jobs_say_they_are_spread_out() {
        let config = instant_config();
        config.write().unwrap().sync.startup_spread_secs = 3600;
        let (store, dir) = open_store();
        let probe = crate::reconnect::no_keychain_probe();
        // Dormant, so nothing runs and every job shows what it waits for.
        let dormant = ConnState::Dormant(DormancyReason::NoCredentials);
        let env = start_configured(store, avatar_dir(&dir), dormant, probe, config);

        let snapshot = env.sync.jobs().await;
        let now = now_secs();
        let spread = snapshot
            .jobs
            .iter()
            .filter(|j| j.hold == Some(Hold::Startup));
        let spread: Vec<&JobInfo> = spread.collect();
        assert!(!spread.is_empty(), "{snapshot:?}");
        for job in &spread {
            assert_eq!(job.status, JobStatus::Waiting, "{job:?}");
            assert!(job.next_due.is_some_and(|at| at > now), "{job:?}");
        }
        // The foreground lists wait for nothing but the session.
        let foreground = info(&env, Job::AssignedIssues).await;
        assert_eq!(
            (foreground.status, foreground.hold),
            (JobStatus::Due, Some(Hold::Session))
        );
    }

    /// The member group above a corpus project gets its epics synced once
    /// both listings are in.
    #[tokio::test]
    async fn epics_sync_for_the_groups_above_the_corpus() {
        let fake = Arc::new(FakeGitlab::default());
        fake.serve("events", vec![event_json(1, 7, "opened", now_secs())]);
        fake.serve("projects", vec![project_json(7)]);
        fake.serve("groups", vec![group_json(3, "g"), group_json(4, "other")]);
        fake.serve("groups/3/epics", vec![epic_json(3, 5, "Accounts")]);
        let env = start(connected(&fake, 1));

        eventually("the group's epics", || {
            env.store.epics.get((3, 5)).unwrap().is_some()
        })
        .await;
        assert_eq!(
            fake.calls_to("groups/3/epics")[0],
            Listing::GroupEpics {
                group_id: 3,
                updated_after: None
            }
        );
        assert!(
            fake.calls_to("groups/4/epics").is_empty(),
            "no corpus project lies in `other`"
        );
    }

    /// GitLab without epics (no Premium) rejects the listing: the job rests
    /// for about a day instead of climbing the backoff, and nothing else is
    /// held up.
    #[tokio::test]
    async fn an_instance_without_epics_rests_the_job() {
        let (logs, _guard) = Logs::capture();
        let fake = Arc::new(FakeGitlab::default());
        let now = now_secs();
        fake.serve("events", vec![event_json(1, 7, "opened", now)]);
        fake.serve("projects", vec![project_json(7)]);
        fake.serve("groups", vec![group_json(3, "g")]);
        fake.fail_next("groups/3/epics", FakeErr::Rejected);
        let env = start(connected(&fake, 1));

        eventually("the epics job to rest", || {
            state(&env, Job::GroupEpics(3)).failures == 1
                && state(&env, Job::ProjectIssues(7)).last_ok > 0
        })
        .await;
        let resting = state(&env, Job::GroupEpics(3));
        assert!(
            resting.retry_at > now + REJECTED_BACKOFF_CAP,
            "longer than any rejection backoff: {resting:?}"
        );
        assert_eq!(fake.calls_to("groups/3/epics").len(), 1);
        assert!(matches!(
            &*env.session.read().await,
            ConnState::Connected(_)
        ));
        // Unavailable at the first rejection, and quiet about it.
        assert_eq!(resting.rejections, 1);
        let epics = info(&env, Job::GroupEpics(3)).await;
        assert!(epics.unavailable, "{epics:?}");
        assert_eq!(epics.status, JobStatus::Waiting);
        assert_eq!(epics.next_due, Some(resting.retry_at));
        assert!(epics.last_error.is_some());
        assert_eq!(logs.count("WARN", "group/3/epics"), 0, "{}", logs.text());
        assert_eq!(logs.count("INFO", "group/3/epics"), 0, "{}", logs.text());
        assert!(logs.text().contains("GitLab doesn't serve this here"));
    }

    /// What the worker logs on the test's thread, where a `#[tokio::test]`
    /// runs it and its fetches.
    #[derive(Clone, Default)]
    struct Logs(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Logs {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logs {
        type Writer = Logs;
        fn make_writer(&'a self) -> Logs {
            self.clone()
        }
    }

    impl Logs {
        /// Capture everything from debug up until the guard drops.
        fn capture() -> (Self, tracing::subscriber::DefaultGuard) {
            let logs = Logs::default();
            let subscriber = tracing_subscriber::fmt()
                .with_writer(logs.clone())
                .with_max_level(tracing::Level::DEBUG)
                .with_ansi(false)
                .finish();
            (logs, tracing::subscriber::set_default(subscriber))
        }

        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }

        /// The lines at `level` about the job keyed `key`.
        fn count(&self, level: &str, key: &str) -> usize {
            self.said(level, key, "")
        }

        /// The lines at `level` about the job keyed `key` that say `what`.
        fn said(&self, level: &str, key: &str, what: &str) -> usize {
            let (job, level) = (format!("job={key}"), format!(" {level} "));
            let text = self.text();
            let about_job = |l: &&str| l.split(' ').any(|word| word == job);
            text.lines()
                .filter(about_job)
                .filter(|l| l.contains(&level) && l.contains(what))
                .count()
        }
    }

    /// Project 7, tracked through an event and listed as a member, so its
    /// jobs are planned once the worker read both.
    fn serve_tracked_project(fake: &FakeGitlab) {
        fake.serve("events", vec![event_json(1, 7, "opened", now_secs())]);
        fake.serve("projects", vec![project_json(7)]);
    }

    /// Run `job` once more, ahead of its backoff, and wait for the run.
    async fn rerun(env: &Env, job: Job) {
        tokio::time::timeout(Duration::from_secs(2), env.sync.refresh_now(&[job]))
            .await
            .expect("the demanded run");
    }

    /// Wait for `job`'s first scheduled run to have failed.
    async fn first_failure(env: &Env, job: Job) {
        eventually(&format!("{} to fail", job.key()), || {
            state(env, job).failures >= 1
        })
        .await;
    }

    /// The third 403 in a row makes a per-project job unavailable: it rests
    /// a day instead of backing off, its snapshot says so, and its later
    /// refusals only log at debug level. Two refusals aren't enough.
    #[tokio::test]
    async fn three_refusals_in_a_row_make_a_project_job_unavailable() {
        let (logs, _guard) = Logs::capture();
        let fake = Arc::new(FakeGitlab::default());
        serve_tracked_project(&fake);
        for _ in 0..4 {
            fake.fail_next("projects/7/boards", FakeErr::Rejected);
        }
        let job = Job::ProjectBoards(7);
        let key = job.key();
        let env = start(connected(&fake, 1));

        first_failure(&env, job).await;
        rerun(&env, job).await;
        let twice = state(&env, job);
        assert_eq!((twice.failures, twice.rejections), (2, 2));
        assert!(
            twice.retry_at < now_secs() + REJECTED_BACKOFF_CAP,
            "{twice:?}"
        );
        let failing = info(&env, job).await;
        assert!(!failing.unavailable, "two refusals: {failing:?}");
        assert_eq!(failing.status, JobStatus::BackingOff);
        assert_eq!(logs.count("WARN", &key), 2, "{}", logs.text());

        rerun(&env, job).await;
        let now = now_secs();
        let thrice = state(&env, job);
        assert_eq!((thrice.failures, thrice.rejections), (3, 3));
        assert!(
            thrice.retry_at > now + REJECTED_BACKOFF_CAP,
            "rests about a day: {thrice:?}"
        );
        let unavailable = info(&env, job).await;
        assert!(unavailable.unavailable, "{unavailable:?}");
        assert_eq!(unavailable.status, JobStatus::Waiting);
        assert_eq!(unavailable.next_due, Some(thrice.retry_at));
        assert_eq!(unavailable.failures, 3);
        assert!(
            unavailable
                .last_error
                .as_deref()
                .is_some_and(|e| e.contains("403")),
            "{unavailable:?}"
        );
        assert_eq!(logs.count("WARN", &key), 2, "{}", logs.text());
        assert_eq!(logs.count("INFO", &key), 1, "said once: {}", logs.text());

        // From now on it fails quietly.
        rerun(&env, job).await;
        assert_eq!(state(&env, job).rejections, 4);
        assert!(info(&env, job).await.unavailable);
        assert_eq!(logs.count("WARN", &key), 2, "{}", logs.text());
        assert_eq!(logs.count("INFO", &key), 1, "{}", logs.text());
        assert_eq!(
            logs.said("DEBUG", &key, "GitLab doesn't serve this here"),
            1,
            "{}",
            logs.text()
        );
        assert!(matches!(
            &*env.session.read().await,
            ConnState::Connected(_)
        ));
    }

    /// A 404 (a project GitLab hides, a route an instance lacks) refuses
    /// like a 403.
    #[tokio::test]
    async fn a_not_found_counts_like_a_forbidden() {
        let fake = Arc::new(FakeGitlab::default());
        serve_tracked_project(&fake);
        for err in [
            FakeErr::RejectedWith(404),
            FakeErr::Rejected,
            FakeErr::RejectedWith(404),
        ] {
            fake.fail_next("projects/7/merge_requests", err);
        }
        let job = Job::ProjectMergeRequests(7);
        let env = start(connected(&fake, 1));

        first_failure(&env, job).await;
        rerun(&env, job).await;
        rerun(&env, job).await;
        assert_eq!(state(&env, job).rejections, 3);
        assert!(info(&env, job).await.unavailable);
    }

    /// Network errors, 5xx and 429 don't say whether GitLab serves the
    /// listing: they don't count as refusals, and they don't start the
    /// count over either, so refusals around an outage still add up. Two
    /// network errors and a 403 are one refusal.
    #[tokio::test]
    async fn failures_that_say_nothing_about_the_listing_neither_count_nor_reset() {
        let fake = Arc::new(FakeGitlab::default());
        serve_tracked_project(&fake);
        for err in [
            FakeErr::Transient,
            FakeErr::Transient,
            FakeErr::Rejected,
            FakeErr::Throttled(503),
            FakeErr::Rejected,
            FakeErr::Throttled(502),
            FakeErr::Rejected,
        ] {
            fake.fail_next("projects/7/issues", err);
        }
        let job = Job::ProjectIssues(7);
        let env = start(connected(&fake, 1));
        // What the reconnect supervisor would do once the network error
        // demoted the session.
        let reconnect = || async {
            eventually("the demotion", || {
                matches!(
                    env.session.try_read().as_deref(),
                    Ok(ConnState::Dormant(DormancyReason::Unreachable { .. }))
                )
            })
            .await;
            *env.session.write().await = connected(&fake, 1);
            env.sync.wake();
        };

        first_failure(&env, job).await;
        reconnect().await;
        rerun(&env, job).await;
        reconnect().await;
        assert_eq!(state(&env, job).failures, 2);
        assert_eq!(state(&env, job).rejections, 0, "network errors");
        rerun(&env, job).await;
        let once = state(&env, job);
        assert_eq!((once.failures, once.rejections), (3, 1));
        assert!(!info(&env, job).await.unavailable, "one refusal, not three");

        rerun(&env, job).await;
        assert_eq!(state(&env, job).rejections, 1, "a 503 leaves the count");
        rerun(&env, job).await;
        assert_eq!(state(&env, job).rejections, 2);
        rerun(&env, job).await;
        assert_eq!(state(&env, job).rejections, 2, "a 502 too");
        assert!(!info(&env, job).await.unavailable);
        rerun(&env, job).await;
        let done = state(&env, job);
        assert_eq!((done.failures, done.rejections), (7, 3));
        assert!(info(&env, job).await.unavailable);
    }

    /// A 400 (or a 422, …) is GitLab answering the request, not refusing
    /// the listing: it backs off like any rejection, never makes the job
    /// unavailable, and starts the refusal count over.
    #[tokio::test]
    async fn a_bad_request_never_makes_a_job_unavailable() {
        let (logs, _guard) = Logs::capture();
        let fake = Arc::new(FakeGitlab::default());
        serve_tracked_project(&fake);
        for err in [
            FakeErr::Rejected,
            FakeErr::Rejected,
            FakeErr::RejectedWith(400),
            FakeErr::Rejected,
            FakeErr::RejectedWith(400),
            FakeErr::RejectedWith(422),
            FakeErr::RejectedWith(400),
        ] {
            fake.fail_next("projects/7/boards", err);
        }
        let job = Job::ProjectBoards(7);
        let env = start(connected(&fake, 1));

        first_failure(&env, job).await;
        rerun(&env, job).await;
        assert_eq!(state(&env, job).rejections, 2);
        rerun(&env, job).await;
        assert_eq!(state(&env, job).rejections, 0, "the 400 starts over");
        rerun(&env, job).await;
        assert_eq!(state(&env, job).rejections, 1);
        for _ in 0..3 {
            rerun(&env, job).await;
            let s = state(&env, job);
            assert_eq!(s.rejections, 0, "{s:?}");
            assert!(s.retry_at <= now_secs() + REJECTED_BACKOFF_CAP, "{s:?}");
            assert!(!info(&env, job).await.unavailable);
        }
        assert_eq!(state(&env, job).failures, 7);
        assert_eq!(logs.count("WARN", &job.key()), 7, "{}", logs.text());
    }

    /// Make project 7's boards unavailable: three 403s in a row.
    async fn boards_unavailable(fake: &Arc<FakeGitlab>) -> Env {
        serve_tracked_project(fake);
        for _ in 0..3 {
            fake.fail_next("projects/7/boards", FakeErr::Rejected);
        }
        let env = start(connected(fake, 1));
        let job = Job::ProjectBoards(7);
        first_failure(&env, job).await;
        rerun(&env, job).await;
        rerun(&env, job).await;
        assert!(info(&env, job).await.unavailable);
        env
    }

    #[tokio::test]
    async fn a_success_makes_an_unavailable_job_available_again() {
        let fake = Arc::new(FakeGitlab::default());
        let env = boards_unavailable(&fake).await;
        let job = Job::ProjectBoards(7);

        // The feature is back: the next run (a day later, or demanded) lands.
        rerun(&env, job).await;
        let served = state(&env, job);
        assert!(served.last_ok > 0, "{served:?}");
        assert_eq!(
            (served.failures, served.rejections, served.retry_at),
            (0, 0, 0)
        );
        let available = info(&env, job).await;
        assert!(!available.unavailable);
        assert_eq!(available.status, JobStatus::Waiting);
        assert_eq!((available.failures, available.last_error), (0, None));
    }

    /// `forskap auth login` clears every backoff: a new token may see what
    /// the old one couldn't, so the refusals start over too.
    #[tokio::test]
    async fn a_login_makes_an_unavailable_job_available_again() {
        let fake = Arc::new(FakeGitlab::default());
        let env = boards_unavailable(&fake).await;
        let job = Job::ProjectBoards(7);

        env.sync.logged_in();
        eventually("the rerun", || state(&env, job).last_ok > 0).await;
        let s = state(&env, job);
        assert_eq!((s.failures, s.rejections, s.retry_at), (0, 0, 0));
        assert!(!info(&env, job).await.unavailable);
        assert_eq!(fake.calls_to("projects/7/boards").len(), 4);
    }

    /// A cache clear that drops a job's rows resets its state: what was
    /// unavailable runs at once. The boards go with the assigned lists, the
    /// issues and merge requests with the corpus.
    #[tokio::test]
    async fn a_clear_resetting_an_unavailable_job_runs_it_again() {
        let fake = Arc::new(FakeGitlab::default());
        let env = boards_unavailable(&fake).await;
        let job = Job::ProjectBoards(7);

        // The corpus is not the boards' slice.
        tokio::time::timeout(Duration::from_secs(2), env.sync.clear(Clear::Corpus))
            .await
            .unwrap();
        assert_eq!(state(&env, job).rejections, 3);

        tokio::time::timeout(Duration::from_secs(2), env.sync.clear(Clear::Assigned))
            .await
            .unwrap();
        eventually("the boards to run again", || state(&env, job).last_ok > 0).await;
        let s = state(&env, job);
        assert_eq!((s.failures, s.rejections), (0, 0));
        assert!(!info(&env, job).await.unavailable);
    }

    /// A changed fingerprint (a reload changing `search.max_items_per_project`,
    /// a schema bump) makes the next run full, but never cuts a backoff
    /// short: an unavailable job stays so until its rest is over.
    #[tokio::test]
    async fn a_changed_fingerprint_leaves_an_unavailable_job_resting() {
        let fake = Arc::new(FakeGitlab::default());
        serve_tracked_project(&fake);
        for _ in 0..3 {
            fake.fail_next("projects/7/issues", FakeErr::Rejected);
        }
        let env = start(connected(&fake, 1));
        let job = Job::ProjectIssues(7);
        first_failure(&env, job).await;
        rerun(&env, job).await;
        rerun(&env, job).await;
        let resting = state(&env, job);
        assert!(info(&env, job).await.unavailable);

        env.config.write().unwrap().search.max_items_per_project += 1;
        env.sync.reconfigure();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(state(&env, job), resting);
        let after = info(&env, job).await;
        assert!(after.unavailable);
        assert_eq!(after.next_due, Some(resting.retry_at));
        assert_eq!(fake.calls_to("projects/7/issues").len(), 3);
    }

    /// Job states persist, the refusal count with them: after a restart an
    /// unavailable job stays one, and its next refusal stays quiet.
    #[tokio::test]
    async fn an_unavailable_job_stays_quiet_after_a_restart() {
        let (logs, _guard) = Logs::capture();
        let (store, _dir) = open_store();
        let fake = Arc::new(FakeGitlab::default());
        serve_tracked_project(&fake);
        for _ in 0..3 {
            fake.fail_next("projects/7/boards", FakeErr::Rejected);
        }
        let job = Job::ProjectBoards(7);
        let first = start_on(Arc::clone(&store), connected(&fake, 1));
        first_failure(&first, job).await;
        rerun(&first, job).await;
        rerun(&first, job).await;
        let before = state(&first, job);
        assert_eq!(before.rejections, 3);
        drop(first);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let warned = logs.count("WARN", &job.key());

        let again = Arc::new(FakeGitlab::default());
        serve_tracked_project(&again);
        again.fail_next("projects/7/boards", FakeErr::Rejected);
        let second = start_on(store, connected(&again, 1));
        let restarted = snapshot_when(&second, "the plan after the restart", |s| {
            s.jobs.iter().any(|j| j.key == job.key())
        })
        .await;
        let line = restarted.jobs.iter().find(|j| j.key == job.key()).unwrap();
        assert!(line.unavailable, "{line:?}");
        assert_eq!(line.status, JobStatus::Waiting);
        assert_eq!(line.next_due, Some(before.retry_at));
        // A day of rest without a reason helps nobody: it is stored too.
        let refused = line.last_error.as_deref().unwrap_or_default();
        assert!(refused.contains("403"), "{line:?}");
        assert!(
            again.calls_to("projects/7/boards").is_empty(),
            "still resting"
        );

        rerun(&second, job).await;
        assert_eq!(state(&second, job).rejections, 4);
        assert_eq!(logs.count("WARN", &job.key()), warned, "{}", logs.text());
        assert!(info(&second, job).await.unavailable);
    }

    /// A job backing off says why also after a restart, until it runs
    /// through: the error is stored with the backoff it explains.
    #[tokio::test]
    async fn a_failed_jobs_reason_outlives_a_restart() {
        let (store, _dir) = open_store();
        let fake = Arc::new(FakeGitlab::default());
        fake.fail_next("merge_requests", FakeErr::Throttled(503));
        let job = Job::AssignedMergeRequests;
        let first = start_on(Arc::clone(&store), connected(&fake, 1));
        let failed = failed(&first, job).await;
        let before = failed.jobs.iter().find(|j| j.key == job.key()).unwrap();
        let why = before.last_error.clone().unwrap();
        assert!(why.contains("503"), "{why}");
        drop(first);
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Dormant, so the job can't run and lose its error that way.
        let dormant = ConnState::Dormant(DormancyReason::NoCredentials);
        let second = start_on(Arc::clone(&store), dormant);
        let restarted = info(&second, job).await;
        assert_eq!(restarted.status, JobStatus::BackingOff);
        assert_eq!((restarted.failures, restarted.last_error), (1, Some(why)));
        drop(second);
        tokio::time::sleep(Duration::from_millis(50)).await;

        // A run that succeeds takes the stored error with it.
        let third = start_on(store, connected(&fake, 1));
        rerun(&third, job).await;
        assert_eq!(info(&third, job).await.last_error, None);
        assert!(third.store.job_errors().unwrap().is_empty());
    }

    /// GitLab refusing an account-wide listing means something is wrong
    /// with the session: it backs off and warns every time, and is never
    /// unavailable.
    #[tokio::test]
    async fn an_account_wide_listing_never_becomes_unavailable() {
        let (logs, _guard) = Logs::capture();
        let fake = Arc::new(FakeGitlab::default());
        for _ in 0..4 {
            fake.fail_next("merge_requests", FakeErr::Rejected);
        }
        let job = Job::AssignedMergeRequests;
        let env = start(connected(&fake, 1));

        first_failure(&env, job).await;
        for _ in 0..3 {
            rerun(&env, job).await;
        }
        let s = state(&env, job);
        assert_eq!((s.failures, s.rejections), (4, 0));
        assert!(s.retry_at <= now_secs() + REJECTED_BACKOFF_CAP, "{s:?}");
        let line = info(&env, job).await;
        assert!(!line.unavailable);
        assert_eq!(line.status, JobStatus::BackingOff);
        assert_eq!(logs.count("WARN", &job.key()), 4, "{}", logs.text());
    }

    /// A project with its merge requests switched off gets no merge request
    /// job, so GitLab is never asked; switching them back on brings the job
    /// back with the next member listing.
    #[tokio::test]
    async fn a_project_without_merge_requests_is_never_asked_for_them() {
        let fake = Arc::new(FakeGitlab::default());
        fake.serve("events", vec![event_json(1, 7, "opened", now_secs())]);
        fake.serve(
            "projects",
            vec![crate::testing::project_json_without(7, "merge_requests")],
        );
        let env = start(connected(&fake, 1));

        eventually("the project's issues", || {
            state(&env, Job::ProjectIssues(7)).last_ok > 0
                && state(&env, Job::ProjectBoards(7)).last_ok > 0
        })
        .await;
        let planned = env.sync.jobs().await;
        let mrs = Job::ProjectMergeRequests(7).key();
        assert!(!planned.jobs.iter().any(|j| j.key == mrs), "{planned:?}");
        assert!(fake.calls_to("projects/7/merge_requests").is_empty());

        fake.serve("projects", vec![project_json(7)]);
        rerun(&env, Job::MemberProjects).await;
        eventually("the merge requests", || {
            state(&env, Job::ProjectMergeRequests(7)).last_ok > 0
        })
        .await;
        assert!(!fake.calls_to("projects/7/merge_requests").is_empty());
    }

    #[tokio::test]
    async fn a_clear_cancels_the_fetch_in_flight() {
        let fake = Arc::new(FakeGitlab::default());
        fake.serve("events", vec![event_json(1, 7, "opened", now_secs())]);
        fake.serve("projects", vec![project_json(7)]);
        fake.serve("projects/7/issues", vec![issue_json(7, 1, "late")]);
        let gate = fake.gate("projects/7/issues");
        let env = start(connected(&fake, 1));

        tokio::time::timeout(Duration::from_secs(2), fake.gated.notified())
            .await
            .expect("the project fetch starts");
        // Nothing re-plans project 7 after the wipe.
        fake.serve("events", Vec::new());
        fake.serve("projects", Vec::new());
        tokio::time::timeout(Duration::from_secs(2), env.sync.clear(Clear::Everything))
            .await
            .expect("the clear doesn't wait for the fetch");
        gate.notify_one();
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert!(env.store.issues.get((7, 1)).unwrap().is_none());
        assert!(env.store.events.scan(RowScope::All).unwrap().is_empty());
    }

    /// Clearing the assigned lists drops the evidence tracking project 7
    /// until the refill lands; its corpus must not go with it.
    #[tokio::test]
    async fn a_scoped_clear_keeps_the_corpus_while_its_evidence_refills() {
        let (store, _dir) = open_store();
        seed_assigned_project(&store);
        let fake = Arc::new(FakeGitlab::default());
        fake.serve("issues", vec![issue_json(7, 1, "one")]);
        let env = start_on(store, connected(&fake, 1));

        let cleared = env.sync.clear(Clear::Assigned);
        let refilled = env
            .sync
            .refresh_now(&[Job::AssignedIssues, Job::AssignedMergeRequests]);
        cleared.await;
        refilled.await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert!(env.store.issues.get((7, 2)).unwrap().is_some());
        assert!(state(&env, Job::ProjectIssues(7)).last_ok > 0);
        assert!(fake.calls_to("projects/7/issues").is_empty(), "no refetch");
    }

    /// A job demanded before a replan dropped it doesn't run: its state
    /// would outlive the plan.
    #[tokio::test]
    async fn a_demand_the_plan_dropped_never_runs() {
        let (store, dir) = open_store();
        seed_assigned_project(&store);
        let fake = Arc::new(FakeGitlab::default());
        // One at a time, so the replan comes before the dropped job's turn.
        let env = start_flying(1, store, &dir, connected(&fake, 1));

        // The refreshed view no longer lists project 7, which untracks it.
        env.sync
            .refresh_now(&[Job::AssignedIssues, Job::ProjectIssues(7)])
            .await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert!(fake.calls_to("projects/7/issues").is_empty());
        assert_eq!(state(&env, Job::ProjectIssues(7)), JobState::default());
        assert!(env.store.issues.get((7, 2)).unwrap().is_none());
    }

    /// States of jobs that left the plan while the daemon was down go with
    /// their rows, so a project tracked again starts with a full fetch.
    #[tokio::test]
    async fn a_boot_drops_the_states_of_unplanned_jobs() {
        let (store, _dir) = open_store();
        mark_synced(&store, &BASE);
        mark_synced(&store, &[Job::ProjectIssues(9), Job::ProjectBoards(9)]);
        let mut c = store.begin();
        c.upsert(&[issue_row(9, 1)]).unwrap();
        c.commit().unwrap();

        let fake = Arc::new(FakeGitlab::default());
        let env = start_on(store, connected(&fake, 1));
        eventually("the orphaned states to go", || {
            state(&env, Job::ProjectIssues(9)) == JobState::default()
                && state(&env, Job::ProjectBoards(9)) == JobState::default()
        })
        .await;
        assert!(env.store.issues.get((9, 1)).unwrap().is_none());
    }

    /// A history that never synced runs before the corpus backlog, not
    /// after it.
    #[tokio::test]
    async fn the_first_full_history_runs_ahead_of_the_corpus() {
        let (store, _dir) = open_store();
        let mut synced: Vec<Job> = BASE.to_vec();
        synced.retain(|j| *j != Job::AllTimelogs);
        mark_synced(&store, &synced);
        let mut c = store.begin();
        c.upsert(&[Project {
            id: 7,
            ..Default::default()
        }])
        .unwrap();
        c.commit().unwrap();
        let fake = Arc::new(FakeGitlab::default());
        let mut c = store.begin();
        let events: Vec<crate::sync::model::Event> =
            vec![serde_json::from_value(event_json(1, 7, "opened", now_secs())).unwrap()];
        c.upsert(&events).unwrap();
        c.commit().unwrap();
        // The corpus stalls on its first job.
        let _gate = fake.gate("projects/7/boards");
        let _env = start_on(store, connected(&fake, 1));

        eventually("the full history fetch", || {
            !fake.timelog_calls().is_empty()
        })
        .await;
    }

    /// A clear of another slice leaves the fetch in flight alone.
    #[tokio::test]
    async fn a_clear_elsewhere_lets_the_fetch_land() {
        let (store, _dir) = open_store();
        seed_assigned_project(&store);
        let fake = Arc::new(FakeGitlab::default());
        fake.serve("projects/7/issues", vec![issue_json(7, 3, "new")]);
        let gate = fake.gate("projects/7/issues");
        let env = start_on(store, connected(&fake, 1));

        let refreshed = env.sync.refresh_now(&[Job::ProjectIssues(7)]);
        tokio::time::timeout(Duration::from_secs(2), fake.gated.notified())
            .await
            .expect("the project fetch starts");
        env.sync
            .clear(Clear::Timelogs {
                from: 0,
                until: u64::MAX,
            })
            .await;
        gate.notify_one();
        refreshed.await;
        assert!(env.store.issues.get((7, 3)).unwrap().is_some());
    }

    /// A demanded fetch the clear voids reruns on the cleared store before
    /// its waiters are released.
    #[tokio::test]
    async fn a_cancelled_demand_reruns() {
        let (store, _dir) = open_store();
        seed_assigned_project(&store);
        let fake = Arc::new(FakeGitlab::default());
        let gate = fake.gate("projects/7/issues");
        let env = start_on(store, connected(&fake, 1));

        let refreshed = env.sync.refresh_now(&[Job::ProjectIssues(7)]);
        tokio::time::timeout(Duration::from_secs(2), fake.gated.notified())
            .await
            .expect("the project fetch starts");
        env.sync.clear(Clear::Corpus).await;
        tokio::time::timeout(Duration::from_secs(2), refreshed)
            .await
            .expect("the rerun lands");
        gate.notify_one();
        assert!(fake.calls_to("projects/7/issues").len() >= 2);
    }

    /// A project joined since the daily member listing reruns it, so the
    /// project shows up in search without waiting a day.
    #[tokio::test]
    async fn joining_a_project_relists_the_memberships() {
        let (store, _dir) = open_store();
        let mut synced: Vec<Job> = BASE.to_vec();
        synced.retain(|j| *j != Job::Events);
        mark_synced(&store, &synced);
        let fake = Arc::new(FakeGitlab::default());
        fake.serve("events", vec![event_json(1, 5, "joined", now_secs())]);
        fake.serve("projects", vec![project_json(5)]);
        let env = start_on(store, connected(&fake, 1));

        eventually("the new membership", || {
            env.store.projects.get((5, 0)).unwrap().is_some()
        })
        .await;
    }

    /// An assigned issue that left the view in a project without a corpus
    /// goes, instead of lingering in search with its old state.
    #[tokio::test]
    async fn a_row_leaving_the_view_goes_without_a_corpus() {
        let (store, _dir) = open_store();
        mark_synced(&store, &BASE);
        let mut c = store.begin();
        c.upsert(&[issue_row(9, 1), issue_row(9, 2)]).unwrap();
        c.set_view(
            ASSIGNED_ISSUES,
            &crate::sync::store::View {
                keys: vec![(9, 1), (9, 2)],
                fetched_at: now_secs(),
            },
        )
        .unwrap();
        c.commit().unwrap();
        let fake = Arc::new(FakeGitlab::default());
        // #2 keeps project 9 assigned, so the plan doesn't change.
        fake.serve("issues", vec![issue_json(9, 2, "two")]);
        let env = start_on(store, connected(&fake, 1));

        env.sync.refresh_now(&[Job::AssignedIssues]).await;
        assert!(env.store.issues.get((9, 1)).unwrap().is_none());
        assert!(env.store.issues.get((9, 2)).unwrap().is_some());
    }

    /// An assigned issue that closed leaves the assigned view, but not the
    /// recent one: its row stays for that list to update, until it ages out
    /// there too.
    #[tokio::test]
    async fn a_closed_assigned_issue_stays_while_the_recent_list_names_it() {
        let (store, _dir) = open_store();
        mark_synced(&store, &BASE);
        let mut c = store.begin();
        c.upsert(&[issue_row(9, 1), issue_row(9, 2)]).unwrap();
        for name in [ASSIGNED_ISSUES, RECENT_ASSIGNED_ISSUES] {
            let listed = crate::sync::store::View {
                keys: vec![(9, 1), (9, 2)],
                fetched_at: now_secs(),
            };
            c.set_view(name, &listed).unwrap();
        }
        c.commit().unwrap();
        let fake = Arc::new(FakeGitlab::default());
        fake.serve("issues", vec![issue_json(9, 2, "two")]);
        let mut closed = issue_json(9, 1, "one");
        closed["state"] = "closed".into();
        fake.serve(RECENT_ASSIGNED_PATH, vec![closed, issue_json(9, 2, "two")]);
        let env = start_on(store, connected(&fake, 1));
        let state_of = |iid| {
            let row = env.store.issues.get((9, iid)).unwrap();
            row.map(|i: Issue| i.state)
        };

        env.sync.refresh_now(&[Job::AssignedIssues]).await;
        assert_eq!(state_of(1).as_deref(), Some("opened"), "as last seen");
        env.sync.refresh_now(&[Job::RecentAssignedIssues]).await;
        assert_eq!(state_of(1).as_deref(), Some("closed"));

        fake.serve(RECENT_ASSIGNED_PATH, vec![issue_json(9, 2, "two")]);
        env.sync.refresh_now(&[Job::RecentAssignedIssues]).await;
        assert_eq!(state_of(1), None, "no view names it any more");
        assert_eq!(state_of(2).as_deref(), Some("opened"));
    }

    /// The recent lists go with the assigned ones: both are "my issues".
    #[tokio::test]
    async fn clearing_the_assigned_lists_drops_the_recent_views_and_refetches_them() {
        let (store, _dir) = open_store();
        mark_synced(&store, &BASE);
        let mut c = store.begin();
        for name in [RECENT_AUTHORED_ISSUES, RECENT_ASSIGNED_ISSUES] {
            c.set_view(name, &Default::default()).unwrap();
        }
        c.commit().unwrap();
        let fake = Arc::new(FakeGitlab::default());
        // The assigned list runs first and holds the lane of all three.
        let gate = fake.gate("issues");
        let env = start_on(store, connected(&fake, 1));

        env.sync.clear(Clear::Assigned).await;
        for name in [RECENT_AUTHORED_ISSUES, RECENT_ASSIGNED_ISSUES] {
            assert!(env.store.view(name).unwrap().is_none(), "{name}");
        }
        gate.notify_one();
        eventually("both lists again", || {
            [Job::RecentAuthoredIssues, Job::RecentAssignedIssues]
                .iter()
                .all(|job| state(&env, *job).last_ok > 0)
        })
        .await;
        assert!(env.store.view(RECENT_ASSIGNED_ISSUES).unwrap().is_some());
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

    /// A clear and the refill requested with it are queued together, so the
    /// refill isn't preceded by a scheduled run of the same job.
    #[tokio::test]
    async fn a_clear_and_its_refill_run_each_job_once() {
        let fake = Arc::new(FakeGitlab::default());
        let env = start(connected(&fake, 1));
        env.sync.refresh_now(&BASE).await;
        let before = fake.calls_to("issues").len();

        let cleared = env.sync.clear(Clear::Everything);
        let refilled = env.sync.refresh_now(Clear::Everything.refill());
        cleared.await;
        refilled.await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(fake.calls_to("issues").len(), before + 1);
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

    /// A restart before the view catches up must not bring a closed issue
    /// back.
    #[tokio::test]
    async fn noted_writes_survive_a_restart() {
        let (store, _dir) = open_store();
        let dormant = || ConnState::Dormant(DormancyReason::NoCredentials);
        let close = Write {
            kind: Issuable::Issue,
            project_id: 7,
            iid: 1,
            op: WriteOp::Close,
        };
        let before = now_secs();
        let first = start_on(Arc::clone(&store), dormant());
        first.sync.note_write(&close);
        eventually("the note to persist", || {
            !store.noted.scan(RowScope::All).unwrap().is_empty()
        })
        .await;
        drop(first);

        let second = start_on(store, dormant());
        assert_eq!(second.sync.writes_since(before), [close]);
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

    /// Members 7 (with an avatar) and 8 (without), listed just now.
    fn seed_avatar_project(store: &SyncStore, url: &str) {
        let mut c = store.begin();
        c.upsert(&[
            Project {
                id: 7,
                avatar_url: url.into(),
                ..Default::default()
            },
            Project {
                id: 8,
                ..Default::default()
            },
        ])
        .unwrap();
        c.commit().unwrap();
        mark_synced(store, &BASE);
    }

    fn avatar_file(env: &Env, project: u64) -> Option<String> {
        let row = env.store.avatars.get((project, 0)).unwrap();
        row.map(|a| a.file)
    }

    fn files(dir: &AvatarDir) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(dir.path_of("")) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// The avatar is downloaded on its own, once: neither time nor a
    /// restart fetches it again.
    #[tokio::test]
    async fn an_avatar_is_fetched_once_per_url() {
        let (store, dir) = open_store();
        seed_avatar_project(&store, "https://gl/a.png");
        let fake = Arc::new(FakeGitlab::default());
        fake.serve_avatar(7, PNG);
        let first = start_with_avatars(Arc::clone(&store), &dir, connected(&fake, 1));

        eventually("the avatar", || avatar_file(&first, 7).is_some()).await;
        let file = avatar_file(&first, 7).unwrap();
        assert_eq!(files(&first.avatars), std::slice::from_ref(&file));
        assert_eq!(state(&first, Job::ProjectAvatar(8)), JobState::default());
        drop(first);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let second = start_with_avatars(store, &dir, connected(&fake, 1));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(fake.avatar_calls(), [7], "{:?}", fake.calls());
        assert_eq!(files(&avatar_dir(&dir)), [file]);
        drop(second);
    }

    /// The URL [`project_json_with_avatar`] gives project `id`'s avatar.
    fn avatar_url(id: i64, file: &str) -> String {
        let project = project_json_with_avatar(id, file);
        project["avatar_url"].as_str().unwrap().into()
    }

    /// Set `path`'s modification time to a fixed old one, and return it.
    fn age(path: &std::path::Path) -> std::time::SystemTime {
        let old = std::time::UNIX_EPOCH + Duration::from_secs(1_000);
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_modified(old).unwrap();
        old
    }

    /// A new image under a new URL gets a new file; the old one goes once
    /// the new row landed.
    #[tokio::test]
    async fn a_new_image_replaces_the_file() {
        let (store, dir) = open_store();
        seed_avatar_project(&store, "https://gl/a.png");
        let fake = Arc::new(FakeGitlab::default());
        fake.serve_avatar(7, PNG);
        fake.serve("projects", vec![project_json_with_avatar(7, "b.png")]);
        let env = start_with_avatars(store, &dir, connected(&fake, 1));
        eventually("the avatar", || avatar_file(&env, 7).is_some()).await;
        let old = avatar_file(&env, 7).unwrap();

        // Same format, other bytes: only the content tells the names apart.
        let other = [PNG, b"\0"].concat();
        fake.serve_avatar(7, &other);
        env.sync.refresh_now(&[Job::MemberProjects]).await;
        eventually("the new avatar", || {
            avatar_file(&env, 7).is_some_and(|f| f != old)
        })
        .await;
        let new = avatar_file(&env, 7).unwrap();
        assert!(new.ends_with(".png"), "{new}");
        assert_eq!(std::fs::read(env.avatars.path_of(&new)).unwrap(), other);
        assert_eq!(files(&env.avatars), [new]);
        assert_eq!(fake.avatar_calls(), [7, 7]);
    }

    /// GitLab's avatar URL ends in `?v=<updated_at>`, so any update of the
    /// project changes it. The image is downloaded again, but an unchanged
    /// one keeps its path, and its file stays in place, untouched.
    #[tokio::test]
    async fn the_same_image_under_a_new_url_keeps_its_file() {
        let (store, dir) = open_store();
        seed_avatar_project(&store, &avatar_url(7, "a.png?v=1"));
        let fake = Arc::new(FakeGitlab::default());
        fake.serve_avatar(7, PNG);
        fake.serve("projects", vec![project_json_with_avatar(7, "a.png?v=2")]);
        let env = start_with_avatars(store, &dir, connected(&fake, 1));
        eventually("the avatar", || avatar_file(&env, 7).is_some()).await;
        let file = avatar_file(&env, 7).unwrap();
        let path = env.avatars.path_of(&file);
        let old = age(&path);
        let first = state(&env, Job::ProjectAvatar(7));

        // A thread of its own sees the worker mid-commit, a task would not.
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watch = {
            let (stop, path) = (Arc::clone(&stop), path.clone());
            std::thread::spawn(move || {
                let (mut looks, mut misses) = (0, 0);
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    looks += 1;
                    misses += usize::from(!path.is_file());
                    std::thread::sleep(Duration::from_micros(50));
                }
                (looks, misses)
            })
        };
        env.sync.refresh_now(&[Job::MemberProjects]).await;
        eventually("the second download", || {
            state(&env, Job::ProjectAvatar(7)).fingerprint != first.fingerprint
        })
        .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let (looks, misses) = watch.join().unwrap();

        assert_eq!(fake.avatar_calls(), [7, 7], "downloaded again, once");
        assert_eq!(
            misses, 0,
            "the file was missing in {misses} of {looks} looks"
        );
        assert_eq!(avatar_file(&env, 7), Some(file.clone()));
        assert_eq!(files(&env.avatars), [file]);
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(modified, old, "the file was written again");
    }

    /// Two projects with the same image (a group's logo, say) get a file
    /// each: one replacing or dropping its avatar leaves the other's alone.
    #[tokio::test]
    async fn projects_sharing_an_image_keep_a_file_each() {
        let (store, dir) = open_store();
        let mut c = store.begin();
        let project = |id| Project {
            id,
            avatar_url: avatar_url(id, "logo.png"),
            ..Default::default()
        };
        c.upsert(&[project(7), project(8)]).unwrap();
        c.commit().unwrap();
        mark_synced(&store, &BASE);
        let fake = Arc::new(FakeGitlab::default());
        fake.serve_avatar(7, PNG);
        fake.serve_avatar(8, PNG);
        let env = start_with_avatars(store, &dir, connected(&fake, 1));
        eventually("both avatars", || {
            avatar_file(&env, 7).is_some() && avatar_file(&env, 8).is_some()
        })
        .await;
        let seven = avatar_file(&env, 7).unwrap();
        let eight = avatar_file(&env, 8).unwrap();
        assert_ne!(seven, eight);
        let mut both = vec![seven.clone(), eight.clone()];
        both.sort();
        assert_eq!(files(&env.avatars), both);

        fake.serve_avatar(8, b"GIF89a");
        let seven_json = project_json_with_avatar(7, "logo.png");
        fake.serve(
            "projects",
            vec![seven_json.clone(), project_json_with_avatar(8, "new.gif")],
        );
        env.sync.refresh_now(&[Job::MemberProjects]).await;
        eventually("8's new avatar", || {
            avatar_file(&env, 8).is_some_and(|f| f.ends_with(".gif"))
        })
        .await;
        let mut both = vec![seven.clone(), avatar_file(&env, 8).unwrap()];
        both.sort();
        eventually("8's old file to go", || files(&env.avatars) == both).await;

        fake.serve("projects", vec![seven_json, project_json(8)]);
        env.sync.refresh_now(&[Job::MemberProjects]).await;
        eventually("8's avatar to go", || avatar_file(&env, 8).is_none()).await;
        eventually("8's file to go", || {
            files(&env.avatars) == std::slice::from_ref(&seven)
        })
        .await;
        assert_eq!(std::fs::read(env.avatars.path_of(&seven)).unwrap(), PNG);
        assert_eq!(avatar_file(&env, 7), Some(seven));
    }

    /// Before, a file was named by the job's fingerprint. An upgrade
    /// downloads nothing: such a file keeps its name until its project's
    /// next download, which moves it to its content name and leaves nothing
    /// of the old one behind.
    #[tokio::test]
    async fn a_file_named_the_old_way_moves_at_its_next_download() {
        let (store, dir) = open_store();
        seed_avatar_project(&store, &avatar_url(7, "a.png?v=1"));
        mark_synced(&store, &[Job::ProjectAvatar(7)]);
        let synced = store.job_state(&Job::ProjectAvatar(7).key()).unwrap();
        let legacy = format!("7-{:016x}.png", synced.fingerprint);
        let mut c = store.begin();
        c.upsert(&[Avatar {
            project_id: 7,
            file: legacy.clone(),
        }])
        .unwrap();
        c.commit().unwrap();
        avatar_dir(&dir).write(&legacy, PNG).unwrap();

        let fake = Arc::new(FakeGitlab::default());
        fake.serve_avatar(7, PNG);
        fake.serve("projects", vec![project_json_with_avatar(7, "a.png?v=2")]);
        let env = start_with_avatars(store, &dir, connected(&fake, 1));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(fake.avatar_calls().is_empty(), "{:?}", fake.calls());
        assert_eq!(avatar_file(&env, 7).as_ref(), Some(&legacy));
        assert_eq!(files(&env.avatars), [legacy]);

        env.sync.refresh_now(&[Job::MemberProjects]).await;
        let named = crate::sync::avatars::file_name(7, PNG).unwrap();
        eventually("the content name", || {
            avatar_file(&env, 7).as_ref() == Some(&named)
        })
        .await;
        eventually("the old file to go", || {
            files(&env.avatars) == std::slice::from_ref(&named)
        })
        .await;
        assert_eq!(fake.avatar_calls(), [7]);
    }

    /// GitLab before 16.9 answers 404: recorded as "none", not retried.
    #[tokio::test]
    async fn a_missing_avatar_is_no_failure() {
        let (store, dir) = open_store();
        seed_avatar_project(&store, "https://gl/a.png");
        let fake = Arc::new(FakeGitlab::default());
        let env = start_with_avatars(store, &dir, connected(&fake, 1));

        eventually("the avatar job", || avatar_file(&env, 7).is_some()).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(avatar_file(&env, 7).unwrap(), "");
        let ran = state(&env, Job::ProjectAvatar(7));
        assert_eq!((ran.failures, ran.retry_at), (0, 0));
        assert_eq!(fake.avatar_calls(), [7]);
    }

    /// An avatar never runs ahead of data that is due.
    #[tokio::test]
    async fn avatars_wait_for_everything_else() {
        let (store, dir) = open_store();
        seed_avatar_project(&store, "https://gl/a.png");
        let mut c = store.begin();
        c.remove_job(&Job::MemberGroups.key());
        c.commit().unwrap();
        let fake = Arc::new(FakeGitlab::default());
        let gate = fake.gate("groups");
        // One at a time: a second slot would be free for the avatar.
        let env = start_flying(1, store, &dir, connected(&fake, 1));

        tokio::time::timeout(Duration::from_secs(2), fake.gated.notified())
            .await
            .expect("the group listing starts");
        assert!(fake.avatar_calls().is_empty());
        gate.notify_one();
        eventually("the avatar", || avatar_file(&env, 7).is_some()).await;
    }

    #[tokio::test]
    async fn a_project_losing_its_avatar_loses_the_file() {
        let (store, dir) = open_store();
        seed_avatar_project(&store, "https://gl/a.png");
        let fake = Arc::new(FakeGitlab::default());
        fake.serve_avatar(7, PNG);
        fake.serve("projects", vec![project_json(7)]);
        let env = start_with_avatars(store, &dir, connected(&fake, 1));
        eventually("the avatar", || !files(&env.avatars).is_empty()).await;

        env.sync.refresh_now(&[Job::MemberProjects]).await;
        eventually("the avatar to go", || avatar_file(&env, 7).is_none()).await;
        eventually("its file to go", || files(&env.avatars).is_empty()).await;
        assert_eq!(state(&env, Job::ProjectAvatar(7)), JobState::default());
    }

    #[tokio::test]
    async fn clearing_the_corpus_drops_the_avatars_and_fetches_them_again() {
        let (store, dir) = open_store();
        seed_avatar_project(&store, "https://gl/a.png");
        let fake = Arc::new(FakeGitlab::default());
        fake.serve_avatar(7, PNG);
        let env = start_with_avatars(store, &dir, connected(&fake, 1));
        eventually("the avatar", || !files(&env.avatars).is_empty()).await;

        // The refilled listing has no member left.
        env.sync.clear(Clear::Corpus).await;
        assert_eq!(avatar_file(&env, 7), None);
        assert!(files(&env.avatars).is_empty());

        fake.serve("projects", vec![project_json_with_avatar(7, "a.png")]);
        env.sync.clear(Clear::Everything).await;
        eventually("the avatar again", || !files(&env.avatars).is_empty()).await;
    }

    /// `~/.cache` may be wiped at any time: the files come back.
    #[tokio::test]
    async fn a_boot_fetches_avatars_whose_file_is_gone() {
        let (store, dir) = open_store();
        seed_avatar_project(&store, "https://gl/a.png");
        mark_synced(&store, &[Job::ProjectAvatar(7)]);
        let mut c = store.begin();
        c.upsert(&[Avatar {
            project_id: 7,
            file: "7-1.png".into(),
        }])
        .unwrap();
        c.commit().unwrap();
        avatar_dir(&dir).write("9-1.png", PNG).unwrap();

        let fake = Arc::new(FakeGitlab::default());
        fake.serve_avatar(7, PNG);
        let env = start_with_avatars(store, &dir, connected(&fake, 1));
        eventually("the avatar", || {
            avatar_file(&env, 7).is_some_and(|f| f != "7-1.png")
        })
        .await;
        assert_eq!(files(&env.avatars), [avatar_file(&env, 7).unwrap()]);
    }

    /// Wait until `path` was read `n` times.
    async fn called(fake: &FakeGitlab, path: &str, n: usize) {
        eventually(&format!("read {n} of {path}"), || {
            fake.calls_to(path).len() == n
        })
        .await;
    }

    /// The first snapshot `cond` holds for.
    async fn snapshot_when(env: &Env, what: &str, cond: impl Fn(&Snapshot) -> bool) -> Snapshot {
        for _ in 0..200 {
            let snapshot = env.sync.jobs().await;
            if cond(&snapshot) {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {what}");
    }

    /// A snapshot once `job`'s last run is known to have failed.
    async fn failed(env: &Env, job: Job) -> Snapshot {
        let key = job.key();
        let failed = |s: &Snapshot| {
            let job = s.jobs.iter().find(|j| j.key == key);
            job.is_some_and(|j| j.last_error.is_some())
        };
        snapshot_when(env, &format!("{key} to fail"), failed).await
    }

    /// Both assigned lists held in flight on a fresh store.
    async fn both_lists_in_flight(fake: &FakeGitlab) {
        called(fake, "issues", 1).await;
        called(fake, "merge_requests", 1).await;
    }

    #[tokio::test]
    async fn fetches_run_side_by_side_up_to_the_bound() {
        let fake = Arc::new(FakeGitlab::default());
        fake.serve("issues", vec![issue_json(9, 1, "one")]);
        let issues = fake.gate("issues");
        let merge_requests = fake.gate("merge_requests");
        let before = now_secs();
        let env = start(connected(&fake, 1));
        both_lists_in_flight(&fake).await;

        let snapshot = env.sync.jobs().await;
        let (running, queued) = snapshot.jobs.split_at(2);
        let keys: Vec<&str> = running.iter().map(|j| j.key.as_str()).collect();
        assert_eq!(keys, [ASSIGNED_ISSUES, ASSIGNED_MERGE_REQUESTS]);
        for job in running {
            assert_eq!(job.status, JobStatus::Running, "{job:?}");
            assert!(job.running_since.is_some_and(|at| at >= before));
            assert_eq!(job.next_due, None);
        }
        for job in queued {
            assert_eq!(job.status, JobStatus::Due, "{job:?}");
            assert_eq!(job.running_since, None);
        }
        assert_eq!(fake.read_calls(), 2, "no third fetch: {:?}", fake.calls());

        // The one that finishes first lands first.
        merge_requests.notify_one();
        eventually("the merge requests", || {
            state(&env, Job::AssignedMergeRequests).last_ok > 0
        })
        .await;
        assert_eq!(state(&env, Job::AssignedIssues), JobState::default());
        issues.notify_one();
        eventually("the issues", || {
            env.store.issues.get((9, 1)).unwrap().is_some()
        })
        .await;
    }

    /// Member projects 7 and 8 with activity, nothing of theirs synced yet.
    fn seed_two_projects(store: &SyncStore) {
        let now = now_secs();
        let projects = [7, 8].map(|id| Project {
            id,
            ..Default::default()
        });
        let events: Vec<crate::sync::model::Event> = [7, 8]
            .iter()
            .map(|&p| serde_json::from_value(event_json(p, p, "opened", now)).unwrap())
            .collect();
        let mut c = store.begin();
        c.upsert(&projects).unwrap();
        c.upsert(&events).unwrap();
        c.commit().unwrap();
        mark_synced(store, &BASE);
    }

    #[tokio::test]
    async fn a_project_has_one_fetch_in_flight_however_many_slots_are_free() {
        let (store, dir) = open_store();
        seed_two_projects(&store);
        let fake = Arc::new(FakeGitlab::default());
        let gate = fake.gate("projects/7/boards");
        let env = start_flying(4, store, &dir, connected(&fake, 1));

        // Project 8 gets through all its jobs next to the held fetch.
        eventually("project 8", || {
            [
                Job::ProjectBoards(8),
                Job::ProjectIssues(8),
                Job::ProjectMergeRequests(8),
            ]
            .iter()
            .all(|job| state(&env, *job).last_ok > 0)
        })
        .await;
        assert_eq!(
            info(&env, Job::ProjectBoards(7)).await.status,
            JobStatus::Running
        );
        assert_eq!(
            info(&env, Job::ProjectIssues(7)).await.status,
            JobStatus::Due
        );
        assert!(fake.calls_to("projects/7/issues").is_empty());
        assert!(fake.calls_to("projects/7/merge_requests").is_empty());

        gate.notify_one();
        eventually("project 7", || {
            state(&env, Job::ProjectIssues(7)).last_ok > 0
                && state(&env, Job::ProjectMergeRequests(7)).last_ok > 0
        })
        .await;
    }

    /// One slot, and it is taken: what waits says whether it waits for the
    /// slot or for the very fetch that holds its lane.
    #[tokio::test]
    async fn with_every_slot_taken_the_next_job_waits_for_one() {
        let fake = Arc::new(FakeGitlab::default());
        let _gate = fake.gate("issues");
        let (store, dir) = open_store();
        let env = start_flying(1, store, &dir, connected(&fake, 1));
        called(&fake, "issues", 1).await;
        env.sync.refresh_soon(&[Job::MemberGroups]);

        let demanded = |s: &Snapshot| s.jobs.iter().any(|j| j.status == JobStatus::Demanded);
        let snapshot = snapshot_when(&env, "the demand", demanded).await;
        assert!(snapshot.connected);
        let (running, queued) = snapshot.jobs.split_first().unwrap();
        assert_eq!(
            (running.key.as_str(), running.status, &running.hold),
            (ASSIGNED_ISSUES, JobStatus::Running, &None)
        );
        // The other issue lists store the same rows: they wait for this fetch
        // even with a slot to spare.
        let lists = [Job::RecentAuthoredIssues, Job::RecentAssignedIssues].map(|j| j.key());
        for job in queued {
            let expected = if lists.contains(&job.key) {
                Hold::Lane(ASSIGNED_ISSUES.into())
            } else {
                Hold::Slots
            };
            assert_eq!(job.hold, Some(expected), "{job:?}");
        }
    }

    /// Free slots don't help a job whose lane is taken: it names the fetch
    /// it waits behind.
    #[tokio::test]
    async fn a_job_behind_a_running_one_of_its_lane_names_it() {
        let (store, dir) = open_store();
        seed_two_projects(&store);
        let fake = Arc::new(FakeGitlab::default());
        let _gate = fake.gate("projects/7/boards");
        let env = start_flying(4, store, &dir, connected(&fake, 1));
        called(&fake, "projects/7/boards", 1).await;

        for job in [Job::ProjectIssues(7), Job::ProjectMergeRequests(7)] {
            let held = info(&env, job).await;
            assert_eq!(
                (held.status, held.hold),
                (
                    JobStatus::Due,
                    Some(Hold::Lane(Job::ProjectBoards(7).key()))
                ),
                "{job:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_freed_slot_goes_to_the_demanded_job_first() {
        let fake = Arc::new(FakeGitlab::default());
        let issues = fake.gate("issues");
        let _merge_requests = fake.gate("merge_requests");
        let _groups = fake.gate("groups");
        let env = start(connected(&fake, 1));
        both_lists_in_flight(&fake).await;
        env.sync.refresh_soon(&[Job::MemberGroups]);
        assert_eq!(
            info(&env, Job::MemberGroups).await.status,
            JobStatus::Demanded
        );

        issues.notify_one();
        called(&fake, "groups", 1).await;
        // The recent timelogs and the events are due and rank higher.
        assert!(fake.timelog_calls().is_empty());
        assert_eq!(fake.read_calls(), 3, "{:?}", fake.calls());
    }

    /// Two fetches that ran into one rate limit are one pause, not a
    /// doubled one.
    #[tokio::test]
    async fn a_siblings_rate_limit_is_the_same_pause() {
        let fake = Arc::new(FakeGitlab::default());
        let issues = fake.gate("issues");
        let merge_requests = fake.gate("merge_requests");
        fake.fail_next("issues", FakeErr::Throttled(429));
        fake.fail_next("merge_requests", FakeErr::Throttled(429));
        let env = start(connected(&fake, 1));
        both_lists_in_flight(&fake).await;

        issues.notify_one();
        let paused = |s: &Snapshot| s.paused_until.is_some();
        let first = snapshot_when(&env, "the pause", paused).await;
        merge_requests.notify_one();
        let second = failed(&env, Job::AssignedMergeRequests).await;

        let (first, second) = (first.paused_until.unwrap(), second.paused_until.unwrap());
        assert!(
            second < first + schedule::backoff(1, RATE_LIMIT_PAUSE_CAP) / 2,
            "the second 429 climbed the pause: {first} -> {second}"
        );
        for job in [Job::AssignedIssues, Job::AssignedMergeRequests] {
            let limited = info(&env, job).await;
            assert_eq!((limited.status, limited.failures), (JobStatus::Due, 0));
        }
        assert_eq!(fake.read_calls(), 2, "paused: {:?}", fake.calls());
    }

    /// The pause stops launches; what is already fetched still lands.
    #[tokio::test]
    async fn a_fetch_in_flight_lands_during_the_rate_limit_pause() {
        let fake = Arc::new(FakeGitlab::default());
        let issues = fake.gate("issues");
        let merge_requests = fake.gate("merge_requests");
        fake.fail_next("issues", FakeErr::Throttled(429));
        let env = start(connected(&fake, 1));
        both_lists_in_flight(&fake).await;

        issues.notify_one();
        let paused = |s: &Snapshot| s.paused_until.is_some();
        snapshot_when(&env, "the pause", paused).await;
        merge_requests.notify_one();
        eventually("the merge requests", || {
            state(&env, Job::AssignedMergeRequests).last_ok > 0
        })
        .await;

        let snapshot = env.sync.jobs().await;
        assert!(snapshot.paused_until.is_some());
        assert_eq!(fake.read_calls(), 2, "paused: {:?}", fake.calls());
    }

    /// One outage: the session is demoted once and one job backs off, as
    /// if the second fetch had never started.
    #[tokio::test]
    async fn a_siblings_network_error_is_the_same_outage() {
        let fake = Arc::new(FakeGitlab::default());
        let issues = fake.gate("issues");
        let merge_requests = fake.gate("merge_requests");
        fake.fail_next("issues", FakeErr::Transient);
        fake.fail_next("merge_requests", FakeErr::Transient);
        let env = start(connected(&fake, 1));
        both_lists_in_flight(&fake).await;

        issues.notify_one();
        tokio::time::timeout(Duration::from_secs(2), env.reconnect.notified())
            .await
            .expect("the reconnect supervisor is woken");
        assert_eq!(state(&env, Job::AssignedIssues).failures, 1);

        merge_requests.notify_one();
        failed(&env, Job::AssignedMergeRequests).await;
        assert_eq!(state(&env, Job::AssignedMergeRequests), JobState::default());
        assert!(
            tokio::time::timeout(Duration::from_millis(100), env.reconnect.notified())
                .await
                .is_err(),
            "demoted once"
        );
        assert_eq!(fake.read_calls(), 2, "dormant: {:?}", fake.calls());
    }

    /// A fetch still running on the demoted session fails after the
    /// reconnect: that must not cost the new session.
    #[tokio::test]
    async fn a_siblings_late_failure_leaves_the_new_session_alone() {
        let fake = Arc::new(FakeGitlab::default());
        let issues = fake.gate("issues");
        let merge_requests = fake.gate("merge_requests");
        fake.fail_next("issues", FakeErr::Transient);
        fake.fail_next("merge_requests", FakeErr::Transient);
        let env = start(connected(&fake, 1));
        both_lists_in_flight(&fake).await;
        issues.notify_one();
        tokio::time::timeout(Duration::from_secs(2), env.reconnect.notified())
            .await
            .expect("the reconnect supervisor is woken");

        let fresh = Arc::new(FakeGitlab::default());
        *env.session.write().await = connected(&fresh, 1);
        env.sync.wake();
        merge_requests.notify_one();

        // Its lane frees with the failure; the rerun reads the new session.
        eventually("the merge requests", || {
            state(&env, Job::AssignedMergeRequests).last_ok > 0
        })
        .await;
        assert_eq!(state(&env, Job::AssignedMergeRequests).failures, 0);
        assert_eq!(fresh.calls_to("merge_requests").len(), 1);
        assert!(matches!(
            &*env.session.read().await,
            ConnState::Connected(_)
        ));
    }

    /// One dead token: the keychain is asked once and no job backs off.
    #[tokio::test]
    async fn a_siblings_dead_token_parks_the_session_once() {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

        let fake = Arc::new(FakeGitlab::default());
        let issues = fake.gate("issues");
        let merge_requests = fake.gate("merge_requests");
        fake.fail_next("issues", FakeErr::Unauthorized);
        fake.fail_next("merge_requests", FakeErr::Unauthorized);
        let probes = Arc::new(AtomicUsize::new(0));
        let asked = Arc::clone(&probes);
        let probe: KeychainProbe = Arc::new(move |_| {
            asked.fetch_add(1, SeqCst);
            Box::pin(async { false })
        });
        let (store, _dir) = open_store();
        let env = start_probing(store, connected(&fake, 1), probe);
        both_lists_in_flight(&fake).await;

        issues.notify_one();
        eventually("the token rejection", || {
            matches!(
                env.session.try_read().as_deref(),
                Ok(ConnState::Dormant(DormancyReason::TokenRejected { .. }))
            )
        })
        .await;
        merge_requests.notify_one();
        failed(&env, Job::AssignedMergeRequests).await;

        assert_eq!(probes.load(SeqCst), 1);
        assert_eq!(state(&env, Job::AssignedIssues), JobState::default());
        assert_eq!(state(&env, Job::AssignedMergeRequests), JobState::default());
        assert!(matches!(
            &*env.session.read().await,
            ConnState::Dormant(DormancyReason::TokenRejected { .. })
        ));
    }

    /// Every fetch into the cleared slice is void, and each demanded one
    /// reruns; a fetch elsewhere goes on.
    #[tokio::test]
    async fn a_clear_cancels_every_fetch_into_its_slice() {
        let (store, dir) = open_store();
        mark_synced(&store, &BASE);
        let fake = Arc::new(FakeGitlab::default());
        fake.serve("events", vec![event_json(1, 7, "opened", now_secs())]);
        let _issues = fake.gate("issues");
        let _merge_requests = fake.gate("merge_requests");
        let events = fake.gate("events");
        let env = start_flying(3, store, &dir, connected(&fake, 1));

        let lists = env
            .sync
            .refresh_now(&[Job::AssignedIssues, Job::AssignedMergeRequests]);
        let contributed = env.sync.refresh_now(&[Job::Events]);
        both_lists_in_flight(&fake).await;
        called(&fake, "events", 1).await;

        env.sync.clear(Clear::Assigned).await;
        // The first fetches are still held: only reruns can end the wait.
        tokio::time::timeout(Duration::from_secs(2), lists)
            .await
            .expect("both reruns land");
        assert_eq!(fake.calls_to("issues").len(), 2);
        assert_eq!(fake.calls_to("merge_requests").len(), 2);
        assert_eq!(fake.calls_to("events").len(), 1, "not in the slice");

        events.notify_one();
        tokio::time::timeout(Duration::from_secs(2), contributed)
            .await
            .expect("the fetch outside the slice lands");
        assert_eq!(env.store.events.scan(RowScope::All).unwrap().len(), 1);
    }

    /// A sibling's commit can drop a job from the plan while its fetch is
    /// in flight: nothing of it may land, its rows would outlive the plan.
    #[tokio::test]
    async fn a_fetch_the_plan_dropped_mid_flight_lands_nothing() {
        let (store, dir) = open_store();
        seed_assigned_project(&store);
        let fake = Arc::new(FakeGitlab::default());
        fake.serve("projects/7/issues", vec![issue_json(7, 3, "late")]);
        let gate = fake.gate("projects/7/issues");
        let env = start_flying(2, store, &dir, connected(&fake, 1));

        let corpus = env.sync.refresh_now(&[Job::ProjectIssues(7)]);
        called(&fake, "projects/7/issues", 1).await;
        // The refreshed view no longer lists project 7, which untracks it.
        env.sync.refresh_now(&[Job::AssignedIssues]).await;
        tokio::time::timeout(Duration::from_secs(2), corpus)
            .await
            .expect("whoever waited for the dropped job is let go");
        gate.notify_one();

        let snapshot = env.sync.jobs().await;
        let key = Job::ProjectIssues(7).key();
        assert!(snapshot.jobs.iter().all(|j| j.key != key));
        assert_eq!(state(&env, Job::ProjectIssues(7)), JobState::default());
        assert!(env.store.issues.scan(RowScope::All).unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_panicking_fetch_is_a_failed_job_next_to_a_healthy_one() {
        let fake = Arc::new(FakeGitlab::default());
        let issues = fake.gate("issues");
        let merge_requests = fake.gate("merge_requests");
        fake.fail_next("issues", FakeErr::Panic);
        let env = start(connected(&fake, 1));
        both_lists_in_flight(&fake).await;

        issues.notify_one();
        eventually("the panic to count", || {
            state(&env, Job::AssignedIssues).failures == 1
        })
        .await;
        let panicked = info(&env, Job::AssignedIssues).await;
        assert_eq!(panicked.status, JobStatus::BackingOff);
        assert!(panicked.last_error.is_some_and(|e| e.contains("panicked")));

        merge_requests.notify_one();
        eventually("the merge requests", || {
            state(&env, Job::AssignedMergeRequests).last_ok > 0
        })
        .await;
    }

    #[tokio::test]
    async fn stopping_the_worker_drops_the_fetches_in_flight() {
        let fake = Arc::new(FakeGitlab::default());
        let _issues = fake.gate("issues");
        let _merge_requests = fake.gate("merge_requests");
        let env = start(connected(&fake, 1));
        both_lists_in_flight(&fake).await;

        drop(env);
        // Each fetch holds the client; so did the worker and its session.
        eventually("the fetches to go", || Arc::strong_count(&fake) == 1).await;
    }

    /// A login or rotation swaps the client under a running fetch: what it
    /// read is still the account's data.
    #[tokio::test]
    async fn a_fetch_outliving_its_session_lands_for_the_same_account() {
        let fake = Arc::new(FakeGitlab::default());
        fake.serve("issues", vec![issue_json(9, 1, "one")]);
        let gate = fake.gate("issues");
        let env = start(connected(&fake, 1));
        called(&fake, "issues", 1).await;

        let fresh = Arc::new(FakeGitlab::default());
        *env.session.write().await = connected(&fresh, 1);
        env.sync.wake();
        gate.notify_one();

        eventually("the issue", || {
            env.store.issues.get((9, 1)).unwrap().is_some()
        })
        .await;
        assert!(fresh.calls_to("issues").is_empty(), "no second run");
    }

    /// Another account's login wipes the store; a fetch still running for
    /// the previous one must not land in it afterwards.
    #[tokio::test]
    async fn a_fetch_for_the_previous_account_never_lands() {
        let fake = Arc::new(FakeGitlab::default());
        fake.serve("issues", vec![issue_json(9, 1, "theirs")]);
        let gate = fake.gate("issues");
        let env = start(connected(&fake, 1));
        called(&fake, "issues", 1).await;

        let fresh = Arc::new(FakeGitlab::default());
        *env.session.write().await = connected(&fresh, 2);
        env.sync.wake();
        eventually("the account switch", || {
            let identity = env.store.identity().unwrap();
            identity.is_some_and(|i| i.user_id == 2)
        })
        .await;
        gate.notify_one();

        eventually("the new account's issues", || {
            state(&env, Job::AssignedIssues).last_ok > 0
        })
        .await;
        assert_eq!(fresh.calls_to("issues").len(), 1);
        assert!(env.store.issues.scan(RowScope::All).unwrap().is_empty());
    }

    /// The gap is between launches, so a free slot alone starts nothing.
    #[tokio::test]
    async fn the_gap_holds_the_next_launch_back_until_a_reload_drops_it() {
        let fake = Arc::new(FakeGitlab::default());
        let config = flying(2);
        config.write().unwrap().sync.job_gap_ms = 60_000;
        let (store, dir) = open_store();
        let probe = crate::reconnect::no_keychain_probe();
        let state_ = connected(&fake, 1);
        let env = start_configured(store, avatar_dir(&dir), state_, probe, config);

        eventually("the first job", || {
            state(&env, Job::AssignedIssues).last_ok > 0
        })
        .await;
        let snapshot = env.sync.jobs().await;
        let waiting = &snapshot.jobs[0];
        assert_eq!(waiting.key, ASSIGNED_MERGE_REQUESTS);
        assert_eq!(waiting.status, JobStatus::Due);
        assert_eq!(fake.read_calls(), 1, "{:?}", fake.calls());

        env.config.write().unwrap().sync.job_gap_ms = 0;
        env.sync.reconfigure();
        eventually("the next job", || {
            state(&env, Job::AssignedMergeRequests).last_ok > 0
        })
        .await;
    }

    #[tokio::test]
    async fn a_reload_raising_the_bound_starts_the_next_fetch() {
        let fake = Arc::new(FakeGitlab::default());
        let _issues = fake.gate("issues");
        let _merge_requests = fake.gate("merge_requests");
        let (store, dir) = open_store();
        let env = start_flying(1, store, &dir, connected(&fake, 1));
        called(&fake, "issues", 1).await;
        assert_eq!(
            info(&env, Job::AssignedMergeRequests).await.status,
            JobStatus::Due
        );
        assert!(fake.calls_to("merge_requests").is_empty());

        env.config.write().unwrap().sync.max_in_flight = 2;
        env.sync.reconfigure();
        called(&fake, "merge_requests", 1).await;
        assert_eq!(
            info(&env, Job::AssignedIssues).await.status,
            JobStatus::Running
        );
    }

    /// The board columns of the assigned issues' projects are fetched side
    /// by side; a refresh waits for all of them, whichever finishes last.
    #[tokio::test]
    async fn a_refresh_waits_for_every_board_it_shows() {
        let (store, dir) = open_store();
        mark_synced(&store, &BASE);
        let fake = Arc::new(FakeGitlab::default());
        fake.serve(
            "issues",
            vec![issue_json(7, 1, "one"), issue_json(8, 1, "two")],
        );
        let first = fake.gate("projects/7/boards");
        let last = fake.gate("projects/8/boards");
        let env = start_flying(3, store, &dir, connected(&fake, 1));

        let mut refreshed = Box::pin(env.sync.refresh_now(&[Job::AssignedIssues]));
        called(&fake, "projects/7/boards", 1).await;
        called(&fake, "projects/8/boards", 1).await;
        last.notify_one();
        eventually("project 8's boards", || {
            state(&env, Job::ProjectBoards(8)).last_ok > 0
        })
        .await;
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut refreshed)
                .await
                .is_err(),
            "project 7's boards are still in flight"
        );
        first.notify_one();
        tokio::time::timeout(Duration::from_secs(2), refreshed)
            .await
            .expect("all boards landed");
    }

    /// A worker that runs only demanded jobs, up to three at once: nothing
    /// but the test fills the store.
    fn start_on_demand(store: Arc<SyncStore>, state: ConnState) -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let avatars = AvatarDir::new(tmp.path());
        let session: SessionSlot = Arc::new(tokio::sync::RwLock::new(state));
        let config = flying(3);
        let reconnect = Arc::new(Reconnect::default());
        let sync = SyncHandle::spawn_on_demand(
            Arc::clone(&store),
            avatars.clone(),
            Arc::clone(&session),
            Arc::clone(&config),
            Arc::clone(&reconnect),
        );
        Env {
            sync,
            config,
            session,
            reconnect,
            store,
            avatars,
            _dir: None,
            _avatar_tmp: Some(tmp),
        }
    }

    /// The account [`connected`] logs in as `user_id`.
    fn account(user_id: i64) -> Identity {
        Identity {
            host: "gitlab.test".into(),
            user_id,
        }
    }

    /// An issue as GitLab answers a create with, assigned to `assignees`.
    fn created(project_id: i64, iid: i64, assignees: &[i64]) -> Issue {
        let assignee = |&id| UserRef {
            id,
            ..Default::default()
        };
        Issue {
            assignees: assignees.iter().map(assignee).collect(),
            updated_at: now_secs(),
            ..issue_row(project_id, iid)
        }
    }

    /// Every one of `views` listing `keys`, fetched at `fetched_at`.
    fn seed_views(store: &SyncStore, views: &[&str], keys: &[RowKey], fetched_at: u64) {
        let mut c = store.begin();
        for name in views {
            let view = View {
                keys: keys.to_vec(),
                fetched_at,
            };
            c.set_view(name, &view).unwrap();
        }
        c.commit().unwrap();
    }

    fn view_of(env: &Env, name: &str) -> Option<View> {
        env.store.view(name).unwrap()
    }

    /// The row and its key land together, the key in front: the lists come
    /// newest first. What GitLab assigned decides on the assigned views.
    #[tokio::test]
    async fn a_landed_issue_leads_the_views_it_belongs_to() {
        let (store, _dir) = open_store();
        seed_views(&store, &ISSUE_VIEWS, &[(8, 1)], 500);
        let fake = Arc::new(FakeGitlab::default());
        let env = start_on_demand(store, connected(&fake, 1));
        let keys = |name| view_of(&env, name).unwrap().keys;

        env.sync
            .land_issue(created(9, 5, &[2, 1]), account(1))
            .await;
        assert_eq!(env.store.issues.get((9, 5)).unwrap().unwrap().id, 9005);
        for name in ISSUE_VIEWS {
            let view = view_of(&env, name).unwrap();
            assert_eq!(view.keys, [(9, 5), (8, 1)], "{name}");
            assert_eq!(view.fetched_at, 500, "{name}: still that fetch's view");
        }
        // The assigned view is plan evidence: project 9 shows its boards.
        let snapshot = env.sync.jobs().await;
        let boards = Job::ProjectBoards(9).key();
        assert!(snapshot.jobs.iter().any(|j| j.key == boards));

        // Unassigned, or assigned to someone else only: authored, no more.
        env.sync.land_issue(created(9, 6, &[]), account(1)).await;
        env.sync.land_issue(created(9, 7, &[2]), account(1)).await;
        let authored = [(9, 7), (9, 6), (9, 5), (8, 1)];
        assert_eq!(keys(RECENT_AUTHORED_ISSUES), authored);
        for name in [ASSIGNED_ISSUES, RECENT_ASSIGNED_ISSUES] {
            assert_eq!(keys(name), [(9, 5), (8, 1)], "{name}");
        }

        // A key a list brought in meanwhile is not listed twice.
        env.sync.land_issue(created(9, 6, &[]), account(1)).await;
        assert_eq!(keys(RECENT_AUTHORED_ISSUES), authored);
        assert_eq!(fake.read_calls(), 0, "{:?}", fake.calls());
    }

    /// A list fetched before the create lacks the new issue: landing, it
    /// would replace the view without the key, and the row would go too.
    #[tokio::test]
    async fn a_landing_voids_the_list_fetch_in_flight() {
        let (store, _dir) = open_store();
        seed_views(&store, &ISSUE_VIEWS, &[(9, 1)], 500);
        let fake = Arc::new(FakeGitlab::default());
        // What GitLab lists once the issue exists.
        let listed = vec![issue_json(9, 5, "new"), issue_json(9, 1, "old")];
        fake.serve(RECENT_AUTHORED_PATH, listed);
        let _authored = fake.gate(RECENT_AUTHORED_PATH);
        let merge_requests = fake.gate("merge_requests");
        let env = start_on_demand(store, connected(&fake, 1));

        let relisted = env.sync.refresh_now(&[Job::RecentAuthoredIssues]);
        let elsewhere = env.sync.refresh_now(&[Job::AssignedMergeRequests]);
        called(&fake, RECENT_AUTHORED_PATH, 1).await;
        called(&fake, "merge_requests", 1).await;

        env.sync.land_issue(created(9, 5, &[]), account(1)).await;
        // The first fetch is still held: only a rerun can end the wait.
        tokio::time::timeout(Duration::from_secs(2), relisted)
            .await
            .expect("the rerun lands");
        assert_eq!(fake.calls_to(RECENT_AUTHORED_PATH).len(), 2);
        let view = view_of(&env, RECENT_AUTHORED_ISSUES).unwrap();
        assert_eq!(view.keys, [(9, 5), (9, 1)]);
        assert!(view.fetched_at > 500, "the rerun's view");
        assert!(env.store.issues.get((9, 5)).unwrap().is_some());

        // A fetch in another lane never listed issues: it goes on.
        assert_eq!(fake.calls_to("merge_requests").len(), 1);
        merge_requests.notify_one();
        tokio::time::timeout(Duration::from_secs(2), elsewhere)
            .await
            .expect("the fetch in another lane lands");
        assert_eq!(fake.calls_to("merge_requests").len(), 1);
    }

    /// Its project has no corpus job, so only the view naming it keeps the
    /// row once a changed plan prunes the store.
    #[tokio::test]
    async fn a_landed_issue_survives_a_plan_change_without_a_corpus() {
        let (store, _dir) = open_store();
        seed_views(&store, &ISSUE_VIEWS, &[], 500);
        let fake = Arc::new(FakeGitlab::default());
        fake.serve("events", vec![event_json(1, 7, "opened", now_secs())]);
        fake.serve("projects", vec![project_json(7)]);
        let env = start_on_demand(store, connected(&fake, 1));
        // The boot's own pruning is over.
        env.sync.jobs().await;
        // What the landed row would be without its view: nobody's.
        let mut c = env.store.begin();
        c.upsert(&[issue_row(9, 6)]).unwrap();
        c.commit().unwrap();

        env.sync.land_issue(created(9, 5, &[]), account(1)).await;
        // Activity in member project 7 adds its corpus to the plan.
        env.sync
            .refresh_now(&[Job::Events, Job::MemberProjects])
            .await;
        let snapshot = env.sync.jobs().await;
        let corpus = Job::ProjectIssues(7).key();
        assert!(snapshot.jobs.iter().any(|j| j.key == corpus));
        assert!(env.store.issues.get((9, 6)).unwrap().is_none(), "pruned");
        assert!(env.store.issues.get((9, 5)).unwrap().is_some());
        assert_eq!(
            view_of(&env, RECENT_AUTHORED_ISSUES).unwrap().keys,
            [(9, 5)]
        );
    }

    /// One key is not a list: a view that never synced is not made up, and
    /// its reads stay "not synced yet".
    #[tokio::test]
    async fn a_landing_skips_views_that_never_synced() {
        let (store, _dir) = open_store();
        seed_views(&store, &[RECENT_AUTHORED_ISSUES], &[], 500);
        let fake = Arc::new(FakeGitlab::default());
        let env = start_on_demand(store, connected(&fake, 1));

        env.sync.land_issue(created(9, 5, &[1]), account(1)).await;
        assert!(env.store.issues.get((9, 5)).unwrap().is_some());
        assert_eq!(
            view_of(&env, RECENT_AUTHORED_ISSUES).unwrap().keys,
            [(9, 5)]
        );
        for (name, job) in [
            (ASSIGNED_ISSUES, Job::AssignedIssues),
            (RECENT_ASSIGNED_ISSUES, Job::RecentAssignedIssues),
        ] {
            assert_eq!(view_of(&env, name), None, "{name}");
            assert!(!env.sync.has_synced(job), "{name}");
        }
    }

    /// The store may hold another account's data by the time the issue
    /// arrives: it must not land there.
    #[tokio::test]
    async fn a_landing_for_another_account_is_dropped() {
        let (store, _dir) = open_store();
        seed_views(&store, &ISSUE_VIEWS, &[(9, 1)], 500);
        let fake = Arc::new(FakeGitlab::default());
        let env = start_on_demand(store, connected(&fake, 1));

        let elsewhere = Identity {
            host: "other.test".into(),
            user_id: 1,
        };
        for by in [account(2), elsewhere] {
            env.sync.land_issue(created(9, 5, &[1, 2]), by).await;
        }
        assert!(env.store.issues.scan(RowScope::All).unwrap().is_empty());
        for name in ISSUE_VIEWS {
            assert_eq!(view_of(&env, name).unwrap().keys, [(9, 1)], "{name}");
        }

        // Another account logged in while the create was on its way.
        let fresh = Arc::new(FakeGitlab::default());
        *env.session.write().await = connected(&fresh, 2);
        env.sync.land_issue(created(9, 5, &[1]), account(1)).await;
        assert_eq!(env.store.identity().unwrap(), Some(account(2)));
        assert!(env.store.issues.scan(RowScope::All).unwrap().is_empty());
        assert_eq!(view_of(&env, RECENT_AUTHORED_ISSUES), None, "wiped");
    }

    #[test]
    fn clears_reset_the_jobs_whose_data_they_drop() {
        let keys = BASE
            .iter()
            .chain(&[
                Job::ProjectIssues(7),
                Job::ProjectBoards(7),
                Job::GroupEpics(3),
                Job::AllIssues,
                Job::ProjectAvatar(7),
            ])
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
                "recent/authored/issues",
                "recent/assigned/issues",
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
                "recent/authored/issues",
                "recent/assigned/issues",
                "project/7/issues",
                "group/3/epics",
                "all/issues",
                "project/7/avatar"
            ]
        );
        assert_eq!(
            reset(Clear::Timelogs { from: 0, until: 1 }),
            ["timelogs/recent", "timelogs/all"]
        );
    }
}
