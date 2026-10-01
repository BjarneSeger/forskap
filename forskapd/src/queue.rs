//! Background retry queue for outgoing write operations.
//!
//! Tasks are persisted via `KvStore` before being processed, so they survive
//! daemon restarts. One coordinator task owns the stores and every piece of
//! scheduling state; each attempt is a spawned future that only runs
//! `Write::apply` and reports back through a `JoinSet`. Up to
//! `queue.max_in_flight` attempts run at once, but writes to one issuable run
//! one at a time in enqueue order. Each task backs off exponentially (1 s
//! base, 30 min cap); a 429 also pauses every launch until that task's retry.
//! Network errors, 429s, and 5xx on idempotent ops trigger retries for up to
//! 7 days; a GitLab rejection or an exhausted retry window moves the task to a
//! persistent dead-letter store, surfaced via `forskap queue`. Either way the
//! settle hook hears about it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, mpsc};
use tokio::task::{JoinError, JoinSet};
use tokio::time::Instant;
use tracing::{error, info, warn};

use crate::config::{SharedConfig, next_backoff};
use crate::db::KvStore;
use crate::error::{Error, Result};
use crate::gitlab::{GitlabApi, Issuable};
use crate::handlers::SessionSlot;
use crate::sync::now_secs;
use crate::write::{Write, WriteOp};

const QUEUE_KEYSPACE: &str = "retry_queue_v1";
const DEAD_LETTER_KEYSPACE: &str = "dead_letter_v1";

#[derive(Debug, Serialize, Deserialize)]
struct StoredTask {
    project_id: i64,
    /// Per-project issuable iid. The alias keeps tasks persisted before MR
    /// support readable; `kind` defaults to `Issue` for the same records.
    #[serde(alias = "issue_iid")]
    iid: i64,
    #[serde(default)]
    kind: Issuable,
    op: WriteOp,
    /// UNIX timestamp (seconds) when the task was first enqueued.
    queued_at_secs: u64,
}

/// A task the worker gave up on — GitLab rejected it outright, or it exhausted
/// the retry window. Persisted so the user can inspect, retry, or dismiss it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredFailure {
    project_id: i64,
    /// See [`StoredTask::iid`] for the alias/default compat story.
    #[serde(alias = "issue_iid")]
    iid: i64,
    #[serde(default)]
    kind: Issuable,
    op: WriteOp,
    /// When the task was originally enqueued.
    queued_at_secs: u64,
    /// When the worker gave up.
    failed_at_secs: u64,
    /// The GitLab error or retry-window-exhaustion message.
    error: String,
}

struct QueuedTask {
    id: u64,
    project_id: i64,
    iid: i64,
    kind: Issuable,
    op: WriteOp,
    queued_at_secs: u64,
}

/// The issuable a write targets; writes sharing it never overlap.
type Key = (Issuable, i64, i64);

impl QueuedTask {
    fn key(&self) -> Key {
        (self.kind, self.project_id, self.iid)
    }

    fn write(&self) -> Write {
        Write {
            kind: self.kind,
            project_id: self.project_id,
            iid: self.iid,
            op: self.op.clone(),
        }
    }
}

/// Told when the worker settles a queued write, with the unix seconds it was
/// queued at: `true` once GitLab applied it, `false` when it was
/// dead-lettered.
pub type SettleHook = Arc<dyn Fn(&Write, u64, bool) + Send + Sync>;

pub struct RetryQueue {
    sender: mpsc::Sender<QueuedTask>,
    store: KvStore<u64, StoredTask>,
    dead_letter: KvStore<u64, StoredFailure>,
    next_id: AtomicU64,
    /// Fired to wake the worker early while it is deferring for lack of a
    /// session, so a freshly re-established connection drains the queue at once
    /// instead of waiting out `session_wait`. See [`RetryQueue::drain_waker`].
    drain_wake: Arc<Notify>,
    settle_hook: Arc<OnceLock<SettleHook>>,
}

/// A write still waiting in the retry queue.
pub struct PendingWrite {
    pub write: Write,
    pub queued_at_secs: u64,
}

/// A dead-lettered task, projected for the `forskap queue` view.
pub struct FailedTaskView {
    pub id: u64,
    pub op_kind: &'static str,
    pub project_id: i64,
    pub iid: i64,
    pub kind: Issuable,
    /// Human-readable op detail (e.g. PostTime's duration + summary).
    pub detail: String,
    pub error: String,
    pub queued_at_secs: u64,
    pub failed_at_secs: u64,
}

impl RetryQueue {
    /// Open (or create) the queue keyspaces in `db`, reload any tasks that
    /// survived a previous restart, and spawn the background worker.
    ///
    /// Both stores are durable — every mutation fsyncs — because queued writes
    /// must survive a crash, unlike the re-syncable caches.
    pub fn new(session: SessionSlot, db: &fjall::Database, config: SharedConfig) -> Result<Self> {
        let store: KvStore<u64, StoredTask> = KvStore::open_durable(db, QUEUE_KEYSPACE)?;
        let dead_letter: KvStore<u64, StoredFailure> =
            KvStore::open_durable(db, DEAD_LETTER_KEYSPACE)?;

        let initial_tasks = store.scan(|id, stored| {
            Ok(QueuedTask {
                id,
                project_id: stored.project_id,
                iid: stored.iid,
                kind: stored.kind,
                op: stored.op,
                queued_at_secs: stored.queued_at_secs,
            })
        })?;

        // Seed `next_id` above the max key across *both* tables so dead-letter
        // IDs stay monotonic across restarts (the `forskap tick` notice dedupes by ID).
        let queued_max = initial_tasks.iter().map(|t| t.id).max().unwrap_or(0);
        let dead_max = dead_letter
            .scan(|id, _| Ok(id))?
            .into_iter()
            .max()
            .unwrap_or(0);
        let max_id = queued_max.max(dead_max);

        if !initial_tasks.is_empty() {
            info!(
                count = initial_tasks.len(),
                "reloaded pending tasks from queue database"
            );
        }

        let (tx, rx) = mpsc::channel(256);
        let drain_wake = Arc::new(Notify::new());
        let settle_hook = Arc::new(OnceLock::new());

        let mut worker = Worker::new(
            session,
            store.clone(),
            dead_letter.clone(),
            rx,
            config,
            Arc::clone(&drain_wake),
            Arc::clone(&settle_hook),
        );
        // Admitted before anything enqueued from now on, so a reloaded write
        // keeps its place ahead of a fresh sibling on the same issuable.
        for task in initial_tasks {
            worker.admit(task);
        }
        tokio::spawn(worker.run());

        Ok(Self {
            sender: tx,
            store,
            dead_letter,
            next_id: AtomicU64::new(max_id + 1),
            drain_wake,
            settle_hook,
        })
    }

    /// Install the hook told about every settled write; only the first call
    /// takes effect.
    pub fn on_settled(&self, hook: SettleHook) {
        let _ = self.settle_hook.set(hook);
    }

    /// A handle to nudge the worker awake while it is deferring for lack of a
    /// session. The background reconnect task fires this the instant it flips the
    /// session back to `Connected`, so deferred writes flush immediately rather
    /// than after the next `session_wait` tick.
    pub fn drain_waker(&self) -> Arc<Notify> {
        Arc::clone(&self.drain_wake)
    }

    /// Persist `write` to disk and hand it to the background worker. Returns
    /// immediately; the caller does not wait for the network operation.
    ///
    /// A PostTime's `issuable_id` (the global id GraphQL embeds in
    /// `gid://gitlab/<Kind>/<id>`) lets the worker submit the enqueue time as
    /// `spentAt`, so a task held for hours still shows up in GitLab at the
    /// time it was actually logged.
    pub async fn enqueue(&self, write: Write) {
        self.enqueue_stored(StoredTask {
            project_id: write.project_id,
            iid: write.iid,
            kind: write.kind,
            op: write.op,
            queued_at_secs: now_secs(),
        })
        .await
    }

