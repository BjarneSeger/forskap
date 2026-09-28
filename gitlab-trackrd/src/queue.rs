//! Background retry queue for outgoing write operations.
//!
//! Tasks are persisted via `KvStore` before being processed, so they survive
//! daemon restarts.  A background tokio task works through the queue with
//! exponential backoff (1 s base, 30 min cap).  Network errors, 429s, and 5xx
//! on idempotent ops trigger retries for up to 7 days; a GitLab rejection or
//! an exhausted retry window moves the task to a persistent dead-letter
//! store, surfaced via `tt queue`. Either way the settle hook hears about it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, mpsc};
use tracing::{error, info, warn};

use crate::config::{SharedConfig, next_backoff};
use crate::db::KvStore;
use crate::error::Result;
use crate::gitlab::Issuable;
use crate::handlers::SessionSlot;
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

impl QueuedTask {
    fn write(&self) -> Write {
        Write {
            kind: self.kind,
            project_id: self.project_id,
            iid: self.iid,
            op: self.op.clone(),
        }
    }
}

/// Told when the worker settles a queued write: `true` once GitLab applied
/// it, `false` when it was dead-lettered.
pub type SettleHook = Arc<dyn Fn(&Write, bool) + Send + Sync>;

pub struct RetryQueue {
    sender: mpsc::Sender<QueuedTask>,
    store: KvStore<u64, StoredTask>,
    dead_letter: KvStore<u64, StoredFailure>,
    next_id: AtomicU64,
    /// Fired to wake the worker early while it is deferring a task for lack of a
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

/// A dead-lettered task, projected for the `tt queue` view.
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
        // IDs stay monotonic across restarts (the `tt tick` notice dedupes by ID).
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

        if !initial_tasks.is_empty() {
            let tx_init = tx.clone();
            tokio::spawn(async move {
                for task in initial_tasks {
                    if tx_init.send(task).await.is_err() {
                        break;
                    }
                }
            });
        }

        let drain_wake = Arc::new(Notify::new());
        let settle_hook = Arc::new(OnceLock::new());