    /// Snapshot of the writes still waiting in the queue, newest first. The
    /// history view shows queued PostTimes from it before GitLab has them.
    pub fn pending(&self) -> Result<Vec<PendingWrite>> {
        snapshot_pending(&self.store)
    }

    /// Persist `stored` under a fresh ID and hand it to the worker, keeping its
    /// `queued_at_secs` intact (so a retried PostTime keeps its original
    /// spent-at). Used by both fresh enqueues and dead-letter retries.
    async fn enqueue_stored(&self, stored: StoredTask) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let task = QueuedTask {
            id,
            project_id: stored.project_id,
            iid: stored.iid,
            kind: stored.kind,
            op: stored.op.clone(),
            queued_at_secs: stored.queued_at_secs,
        };
        if let Err(e) = self.store.put(id, &stored) {
            warn!(
                error = %e,
                "failed to persist task to queue db; it will not survive a restart"
            );
        }
        if self.sender.send(task).await.is_err() {
            error!("retry queue channel closed; dropping task");
        }
    }

    /// Snapshot of dead-lettered tasks (failed permanently or timed out),
    /// newest failure first.
    pub fn failures(&self) -> Result<Vec<FailedTaskView>> {
        let mut out = self.dead_letter.scan(|id, f| {
            Ok(FailedTaskView {
                id,
                op_kind: f.op.name(),
                project_id: f.project_id,
                iid: f.iid,
                kind: f.kind,
                detail: f.op.detail(),
                error: f.error,
                queued_at_secs: f.queued_at_secs,
                failed_at_secs: f.failed_at_secs,
            })
        })?;
        out.sort_by_key(|f| std::cmp::Reverse(f.failed_at_secs));
        Ok(out)
    }

    /// Re-enqueue a dead-lettered task (preserving its original enqueue time)
    /// and drop it from the dead-letter store. Returns `false` if `id` is
    /// unknown.
    pub async fn retry_failure(&self, id: u64) -> Result<bool> {
        let Some(failure) = self.dead_letter.get(id)? else {
            return Ok(false);
        };
        self.enqueue_stored(StoredTask {
            project_id: failure.project_id,
            iid: failure.iid,
            kind: failure.kind,
            op: failure.op,
            queued_at_secs: failure.queued_at_secs,
        })
        .await;
        self.dead_letter.remove(id)?;
        Ok(true)
    }

    /// Drop a single dead-lettered task without retrying. Returns `false` if
    /// `id` is unknown.
    pub fn dismiss_failure(&self, id: u64) -> Result<bool> {
        if self.dead_letter.get(id)?.is_none() {
            return Ok(false);
        }
        self.dead_letter.remove(id)?;
        Ok(true)
    }

    /// Drop every dead-lettered task.
    pub fn clear_failures(&self) -> Result<()> {
        self.dead_letter.clear()
    }
}

fn snapshot_pending(store: &KvStore<u64, StoredTask>) -> Result<Vec<PendingWrite>> {
    let mut out = store.scan(|_, stored| {
        Ok(PendingWrite {
            write: Write {
                kind: stored.kind,
                project_id: stored.project_id,
                iid: stored.iid,
                op: stored.op,
            },
            queued_at_secs: stored.queued_at_secs,
        })
    })?;
    out.sort_by_key(|p| std::cmp::Reverse(p.queued_at_secs));
    Ok(out)
}

/// One queued write and where it is in its retry schedule.
struct Pending {
    task: QueuedTask,
    /// Attempts launched so far.
    attempt: u32,
    /// What the next failure waits; doubles per failure up to the cap.
    delay: Duration,
    /// Not before this; `None` is ready.
    due: Option<Instant>,
}

/// The coordinator's scheduling state. Clock-free — every method takes
/// `now` — so the schedule is testable without sleeping through it.
#[derive(Default)]
struct Backlog {
    /// Waiting tasks by id, which is enqueue order.
    waiting: BTreeMap<u64, Pending>,
    /// Tasks with an attempt running.
    in_flight: HashMap<u64, Pending>,
    /// After a 429: nothing launches before this.
    paused_until: Option<Instant>,
}

impl Backlog {
    fn push(&mut self, pending: Pending) {
        let id = pending.task.id;
        let replaced = self.waiting.insert(id, pending);
        debug_assert!(replaced.is_none(), "task {id} admitted twice");
    }

    fn is_idle(&self) -> bool {
        self.waiting.is_empty() && self.in_flight.is_empty()
    }

    fn paused(&self, now: Instant) -> bool {
        self.paused_until.is_some_and(|until| until > now)
    }

    /// Hold every launch until `until`; never shortens a pause already set.
    fn pause_until(&mut self, until: Instant) {
        self.paused_until = Some(self.paused_until.map_or(until, |p| p.max(until)));
    }

    /// Ids that may start now, in enqueue order, at most `slots` of them. A
    /// task waits while an attempt on its issuable runs or an earlier
    /// sibling waits — even one still backing off — so writes to one
    /// issuable land in order.
    fn eligible(&self, now: Instant, slots: usize) -> Vec<u64> {
        let mut out = Vec::new();
        if slots == 0 || self.paused(now) {
            return out;
        }
        let mut claimed: HashSet<Key> = self.in_flight.values().map(|p| p.task.key()).collect();
        for (id, pending) in &self.waiting {
            if out.len() == slots {
                break;
            }
            if !claimed.insert(pending.task.key()) {
                continue;
            }
            if pending.due.is_some_and(|due| due > now) {
                continue;
            }
            out.push(*id);
        }
        out
    }

    /// Move `id` to the running attempts and count the attempt.
    fn start(&mut self, id: u64) -> &Pending {
        let mut pending = self
            .waiting
            .remove(&id)
            .expect("only waiting ids are started");
        pending.attempt += 1;
        self.in_flight.insert(id, pending);
        &self.in_flight[&id]
    }

    /// Take `id` back from the running attempts.
    fn finish(&mut self, id: u64) -> Pending {
        self.in_flight
            .remove(&id)
            .expect("only running ids are finished")
    }

    /// Put a failed task back to wait out the larger of its backoff and
    /// `retry_after`, capped at `remaining` (of its lifetime); the backoff
    /// then doubles up to `max_delay`. Returns the wait chosen.
    fn back_off(
        &mut self,
        mut pending: Pending,
        now: Instant,
        retry_after: Option<Duration>,
        remaining: Duration,
        max_delay: Duration,
    ) -> Duration {
        let wait = pending
            .delay
            .max(retry_after.unwrap_or_default())
            .min(remaining);
        pending.due = Some(now + wait);
        pending.delay = next_backoff(pending.delay, max_delay);
        self.push(pending);
        wait
    }

    /// The earliest instant after `now` worth a timer: the end of the pause
    /// while paused (nothing launches before it), else the earliest due time
    /// still ahead. `None` when a timer would change nothing — a ready task
    /// held by a slot or a sibling waits for an event instead.
    fn next_wake(&self, now: Instant) -> Option<Instant> {
        if self.waiting.is_empty() {
            return None;
        }
        if let Some(until) = self.paused_until.filter(|until| *until > now) {
            return Some(until);
        }
        self.waiting
            .values()
            .filter_map(|p| p.due)
            .filter(|due| *due > now)
            .min()
    }
}

/// What an attempt's outcome means for its task.
#[derive(Debug)]
enum Verdict {
    Applied,
    /// Out of the queue for good: rejected, or `expired` past the retry window.
    DeadLetter {
        error: String,
        expired: bool,
    },
    /// Try again later; `pause` (a 429) also holds every other launch.
    Retry {
        error: Error,
        pause: bool,
    },
}

/// Classify an attempt's outcome. Pure: `elapsed` is how long the task has
/// been queued, measured against the retry window.
fn verdict(
    outcome: Result<()>,
    op: &WriteOp,
    elapsed: Duration,
    max_lifetime: Duration,
) -> Verdict {
    match outcome {
        Ok(()) => Verdict::Applied,
        Err(e) if e.is_retryable(op.idempotent()) => {
            if elapsed >= max_lifetime {
                return Verdict::DeadLetter {
                    error: format!(
                        "timed out after {}, seconds retry window: {}",
                        max_lifetime.as_secs(),
                        e
                    ),
                    expired: true,
                };
            }
            Verdict::Retry {
                pause: matches!(e, Error::Throttled { status: 429, .. }),
                error: e,
            }
        }
        Err(e) => Verdict::DeadLetter {
            error: e.to_string(),
            expired: false,
        },
    }
}

/// The coordinator: the only owner of the queue stores and the schedule.
/// Attempts run as spawned futures that do nothing but `Write::apply`.
struct Worker {
    session: SessionSlot,
    store: KvStore<u64, StoredTask>,
    dead_letter: KvStore<u64, StoredFailure>,
    rx: mpsc::Receiver<QueuedTask>,
    /// `rx` is closed; the loop ends once the backlog is idle.
    rx_closed: bool,
    config: SharedConfig,
    drain_wake: Arc<Notify>,
    settle_hook: Arc<OnceLock<SettleHook>>,
    backlog: Backlog,
    /// One spawned `Write::apply` per running attempt.
    attempts: JoinSet<Result<()>>,
    /// Queue id per running attempt, by tokio task id: a panic reports only that.
    attempt_ids: HashMap<tokio::task::Id, u64>,
}

impl Worker {
    fn new(
        session: SessionSlot,
        store: KvStore<u64, StoredTask>,
        dead_letter: KvStore<u64, StoredFailure>,
        rx: mpsc::Receiver<QueuedTask>,
        config: SharedConfig,
        drain_wake: Arc<Notify>,
        settle_hook: Arc<OnceLock<SettleHook>>,
    ) -> Self {
        Self {
            session,
            store,
            dead_letter,
            rx,
            rx_closed: false,
            config,
            drain_wake,
            settle_hook,
            backlog: Backlog::default(),
            attempts: JoinSet::new(),
            attempt_ids: HashMap::new(),
        }
    }

    /// Admit a task, ready to launch.
    fn admit(&mut self, task: QueuedTask) {
        let delay = self.config.read().unwrap().queue.base_delay();
        self.backlog.push(Pending {
            task,
            attempt: 0,
            delay,
            due: None,
        });
    }

    /// Admit everything already on the channel.
    fn admit_ready(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(task) => self.admit(task),
                Err(mpsc::error::TryRecvError::Empty) => return,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    self.rx_closed = true;
                    return;
                }
            }
        }
    }

    async fn run(mut self) {
        loop {
            self.admit_ready();
            if self.rx_closed && self.backlog.is_idle() {
                break;
            }
            let now = Instant::now();
            let deferring = self.launch(now).await;
            let next_wake = if deferring {
                None
            } else {
                self.backlog.next_wake(now)
            };
            let session_wait = deferring.then(|| self.config.read().unwrap().queue.session_wait());
            // A disabled arm still evaluates its expression, hence the
            // placeholders. The drain waker uses `notify_one`, which leaves a
            // permit if we haven't parked here yet — so a nudge fired the
            // instant before this `select!` is still delivered on entry.
            tokio::select! {
                task = self.rx.recv(), if !self.rx_closed => match task {
                    Some(task) => self.admit(task),
                    None => self.rx_closed = true,
                },
                Some(joined) = self.attempts.join_next_with_id(), if !self.attempts.is_empty() => {
                    self.settle(joined);
                }
                _ = tokio::time::sleep_until(next_wake.unwrap_or(now)), if next_wake.is_some() => {}
                _ = tokio::time::sleep(session_wait.unwrap_or_default()), if session_wait.is_some() => {}
                _ = self.drain_wake.notified() => {}
            }
        }
    }

    /// Start every eligible task the bound allows. True when something is
    /// eligible but the session is dormant, so the caller waits for one.
    async fn launch(&mut self, now: Instant) -> bool {
        let max = self.config.read().unwrap().queue.max_in_flight();
        let slots = max.saturating_sub(self.backlog.in_flight.len());
        let ids = self.backlog.eligible(now, slots);
        if ids.is_empty() {
            return false;
        }
        // Bind the clone in its own statement so the read guard is released
        // here — never held across the `select!`. A guard held while waiting
        // would block every session *writer* (the reconnect commit, `forskap auth login`).
        let current = self.session.read().await.gitlab();
        let Some(gitlab) = current else {
            warn!(
                waiting = self.backlog.waiting.len(),
                "no active session; deferring queued writes"
            );
            return true;
        };
        for id in ids {
            self.spawn_attempt(id, Arc::clone(&gitlab));
        }
        false
    }

    fn spawn_attempt(&mut self, id: u64, gitlab: Arc<dyn GitlabApi>) {
        let pending = self.backlog.start(id);
        let (write, queued_at) = (pending.task.write(), pending.task.queued_at_secs);
        let handle = self
            .attempts
            .spawn(async move { write.apply(&*gitlab, Some(queued_at)).await });
        self.attempt_ids.insert(handle.id(), id);
    }

    /// Settle a finished attempt: back its task off, or take it out of the
    /// queue for good.
    fn settle(&mut self, joined: std::result::Result<(tokio::task::Id, Result<()>), JoinError>) {
        let (tid, outcome) = match joined {
            Ok(joined) => joined,
            Err(e) => {
                let id = self
                    .attempt_ids
                    .remove(&e.id())
                    .expect("every attempt is registered");
                let pending = self.backlog.finish(id);
                error!(
                    error = %e,
                    project_id = pending.task.project_id,
                    iid = pending.task.iid,
                    kind = ?pending.task.kind,
                    op = pending.task.op.name(),
                    "task attempt did not complete; dropping"
                );
                self.conclude(pending.task, Some(format!("attempt did not complete: {e}")));
                return;
            }
        };
        let id = self
            .attempt_ids
            .remove(&tid)
            .expect("every attempt is registered");
        let pending = self.backlog.finish(id);
        let elapsed = Duration::from_secs(now_secs().saturating_sub(pending.task.queued_at_secs));
        let (max_lifetime, max_delay) = {
            let cfg = self.config.read().unwrap();
            (cfg.queue.max_lifetime(), cfg.queue.max_delay())
        };
        let attempt = pending.attempt;
        let (project_id, iid, kind, op) = (
            pending.task.project_id,
            pending.task.iid,
            pending.task.kind,
            pending.task.op.name(),
        );
        match verdict(outcome, &pending.task.op, elapsed, max_lifetime) {
            Verdict::Applied => {
                if attempt > 1 {
                    info!(
                        attempt,
                        project_id,
                        iid,
                        ?kind,
                        op,
                        "task succeeded after retry"
                    );
                }
                self.conclude(pending.task, None);
            }
            Verdict::DeadLetter { error, expired } => {
                if expired {
                    error!(
                        attempt,
                        error = %error,
                        project_id,
                        iid,
                        ?kind,
                        op,
                        retry_window = max_lifetime.as_secs(),
                        "dropping task after retry window"
                    );
                } else {
                    error!(error = %error, project_id, iid, ?kind, op, "task rejected by GitLab; dropping");
                }
                self.conclude(pending.task, Some(error));
            }
            Verdict::Retry { error, pause } => {
                let now = Instant::now();
                let wait = self.backlog.back_off(
                    pending,
                    now,
                    error.retry_after(),
                    max_lifetime.saturating_sub(elapsed),
                    max_delay,
                );
                if pause {
                    self.backlog.pause_until(now + wait);
                }
                warn!(
                    attempt,
                    error = %error,
                    delay_secs = wait.as_secs(),
                    project_id,
                    op,
                    paused = pause,
                    "task failed transiently, retrying"
                );
            }
        }
    }

    /// Take a settled task out of the live queue. A failure is recorded in the
    /// dead-letter store first (keyed by the task id) so the user can see,
    /// retry, or dismiss it via `forskap queue`.
    fn conclude(&self, task: QueuedTask, failure: Option<String>) {
        let applied = failure.is_none();
        if let Some(error) = failure {
            let stored = StoredFailure {
                project_id: task.project_id,
                iid: task.iid,
                kind: task.kind,
                op: task.op.clone(),
                queued_at_secs: task.queued_at_secs,
                failed_at_secs: now_secs(),
                error,
            };
            if let Err(e) = self.dead_letter.put(task.id, &stored) {
                warn!(
                    error = %e,
                    task_id = task.id,
                    "failed to record dead-letter entry"
                );
            }
        }

        if let Err(e) = self.store.remove(task.id) {
            warn!(
                error = %e,
                task_id = task.id,
                "failed to remove completed task from queue db"
            );
        }
        if let Some(hook) = self.settle_hook.get() {
            hook(&task.write(), task.queued_at_secs, applied);
        }
    }
}