        tokio::spawn(worker(
            session,
            store.clone(),
            dead_letter.clone(),
            rx,
            config,
            Arc::clone(&drain_wake),
            Arc::clone(&settle_hook),
        ));

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

async fn worker(
    session: SessionSlot,
    store: KvStore<u64, StoredTask>,
    dead_letter: KvStore<u64, StoredFailure>,
    mut rx: mpsc::Receiver<QueuedTask>,
    config: SharedConfig,
    drain_wake: Arc<Notify>,
    settle_hook: Arc<OnceLock<SettleHook>>,
) {
    while let Some(task) = rx.recv().await {
        let mut delay = config.read().unwrap().queue.base_delay();
        let mut attempt = 0u32;

        // `None` ⇒ succeeded; `Some(msg)` ⇒ gave up and should be dead-lettered.
        let failure: Option<String> = 'retry: loop {
            attempt += 1;
            // Bind the clone in its own statement so the read guard is released
            // here — not held across the `select!` below or the API call. A
            // guard held across the defer would block every session *writer*
            // (the reconnect commit, `tt login`) for up to `session_wait`.
            let current = session.read().await.gitlab();
            let gitlab = match current {
                Some(g) => g,
                None => {
                    warn!(
                        attempt,
                        project_id = task.project_id,
                        iid = task.iid,
                        kind = ?task.kind,
                        op = task.op.name(),
                        "no active session; deferring task"
                    );
                    let session_wait = config.read().unwrap().queue.session_wait();
                    // Wake early if a reconnect re-established the session, so a
                    // deferred task flushes at once instead of waiting out the
                    // full interval. The waker uses `notify_one`, which leaves a
                    // permit if we haven't parked here yet — so a nudge fired the
                    // instant before this `select!` is still delivered on entry.
                    tokio::select! {
                        _ = tokio::time::sleep(session_wait) => {}
                        _ = drain_wake.notified() => {}
                    }
                    continue 'retry;
                }
            };
            let outcome = task
                .write()
                .apply(&*gitlab, Some(task.queued_at_secs))
                .await;

            match outcome {
                Ok(()) => {
                    if attempt > 1 {
                        info!(
                            attempt,
                            project_id = task.project_id,
                            iid = task.iid,
                            kind = ?task.kind,
                            op = task.op.name(),
                            "task succeeded after retry"
                        );
                    }
                    break 'retry None;
                }
                Err(e) if e.is_retryable(task.op.idempotent()) => {
                    let elapsed =
                        Duration::from_secs(now_secs().saturating_sub(task.queued_at_secs));
                    let max_lifetime = config.read().unwrap().queue.max_lifetime();
                    if elapsed >= max_lifetime {
                        error!(
                            attempt,
                            error = %e,
                            project_id = task.project_id,
                            iid = task.iid,
                            kind = ?task.kind,
                            op = task.op.name(),
                            retry_window = max_lifetime.as_secs(),
                            "dropping task after retry window"
                        );
                        break 'retry Some(format!(
                            "timed out after {}, seconds retry window: {}",
                            max_lifetime.as_secs(),
                            e
                        ));
                    }
                    let sleep = delay
                        .max(e.retry_after().unwrap_or_default())
                        .min(max_lifetime.checked_sub(elapsed).unwrap());
                    warn!(
                        attempt,
                        error = %e,
                        delay_secs = sleep.as_secs(),
                        project_id = task.project_id,
                        op = task.op.name(),
                        "task failed transiently, retrying"
                    );
                    tokio::time::sleep(sleep).await;
                    let max = config.read().unwrap().queue.max_delay();
                    delay = next_backoff(delay, max);
                }
                Err(e) => {
                    error!(
                        error = %e,
                        project_id = task.project_id,
                        iid = task.iid,
                        kind = ?task.kind,
                        op = task.op.name(),
                        "task rejected by GitLab; dropping"
                    );
                    break 'retry Some(e.to_string());
                }
            }
        };

        // The task left the live queue either way. If it failed permanently,
        // record it in the dead-letter store (keyed by its id) so the user can
        // see, retry, or dismiss it via `tt queue`.
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
            if let Err(e) = dead_letter.put(task.id, &stored) {
                warn!(
                    error = %e,
                    task_id = task.id,
                    "failed to record dead-letter entry"
                );
            }
        }

        if let Err(e) = store.remove(task.id) {
            warn!(
                error = %e,
                task_id = task.id,
                "failed to remove completed task from queue db"
            );
        }
        if let Some(hook) = settle_hook.get() {
            hook(&task.write(), applied);
        }
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::DormancyReason;
    use crate::gitlab::GitlabApi;
    use crate::handlers::{ConnState, Session};
    use crate::testing::{FakeErr, FakeGitlab};

    // The max lifetime cutoff and exponential backoff are not exercised
    // here. Both depend on `SystemTime::now()` rather than tokio's mock clock,
    // so deterministically driving them would require a clock-injection
    // abstraction that isn't warranted for the gain.

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

    async fn run_worker_one_task_with(
        cfg: crate::config::Config,
        gitlab: Arc<dyn GitlabApi>,
        store: KvStore<u64, StoredTask>,
        task: QueuedTask,
    ) -> Vec<FailedTaskView> {
        let dir = tempfile::tempdir().unwrap();
        let dead_letter = KvStore::open_durable(&test_db(&dir), DEAD_LETTER_KEYSPACE).unwrap();
        let session: SessionSlot =
            Arc::new(tokio::sync::RwLock::new(ConnState::Connected(Session {
                gitlab,
                host: "test".to_string(),
                user_id: 0,
            })));
        let (tx, rx) = mpsc::channel(8);
        let config = Arc::new(std::sync::RwLock::new(cfg));
        let drain_wake = Arc::new(Notify::new());
        let handle = tokio::spawn(worker(
            session,
            store,
            dead_letter.clone(),
            rx,
            config,
            drain_wake,
            Arc::new(OnceLock::new()),
        ));
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
            })));
        let settled = Arc::new(std::sync::Mutex::new(Vec::new()));
        let hook_cell = Arc::new(OnceLock::new());
        let seen = Arc::clone(&settled);
        let hook: SettleHook = Arc::new(move |w: &Write, applied| {
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
        for iid in [1, 2] {
            tx.send(QueuedTask {
                id: iid as u64,
                project_id: 7,
                iid,
                kind: Issuable::Issue,
                op: WriteOp::Close,
                queued_at_secs: now_secs(),
            })
            .await
            .unwrap();
        }
        drop(tx);
        handle.await.unwrap();

        assert_eq!(
            *settled.lock().unwrap(),
            [("Close", 1, false), ("Close", 2, true)],
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