/// The coordinator as one future, for tests that drive it over a channel.
#[cfg(test)]
async fn worker(
    session: SessionSlot,
    store: KvStore<u64, StoredTask>,
    dead_letter: KvStore<u64, StoredFailure>,
    rx: mpsc::Receiver<QueuedTask>,
    config: SharedConfig,
    drain_wake: Arc<Notify>,
    settle_hook: Arc<OnceLock<SettleHook>>,
) {
    Worker::new(
        session,
        store,
        dead_letter,
        rx,
        config,
        drain_wake,
        settle_hook,
    )
    .run()
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::DormancyReason;
    use crate::gitlab::GitlabApi;
    use crate::handlers::{ConnState, Session};
    use crate::testing::{FakeErr, FakeGitlab, eventually};

    // The schedule (backoff, pause, per-issuable order) is exercised on the
    // clock-free `Backlog` and `verdict`. Only the wall-clock lifetime
    // cutoff is not: it depends on `SystemTime::now()`, which a clock
    // abstraction isn't warranted for.

    // ── Persisted-record compatibility ──────────────────────────────────────

    /// Records written before MR support carried `issue_iid`/`issue_id`, no
    /// `kind`, and the `CloseIssue` variant name. They must keep parsing —
    /// the retry queue is durable across upgrades by design.
    #[test]
    fn stored_task_written_before_mr_support_still_parses() {
        let old = r#"{"project_id":7,"issue_iid":9,"op":{"PostTime":{"duration":"1h","summary":null,"issue_id":42}},"queued_at_secs":100}"#;
        let t: StoredTask = serde_json::from_str(old).unwrap();
        assert_eq!(t.iid, 9);
        assert_eq!(t.kind, Issuable::Issue, "missing kind defaults to Issue");
        match t.op {
            WriteOp::PostTime { issuable_id, .. } => assert_eq!(issuable_id, Some(42)),
            other => panic!("expected PostTime, got {other:?}"),
        }

        let old_close = r#"{"project_id":7,"issue_iid":9,"op":"CloseIssue","queued_at_secs":100}"#;
        let t: StoredTask = serde_json::from_str(old_close).unwrap();
        assert!(matches!(t.op, WriteOp::Close), "CloseIssue alias parses");
    }

    #[test]
    fn stored_failure_written_before_mr_support_still_parses() {
        let old = r#"{"project_id":7,"issue_iid":9,"op":"CloseIssue","queued_at_secs":100,"failed_at_secs":200,"error":"403"}"#;
        let f: StoredFailure = serde_json::from_str(old).unwrap();
        assert_eq!(f.iid, 9);
        assert_eq!(f.kind, Issuable::Issue);
        assert!(matches!(f.op, WriteOp::Close));
    }

    // ── snapshot_pending ────────────────────────────────────────────────────

    fn test_db(dir: &tempfile::TempDir) -> fjall::Database {
        fjall::Database::builder(dir.path().join("db"))
            .open()
            .unwrap()
    }

    fn store() -> (KvStore<u64, StoredTask>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = test_db(&dir);
        (KvStore::open_durable(&db, QUEUE_KEYSPACE).unwrap(), dir)
    }

    fn post_task(id: i64, queued_at: u64) -> StoredTask {
        StoredTask {
            project_id: id,
            iid: id,
            kind: Issuable::Issue,
            op: WriteOp::PostTime {
                duration: "1h".into(),
                summary: None,
                issuable_id: None,
            },
            queued_at_secs: queued_at,
        }
    }

    fn close_task(id: i64, queued_at: u64) -> StoredTask {
        StoredTask {
            project_id: id,
            iid: id,
            kind: Issuable::Issue,
            op: WriteOp::Close,
            queued_at_secs: queued_at,
        }
    }

    #[test]
    fn snapshot_pending_keeps_every_op_newest_first() {
        let (s, _td) = store();
        s.put(1, &post_task(1, 100)).unwrap();
        s.put(2, &close_task(2, 150)).unwrap();
        s.put(3, &post_task(3, 200)).unwrap();
        s.put(4, &post_task(4, 50)).unwrap();

        let snap = snapshot_pending(&s).unwrap();
        let project_ids: Vec<i64> = snap.iter().map(|p| p.write.project_id).collect();
        assert_eq!(project_ids, vec![3, 2, 1, 4], "sorted by queued_at desc");
        assert_eq!(snap[1].write.op, WriteOp::Close);
        assert!(
            snap.iter().all(|p| p.write.kind == Issuable::Issue),
            "kind carried into the projection"
        );
    }

    #[test]
    fn snapshot_pending_empty_store() {
        let (s, _td) = store();
        assert!(snapshot_pending(&s).unwrap().is_empty());
    }

    // ── Worker behavior with a fake GitLab ──────────────────────────────────

    /// Writes of `op` the fake received.
    fn calls(gitlab: &FakeGitlab, op: &str) -> usize {
        gitlab.writes().iter().filter(|w| w.0 == op).count()
    }

    /// Spawn the worker with one task on the channel, close the sender so the
    /// worker exits after draining, and await its completion. Returns the
    /// dead-letter entries it recorded (projected), newest failure first.
    async fn run_worker_one_task(
        gitlab: Arc<dyn GitlabApi>,
        store: KvStore<u64, StoredTask>,
        task: QueuedTask,
    ) -> Vec<FailedTaskView> {
        run_worker_one_task_with(crate::config::defaults(), gitlab, store, task).await
    }

    /// A worker spawned on a connected session; `dir` backs its dead-letter
    /// store, so keep it alive.
    struct Spawned {
        tx: mpsc::Sender<QueuedTask>,
        handle: tokio::task::JoinHandle<()>,
        dead_letter: KvStore<u64, StoredFailure>,
        dir: tempfile::TempDir,
    }

    fn spawn_worker(
        cfg: crate::config::Config,
        gitlab: Arc<dyn GitlabApi>,
        store: KvStore<u64, StoredTask>,
    ) -> Spawned {
        let dir = tempfile::tempdir().unwrap();
        let dead_letter = KvStore::open_durable(&test_db(&dir), DEAD_LETTER_KEYSPACE).unwrap();
        let session: SessionSlot =
            Arc::new(tokio::sync::RwLock::new(ConnState::Connected(Session {
                gitlab,
                host: "test".to_string(),
                user_id: 0,
                username: "tester".into(),
                token: Default::default(),
            })));
        let (tx, rx) = mpsc::channel(8);
        let handle = tokio::spawn(worker(
            session,
            store,
            dead_letter.clone(),
            rx,
            Arc::new(std::sync::RwLock::new(cfg)),
            Arc::new(Notify::new()),
            Arc::new(OnceLock::new()),
        ));
        Spawned {
            tx,
            handle,
            dead_letter,
            dir,
        }
    }

    /// A Close-shaped task on issue `iid` of project 7, queued now.
    fn task(id: u64, iid: i64, op: WriteOp) -> QueuedTask {
        QueuedTask {
            id,
            project_id: 7,
            iid,
            kind: Issuable::Issue,
            op,
            queued_at_secs: now_secs(),
        }
    }

    async fn run_worker_one_task_with(
        cfg: crate::config::Config,
        gitlab: Arc<dyn GitlabApi>,
        store: KvStore<u64, StoredTask>,
        task: QueuedTask,
    ) -> Vec<FailedTaskView> {
        let Spawned {
            tx,
            handle,
            dead_letter,
            dir: _dir,
        } = spawn_worker(cfg, gitlab, store);
        tx.send(task).await.unwrap();
        drop(tx);
        handle.await.unwrap();

        let mut out = dead_letter
            .scan(|id, f| {
                Ok(FailedTaskView {
                    id,
                    op_kind: f.op.name(),
                    project_id: f.project_id,
                    iid: f.iid,
                    kind: f.kind,
                    detail: f.op.detail(),
                    error: f.error,
                    queued_at_secs: f.queued_at_secs,
                    failed_at_secs: f.failed_at_secs,
                })
            })
            .unwrap();
        out.sort_by_key(|f| std::cmp::Reverse(f.failed_at_secs));
        out
    }

    #[tokio::test]
    async fn worker_removes_task_on_success() {
        let (s, _td) = store();
        s.put(1, &post_task(7, 100)).unwrap();

        let gitlab = Arc::new(FakeGitlab::default());

        let task = QueuedTask {
            id: 1,
            project_id: 7,
            iid: 7,
            kind: Issuable::Issue,
            op: WriteOp::PostTime {
                duration: "1h".into(),
                summary: None,
                issuable_id: None,
            },
            queued_at_secs: 100,
        };
        run_worker_one_task(gitlab.clone(), s.clone(), task).await;

        assert_eq!(calls(&gitlab, "add_spent_time"), 1);
        assert!(
            snapshot_pending(&s).unwrap().is_empty(),
            "task removed after success"
        );
    }

    #[tokio::test]
    async fn worker_defers_while_dormant_then_drains_on_reconnect_nudge() {
        let (s, _td) = store();
        s.put(1, &post_task(7, 100)).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let dead_letter = KvStore::open_durable(&test_db(&dir), DEAD_LETTER_KEYSPACE).unwrap();

        // Start dormant so the task can't run yet. `session_wait` is set to an
        // hour so the defer branch's timeout can't possibly fire during the test:
        // the only thing that can wake the worker in time is the reconnect nudge,
        // so if the nudge regresses this test hangs and the `timeout` below fails.
        let session: SessionSlot = Arc::new(tokio::sync::RwLock::new(ConnState::Dormant(
            DormancyReason::NoCredentials,
        )));
        let mut cfg = crate::config::defaults();
        cfg.queue.session_wait_secs = 3600;
        let config = Arc::new(std::sync::RwLock::new(cfg));
        let drain_wake = Arc::new(Notify::new());

        let gitlab = Arc::new(FakeGitlab::default());

        let (tx, rx) = mpsc::channel(8);
        let handle = tokio::spawn(worker(
            session.clone(),
            s.clone(),
            dead_letter,
            rx,
            config,
            Arc::clone(&drain_wake),
            Arc::new(OnceLock::new()),
        ));

        tx.send(QueuedTask {
            id: 1,
            project_id: 7,
            iid: 7,
            kind: Issuable::Issue,
            op: WriteOp::PostTime {
                duration: "1h".into(),
                summary: None,
                issuable_id: None,
            },
            queued_at_secs: 100,
        })
        .await
        .unwrap();

        // Give the worker a beat to reach the defer branch, then "reconnect":
        // flip the session live and nudge the worker to drain immediately.
        tokio::time::sleep(Duration::from_millis(10)).await;
        *session.write().await = ConnState::Connected(Session {
            gitlab: gitlab.clone(),
            host: "test".into(),
            user_id: 0,
            username: "tester".into(),
            token: Default::default(),
        });
        drain_wake.notify_one();
        drop(tx);

        // The worker must drain via the nudge, not the (1-hour) session_wait
        // timeout: bound the join so a lost wakeup fails the test instead of
        // hanging it. With the nudge working this completes in well under a ms.
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("worker drained via the reconnect nudge, not the session_wait timeout")
            .unwrap();

        assert_eq!(calls(&gitlab, "add_spent_time"), 1);
        assert!(
            snapshot_pending(&s).unwrap().is_empty(),
            "deferred task drained after reconnect"
        );
    }

    #[tokio::test]
    async fn worker_tells_the_hook_how_each_task_settled() {
        let (s, _td) = store();
        let dir = tempfile::tempdir().unwrap();
        let dead_letter = KvStore::open_durable(&test_db(&dir), DEAD_LETTER_KEYSPACE).unwrap();
        let gitlab = Arc::new(FakeGitlab::default());
        let session: SessionSlot =
            Arc::new(tokio::sync::RwLock::new(ConnState::Connected(Session {
                gitlab: gitlab.clone(),
                host: "test".into(),
                user_id: 0,
                username: "tester".into(),
                token: Default::default(),
            })));
        let settled = Arc::new(std::sync::Mutex::new(Vec::new()));
        let hook_cell = Arc::new(OnceLock::new());
        let seen = Arc::clone(&settled);
        let hook: SettleHook = Arc::new(move |w: &Write, _queued_at, applied| {
            seen.lock().unwrap().push((w.op.name(), w.iid, applied));
        });
        let _ = hook_cell.set(hook);

        let (tx, rx) = mpsc::channel(8);
        let config = Arc::new(std::sync::RwLock::new(crate::config::defaults()));
        let handle = tokio::spawn(worker(
            session,
            s,
            dead_letter,
            rx,
            config,
            Arc::new(Notify::new()),
            hook_cell,
        ));
        gitlab.fail_next_write(FakeErr::Rejected);
        // Both on one issuable, so they run one after the other and the
        // queued rejection is the assign's.
        for (id, op) in [(1, WriteOp::AssignSelf), (2, WriteOp::UnassignSelf)] {
            tx.send(task(id, 7, op)).await.unwrap();
        }
        drop(tx);
        handle.await.unwrap();

        assert_eq!(
            *settled.lock().unwrap(),
            [("AssignSelf", 7, false), ("UnassignSelf", 7, true)],
            "the rejected task is reported dead-lettered, the next one applied"
        );
    }

    #[tokio::test]
    async fn worker_routes_post_time_with_issue_id_to_create_timelog() {
        let (s, _td) = store();
        let task = QueuedTask {
            id: 1,
            project_id: 7,
            iid: 7,
            kind: Issuable::Issue,
            op: WriteOp::PostTime {
                duration: "1h".into(),
                summary: Some("note".into()),
                issuable_id: Some(999),
            },
            queued_at_secs: 100,
        };
        s.put(
            1,
            &StoredTask {
                project_id: 7,
                iid: 7,
                kind: Issuable::Issue,
                op: task.op.clone(),
                queued_at_secs: 100,
            },
        )
        .unwrap();

        let gitlab = Arc::new(FakeGitlab::default());

        run_worker_one_task(gitlab.clone(), s.clone(), task).await;

        assert_eq!(
            calls(&gitlab, "create_timelog"),
            1,
            "issue_id present → GraphQL timelogCreate"
        );
        assert_eq!(calls(&gitlab, "add_spent_time"), 0);
    }

    /// A PostTime queued without the issuable's global id (not in the
    /// store at enqueue) looks it up, so the replay keeps its time.
    #[tokio::test]
    async fn worker_looks_up_a_missing_issuable_id_for_the_replay() {
        let (s, _td) = store();
        s.put(1, &post_task(7, 100)).unwrap();
        let gitlab = Arc::new(FakeGitlab::default());
        gitlab.serve(
            "projects/7/issues",
            vec![crate::testing::issue_json(7, 7, "t")],
        );

        let task = QueuedTask {
            id: 1,
            project_id: 7,
            iid: 7,
            kind: Issuable::Issue,
            op: WriteOp::PostTime {
                duration: "1h".into(),
                summary: None,
                issuable_id: None,
            },
            queued_at_secs: 100,
        };
        run_worker_one_task(gitlab.clone(), s.clone(), task).await;

        assert_eq!(
            gitlab.writes(),
            [("create_timelog", Issuable::Issue, 0, 7007)]
        );
    }

    #[tokio::test]
    async fn worker_drops_task_on_permanent_error() {
        let (s, _td) = store();
        s.put(1, &post_task(7, 100)).unwrap();

        let gitlab = Arc::new(FakeGitlab::default());
        gitlab.fail_next_write(FakeErr::Rejected);

        let task = QueuedTask {
            id: 1,
            project_id: 7,
            iid: 7,
            kind: Issuable::Issue,
            op: WriteOp::PostTime {
                duration: "1h".into(),
                summary: None,
                issuable_id: None,
            },
            queued_at_secs: 100,
        };
        run_worker_one_task(gitlab.clone(), s.clone(), task).await;

        assert_eq!(
            calls(&gitlab, "add_spent_time"),
            1,
            "no retry on permanent error"
        );
        assert!(
            snapshot_pending(&s).unwrap().is_empty(),
            "task dropped after permanent rejection"
        );
    }

    /// Backoff delays at zero so a retry costs no wall time.
    fn instant_retry_config() -> crate::config::Config {
        let mut cfg = crate::config::defaults();
        cfg.queue.base_delay_secs = 0;
        cfg.queue.max_delay_secs = 0;
        cfg
    }

    #[tokio::test]
    async fn worker_retries_throttled_close_until_it_lands() {
        let (s, _td) = store();
        s.put(1, &close_task(7, now_secs())).unwrap();

        let gitlab = Arc::new(FakeGitlab::default());
        gitlab.fail_next_write(FakeErr::Throttled(429));
        gitlab.fail_next_write(FakeErr::Throttled(503));

        let task = QueuedTask {
            id: 1,
            project_id: 7,
            iid: 7,
            kind: Issuable::Issue,
            op: WriteOp::Close,
            queued_at_secs: now_secs(),
        };
        let failures =
            run_worker_one_task_with(instant_retry_config(), gitlab.clone(), s.clone(), task).await;

        assert_eq!(calls(&gitlab, "close"), 3);
        assert!(failures.is_empty(), "idempotent close retried through 5xx");
    }

    /// A 5xx may come after GitLab already stored the timelog; retrying could
    /// book the time twice, so it is dead-lettered for the user to judge.
    #[tokio::test]
    async fn worker_dead_letters_post_time_on_server_error() {
        let (s, _td) = store();
        s.put(1, &post_task(7, now_secs())).unwrap();

        let gitlab = Arc::new(FakeGitlab::default());
        gitlab.fail_next_write(FakeErr::Throttled(502));

        let task = QueuedTask {
            id: 1,
            project_id: 7,
            iid: 7,
            kind: Issuable::Issue,
            op: WriteOp::PostTime {
                duration: "1h".into(),
                summary: None,
                issuable_id: None,
            },
            queued_at_secs: now_secs(),
        };
        let failures =
            run_worker_one_task_with(instant_retry_config(), gitlab.clone(), s.clone(), task).await;

        assert_eq!(calls(&gitlab, "add_spent_time"), 1);
        assert_eq!(failures.len(), 1);
    }

    #[tokio::test]
    async fn worker_retries_rate_limited_post_time() {
        let (s, _td) = store();
        s.put(1, &post_task(7, now_secs())).unwrap();

        let gitlab = Arc::new(FakeGitlab::default());
        gitlab.fail_next_write(FakeErr::Throttled(429));

        let task = QueuedTask {
            id: 1,
            project_id: 7,
            iid: 7,
            kind: Issuable::Issue,
            op: WriteOp::PostTime {
                duration: "1h".into(),
                summary: None,
                issuable_id: None,
            },
            queued_at_secs: now_secs(),
        };
        let failures =
            run_worker_one_task_with(instant_retry_config(), gitlab.clone(), s.clone(), task).await;

        assert_eq!(
            calls(&gitlab, "add_spent_time"),
            2,
            "a 429 is rejected before any work, so PostTime retries it"
        );
        assert!(failures.is_empty());
    }

    #[tokio::test]
    async fn worker_close_success() {
        let (s, _td) = store();
        s.put(1, &close_task(7, 100)).unwrap();

        let gitlab = Arc::new(FakeGitlab::default());

        let task = QueuedTask {
            id: 1,
            project_id: 7,
            iid: 7,
            kind: Issuable::Issue,
            op: WriteOp::Close,
            queued_at_secs: 100,
        };
        run_worker_one_task(gitlab.clone(), s.clone(), task).await;

        assert_eq!(calls(&gitlab, "close"), 1);
    }

    /// The worker must thread a task's `MergeRequest` kind into every GitLab
    /// call — a dropped kind would silently act on the wrong resource class.
    #[tokio::test]
    async fn worker_passes_mr_kind_through_to_gitlab_calls() {
        let (s, _td) = store();

        let gitlab = Arc::new(FakeGitlab::default());

        for (id, op) in [
            (1u64, WriteOp::Close),
            (
                2,
                WriteOp::PostTime {
                    duration: "1h".into(),
                    summary: None,
                    issuable_id: None,
                },
            ),
            (
                3,
                WriteOp::PostTime {
                    duration: "1h".into(),
                    summary: None,
                    issuable_id: Some(999),
                },
            ),
        ] {
            let task = QueuedTask {
                id,
                project_id: 7,
                iid: 42,
                kind: Issuable::MergeRequest,
                op,
                queued_at_secs: 100,
            };
            run_worker_one_task(gitlab.clone(), s.clone(), task).await;
        }

        assert_eq!(
            gitlab
                .writes()
                .into_iter()
                .map(|(op, kind, _, _)| (op, kind))
                .collect::<Vec<_>>(),
            vec![
                ("close", Issuable::MergeRequest),
                ("add_spent_time", Issuable::MergeRequest),
                ("create_timelog", Issuable::MergeRequest),
            ]
        );
    }

    #[tokio::test]
    async fn worker_assign_self_success() {
        let (s, _td) = store();
        s.put(
            1,
            &StoredTask {
                project_id: 7,
                iid: 7,
                kind: Issuable::Issue,
                op: WriteOp::AssignSelf,
                queued_at_secs: 100,
            },
        )
        .unwrap();

        let gitlab = Arc::new(FakeGitlab::default());

        let task = QueuedTask {
            id: 1,
            project_id: 7,
            iid: 7,
            kind: Issuable::Issue,
            op: WriteOp::AssignSelf,
            queued_at_secs: 100,
        };
        run_worker_one_task(gitlab.clone(), s.clone(), task).await;

        assert_eq!(calls(&gitlab, "assign_self"), 1);
        assert!(
            snapshot_pending(&s).unwrap().is_empty(),
            "task removed after success"
        );
    }

    // ── Concurrency ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn worker_runs_distinct_issuables_concurrently_up_to_the_bound() {
        let (s, _td) = store();
        let gitlab = Arc::new(FakeGitlab::default());
        let gate = gitlab.gate_writes();
        let mut cfg = crate::config::defaults();
        cfg.queue.max_in_flight = 3;
        let Spawned {
            tx,
            handle,
            dir: _dir,
            ..
        } = spawn_worker(cfg, gitlab.clone(), s.clone());

        for iid in 1..=6 {
            s.put(iid as u64, &close_task(iid, 100)).unwrap();
            tx.send(task(iid as u64, iid, WriteOp::Close))
                .await
                .unwrap();
        }
        eventually("three attempts started", || gitlab.writes().len() == 3).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(gitlab.writes().len(), 3, "the bound holds while they run");

        gate.release();
        drop(tx);
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("drained once released")
            .unwrap();
        assert_eq!(gitlab.writes().len(), 6);
        assert!(snapshot_pending(&s).unwrap().is_empty());
    }

    #[tokio::test]
    async fn worker_serializes_writes_to_one_issuable_in_enqueue_order() {
        let (s, _td) = store();
        let gitlab = Arc::new(FakeGitlab::default());
        let gate = gitlab.gate_writes();
        let Spawned {
            tx,
            handle,
            dir: _dir,
            ..
        } = spawn_worker(crate::config::defaults(), gitlab.clone(), s.clone());

        tx.send(task(1, 7, WriteOp::AssignSelf)).await.unwrap();
        tx.send(task(2, 7, WriteOp::UnassignSelf)).await.unwrap();
        eventually("the assign started", || gitlab.writes().len() == 1).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            gitlab.writes(),
            [("assign_self", Issuable::Issue, 7, 7)],
            "the unassign waits for the assign to settle"
        );

        gate.release();
        drop(tx);
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("drained once released")
            .unwrap();
        assert_eq!(
            gitlab.writes(),
            [
                ("assign_self", Issuable::Issue, 7, 7),
                ("unassign_self", Issuable::Issue, 7, 7)
            ]
        );
    }

    #[tokio::test]
    async fn worker_exits_only_after_in_flight_attempts_settle() {
        let (s, _td) = store();
        let gitlab = Arc::new(FakeGitlab::default());
        let gate = gitlab.gate_writes();
        let Spawned {
            tx,
            mut handle,
            dir: _dir,
            ..
        } = spawn_worker(crate::config::defaults(), gitlab.clone(), s.clone());

        tx.send(task(1, 7, WriteOp::Close)).await.unwrap();
        eventually("the attempt started", || gitlab.writes().len() == 1).await;
        drop(tx);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut handle)
                .await
                .is_err(),
            "the sender is gone but the attempt is still running"
        );

        gate.release();
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("exits once the attempt settles")
            .unwrap();
    }

    // ── Backlog schedule (pure) ─────────────────────────────────────────────

    fn pending(id: u64, iid: i64, due: Option<Instant>) -> Pending {
        Pending {
            task: task(id, iid, WriteOp::Close),
            attempt: 0,
            delay: Duration::from_secs(1),
            due,
        }
    }

    #[test]
    fn backlog_runs_distinct_keys_at_once_and_one_key_in_order() {
        let now = Instant::now();
        let mut b = Backlog::default();
        b.push(pending(1, 1, None));
        b.push(pending(2, 1, None));
        b.push(pending(3, 2, None));
        assert_eq!(
            b.eligible(now, 8),
            [1, 3],
            "one per issuable, lowest id first"
        );
        b.start(1);
        b.start(3);
        assert!(b.eligible(now, 8).is_empty(), "both issuables busy");
        b.finish(1);
        assert_eq!(b.eligible(now, 8), [2]);
    }

    #[test]
    fn backlog_honours_the_slot_bound() {
        let now = Instant::now();
        let mut b = Backlog::default();
        for id in 1..=6 {
            b.push(pending(id, id as i64, None));
        }
        assert_eq!(b.eligible(now, 3), [1, 2, 3]);
        assert!(b.eligible(now, 0).is_empty());
    }

    #[test]
    fn a_backing_off_task_blocks_its_later_siblings() {
        let now = Instant::now();
        let later = now + Duration::from_secs(10);
        let mut b = Backlog::default();
        b.push(pending(1, 1, Some(later)));
        b.push(pending(2, 1, None));
        assert!(
            b.eligible(now, 8).is_empty(),
            "the ready sibling waits behind the earlier one"
        );
        assert_eq!(b.eligible(later, 8), [1]);
    }

    #[test]
    fn back_off_takes_the_larger_of_backoff_and_retry_after_capped_by_lifetime() {
        let now = Instant::now();
        let secs = Duration::from_secs;
        let mut b = Backlog::default();
        let mut p = pending(1, 1, None);
        p.delay = secs(4);

        let wait = b.back_off(p, now, Some(secs(10)), secs(7), secs(30));
        assert_eq!(
            wait,
            secs(7),
            "retry_after beats the backoff, the lifetime caps both"
        );
        let p = b.waiting.remove(&1).unwrap();
        assert_eq!(p.due, Some(now + secs(7)));
        assert_eq!(p.delay, secs(8), "doubled for the next failure");

        let wait = b.back_off(p, now, None, secs(3600), secs(30));
        assert_eq!(wait, secs(8));
        let p = b.waiting.remove(&1).unwrap();
        assert_eq!(p.delay, secs(16));

        b.back_off(
            Pending {
                delay: secs(30),
                ..p
            },
            now,
            None,
            secs(3600),
            secs(30),
        );
        assert_eq!(b.waiting[&1].delay, secs(30), "capped at max_delay");
    }

    #[test]
    fn pause_blocks_every_launch_until_it_lifts_and_never_shortens() {
        let now = Instant::now();
        let secs = Duration::from_secs;
        let mut b = Backlog::default();
        b.push(pending(1, 1, None));
        b.push(pending(2, 2, None));

        b.pause_until(now + secs(5));
        assert!(b.eligible(now, 8).is_empty());
        assert_eq!(b.next_wake(now), Some(now + secs(5)));
        b.pause_until(now + secs(2));
        assert_eq!(
            b.next_wake(now),
            Some(now + secs(5)),
            "a shorter pause does not cut the longer one"
        );
        assert_eq!(b.eligible(now + secs(5), 8), [1, 2]);
        assert_eq!(
            b.next_wake(now + secs(5)),
            None,
            "ready tasks need no timer"
        );
    }

    #[test]
    fn next_wake_is_the_earliest_future_due_and_ignores_ready_tasks() {
        let now = Instant::now();
        let secs = Duration::from_secs;
        let mut b = Backlog::default();
        assert_eq!(b.next_wake(now), None, "nothing waiting");
        b.push(pending(1, 1, Some(now + secs(5))));
        b.push(pending(2, 2, Some(now + secs(2))));
        b.push(pending(3, 3, None));
        assert_eq!(b.next_wake(now), Some(now + secs(2)));
        assert_eq!(
            b.next_wake(now + secs(2)),
            Some(now + secs(5)),
            "a due time that passed is no timer"
        );
        assert_eq!(b.next_wake(now + secs(5)), None);
        b.pause_until(now + secs(1));
        assert_eq!(
            b.next_wake(now),
            Some(now + secs(1)),
            "the pause end comes first"
        );
    }

    // ── verdict (pure) ──────────────────────────────────────────────────────

    #[test]
    fn verdict_pauses_on_429_retries_5xx_only_when_idempotent_and_times_out() {
        let post = WriteOp::PostTime {
            duration: "1h".into(),
            summary: None,
            issuable_id: None,
        };
        let window = Duration::from_secs(60);
        let fresh = Duration::ZERO;
        let err = |e: FakeErr| Err(e.error());

        assert!(matches!(
            verdict(Ok(()), &post, fresh, window),
            Verdict::Applied
        ));
        assert!(matches!(
            verdict(err(FakeErr::Throttled(429)), &post, fresh, window),
            Verdict::Retry { pause: true, .. }
        ));
        assert!(matches!(
            verdict(err(FakeErr::Transient), &post, fresh, window),
            Verdict::Retry { pause: false, .. }
        ));
        assert!(matches!(
            verdict(err(FakeErr::Throttled(503)), &WriteOp::Close, fresh, window),
            Verdict::Retry { pause: false, .. }
        ));
        assert!(
            matches!(
                verdict(err(FakeErr::Throttled(503)), &post, fresh, window),
                Verdict::DeadLetter { expired: false, .. }
            ),
            "a 5xx may already have booked the time"
        );
        assert!(matches!(
            verdict(err(FakeErr::Rejected), &WriteOp::Close, fresh, window),
            Verdict::DeadLetter { expired: false, .. }
        ));
        match verdict(err(FakeErr::Transient), &WriteOp::Close, window, window) {
            Verdict::DeadLetter {
                error,
                expired: true,
            } => assert!(error.contains("retry window"), "{error}"),
            other => panic!("expected an expired dead letter, got {other:?}"),
        }
    }

    /// The rejections carry their status since the sync tells a refused
    /// listing by it; a write treats every status alike, as before: a 403
    /// (or 404, 400, 422) is GitLab's final word, dead-lettered at once with
    /// the error as it always read.
    #[test]
    fn verdict_dead_letters_a_refused_write_whatever_its_status() {
        let post = WriteOp::PostTime {
            duration: "1h".into(),
            summary: None,
            issuable_id: None,
        };
        let ops = [
            post,
            WriteOp::Close,
            WriteOp::AssignSelf,
            WriteOp::UnassignSelf,
        ];
        let window = Duration::from_secs(60);
        for status in [403, 404, 400, 422] {
            for op in &ops {
                let e = FakeErr::RejectedWith(status).error();
                assert!(!e.is_retryable(op.idempotent()), "{status} {op:?}");
                match verdict(Err(e), op, Duration::ZERO, window) {
                    Verdict::DeadLetter {
                        error,
                        expired: false,
                    } => assert!(
                        error.starts_with(&format!("GitLab error: {status}")),
                        "{error}"
                    ),
                    other => panic!("{status} {op:?}: expected a dead letter, got {other:?}"),
                }
            }
        }
    }

    // ── Dead-letter store ───────────────────────────────────────────────────

    fn fail_entry(id: i64) -> StoredFailure {
        StoredFailure {
            project_id: id,
            iid: id,
            kind: Issuable::Issue,
            op: WriteOp::Close,
            queued_at_secs: 100,
            failed_at_secs: 200,
            error: "boom".into(),
        }
    }

    fn retry_queue() -> (RetryQueue, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        // Dormant session: the worker defers every task (30s sleep) instead of
        // processing it, so re-enqueued tasks stay put for assertions.
        let session: SessionSlot = Arc::new(tokio::sync::RwLock::new(ConnState::Dormant(
            DormancyReason::NoCredentials,
        )));
        let config = Arc::new(std::sync::RwLock::new(crate::config::defaults()));
        let q = RetryQueue::new(session, &test_db(&dir), config).unwrap();
        (q, dir)
    }

    #[tokio::test]
    async fn worker_dead_letters_on_permanent_error() {
        let (s, _td) = store();
        s.put(1, &post_task(7, 100)).unwrap();

        let gitlab = Arc::new(FakeGitlab::default());
        gitlab.fail_next_write(FakeErr::Rejected);

        let task = QueuedTask {
            id: 1,
            project_id: 7,
            iid: 7,
            kind: Issuable::Issue,
            op: WriteOp::PostTime {
                duration: "1h".into(),
                summary: None,
                issuable_id: None,
            },
            queued_at_secs: 100,
        };
        let failures = run_worker_one_task(gitlab.clone(), s.clone(), task).await;

        assert!(
            snapshot_pending(&s).unwrap().is_empty(),
            "task removed from the live queue"
        );
        assert_eq!(failures.len(), 1, "one dead-letter entry recorded");
        assert_eq!(failures[0].id, 1);
        assert_eq!(failures[0].op_kind, "PostTime");
        assert_eq!(failures[0].kind, Issuable::Issue);
        assert!(
            failures[0].error.contains("403"),
            "error preserved: {}",
            failures[0].error
        );
    }

    #[tokio::test]
    async fn retry_failure_reenqueues_with_original_time_and_clears() {
        let (q, _td) = retry_queue();
        q.dead_letter
            .put(
                5,
                &StoredFailure {
                    project_id: 7,
                    iid: 9,
                    kind: Issuable::Issue,
                    op: WriteOp::Close,
                    queued_at_secs: 1_000,
                    failed_at_secs: 2_000,
                    error: "403".into(),
                },
            )
            .unwrap();

        assert!(q.retry_failure(5).await.unwrap(), "known id retried");
        assert!(
            q.failures().unwrap().is_empty(),
            "dead-letter entry cleared"
        );

        let live: Vec<(i64, i64, u64)> = q
            .store
            .scan(|_, t| Ok((t.project_id, t.iid, t.queued_at_secs)))
            .unwrap();
        assert_eq!(
            live,
            vec![(7, 9, 1_000)],
            "re-enqueued, preserving original queued_at"
        );

        assert!(!q.retry_failure(999).await.unwrap(), "unknown id → false");
    }

    #[tokio::test]
    async fn dismiss_and_clear_failures() {
        let (q, _td) = retry_queue();
        q.dead_letter.put(1, &fail_entry(1)).unwrap();
        q.dead_letter.put(2, &fail_entry(2)).unwrap();

        assert!(q.dismiss_failure(1).unwrap(), "known id dismissed");
        assert!(!q.dismiss_failure(1).unwrap(), "already gone → false");
        assert_eq!(q.failures().unwrap().len(), 1);

        q.clear_failures().unwrap();
        assert!(q.failures().unwrap().is_empty(), "all cleared");
    }

    #[tokio::test]
    async fn new_seeds_next_id_above_both_tables() {
        let dir = tempfile::tempdir().unwrap();
        let db = test_db(&dir);
        let store: KvStore<u64, StoredTask> = KvStore::open_durable(&db, QUEUE_KEYSPACE).unwrap();
        store.put(3, &post_task(1, 100)).unwrap();
        let dl: KvStore<u64, StoredFailure> =
            KvStore::open_durable(&db, DEAD_LETTER_KEYSPACE).unwrap();
        dl.put(10, &fail_entry(10)).unwrap();

        let session: SessionSlot = Arc::new(tokio::sync::RwLock::new(ConnState::Dormant(
            DormancyReason::NoCredentials,
        )));
        let config = Arc::new(std::sync::RwLock::new(crate::config::defaults()));
        let q = RetryQueue::new(session, &db, config).unwrap();
        assert_eq!(
            q.next_id.load(std::sync::atomic::Ordering::Relaxed),
            11,
            "max(queue=3, dead-letter=10) + 1"
        );
    }
}
