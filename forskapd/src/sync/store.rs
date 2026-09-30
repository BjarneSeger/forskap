//! The sync layer's fjall store: one keyspace per GitLab resource, plus the
//! views, job states and identity the scheduler keeps.
//!
//! Only the sync worker writes, through a [`Commit`] that lands rows, scope
//! reconciliation and job state in one atomic batch. Handlers read
//! concurrently without locks and tolerate a store mid-sync.

use std::collections::HashSet;
use std::marker::PhantomData;
use std::ops::Bound;

use fjall::{Database, Keyspace, KeyspaceCreateOptions};
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::avatars::Avatar;
use super::model::{
    Board, Epic, Event, Group, Issue, MergeRequest, Project, Resource, RowKey, Timelog,
};
use super::schedule::{JobState, fingerprint};
use crate::error::Result;
use crate::write::{Write, WriteOp};

const VIEWS_KEYSPACE: &str = "sync_views_v1";
const JOBS_KEYSPACE: &str = "sync_jobs_v1";
const META_KEYSPACE: &str = "sync_meta_v1";
const IDENTITY_KEY: &str = "identity";

/// Big-endian, so byte order is key order and the first half is a prefix.
fn encode(key: RowKey) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&key.0.to_be_bytes());
    out[8..].copy_from_slice(&key.1.to_be_bytes());
    out
}

fn decode(bytes: &[u8]) -> Option<RowKey> {
    let hi = u64::from_be_bytes(bytes.get(..8)?.try_into().ok()?);
    let lo = u64::from_be_bytes(bytes.get(8..16)?.try_into().ok()?);
    Some((hi, lo))
}

/// Which rows of a table a reconcile or removal covers, by the first key half.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowScope {
    All,
    /// First half equal (one project's issues, MRs or boards, one group's
    /// epics).
    Prefix(u64),
    /// First half ≥ (timelogs/events at or after a time).
    Since(u64),
    /// First half < (timelogs/events older than a time).
    Before(u64),
}

impl RowScope {
    fn bounds(self) -> (Bound<[u8; 16]>, Bound<[u8; 16]>) {
        use Bound::{Excluded, Included, Unbounded};
        match self {
            Self::All => (Unbounded, Unbounded),
            Self::Prefix(hi) => (Included(encode((hi, 0))), Included(encode((hi, u64::MAX)))),
            Self::Since(hi) => (Included(encode((hi, 0))), Unbounded),
            Self::Before(hi) => (Unbounded, Excluded(encode((hi, 0)))),
        }
    }
}

/// The rows of one resource.
pub struct Table<R> {
    ks: Keyspace,
    _rows: PhantomData<fn() -> R>,
}

impl<R: Resource> Table<R> {
    fn open(db: &Database) -> Result<Self> {
        Ok(Self {
            ks: db.keyspace(R::KEYSPACE, KeyspaceCreateOptions::default)?,
            _rows: PhantomData,
        })
    }

    pub fn get(&self, key: RowKey) -> Result<Option<R>> {
        Ok(self
            .ks
            .get(encode(key))?
            .and_then(|bytes| parse_row(&bytes)))
    }

    /// Every row in `scope`, in key order. A row that no longer parses is
    /// skipped with a warning rather than hiding the rest.
    pub fn scan(&self, scope: RowScope) -> Result<Vec<R>> {
        let mut out = Vec::new();
        for guard in self.ks.range(scope.bounds()) {
            let (_, v) = guard.into_inner()?;
            out.extend(parse_row(&v));
        }
        Ok(out)
    }

    /// The keys in `scope`, without decoding rows.
    pub fn keys(&self, scope: RowScope) -> Result<Vec<RowKey>> {
        let mut out = Vec::new();
        for guard in self.ks.range(scope.bounds()) {
            out.extend(decode(&guard.key()?));
        }
        Ok(out)
    }
}

fn parse_row<R: Resource>(bytes: &[u8]) -> Option<R> {
    match serde_json::from_slice(bytes) {
        Ok(row) => Some(row),
        Err(e) => {
            warn!(error = %e, kind = R::NAME, "skipping unreadable stored row");
            None
        }
    }
}

/// Access to a resource's table, so generic code can reach it from the store.
pub trait Stored: Resource {
    fn table(store: &SyncStore) -> &Table<Self>;
}

macro_rules! stored {
    ($($ty:ty => $field:ident),* $(,)?) => {$(
        impl Stored for $ty {
            fn table(store: &SyncStore) -> &Table<Self> {
                &store.$field
            }
        }
    )*};
}

stored!(
    Issue => issues,
    MergeRequest => merge_requests,
    Project => projects,
    Group => groups,
    Epic => epics,
    Board => boards,
    Event => events,
    Timelog => timelogs,
    NotedWrite => noted,
    Avatar => avatars,
);

/// A write GitLab applied at `at`. Kept so views fetched before it are
/// corrected at read time, after a restart too.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotedWrite {
    pub write: Write,
    pub at: u64,
}

impl Resource for NotedWrite {
    const NAME: &'static str = "noted writes";
    const KEYSPACE: &'static str = "sync_noted_v1";
    const SCHEMA: u32 = 1;
    /// By time, then by what was written, so one write noted twice in a
    /// second is one row.
    fn key(&self) -> RowKey {
        let w = &self.write;
        let op = match w.op {
            WriteOp::PostTime { .. } => 0,
            WriteOp::Close => 1,
            WriteOp::AssignSelf => 2,
            WriteOp::UnassignSelf => 3,
        };
        let what = fingerprint(&[w.kind as u64, w.project_id as u64, w.iid as u64, op]);
        (self.at, what)
    }
    fn is_valid(&self) -> bool {
        true
    }
}

/// The ordered keys a listing returned whose rows can't be told apart by
/// their own fields (e.g. "assigned to me"), plus when that fetch started.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct View {
    pub keys: Vec<RowKey>,
    /// Start of the fetch that produced the view; a write applied after it
    /// may not be reflected yet.
    pub fetched_at: u64,
}

/// Whose data the store holds. A different account wipes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub host: String,
    pub user_id: i64,
}

pub struct SyncStore {
    db: Database,
    pub issues: Table<Issue>,
    pub merge_requests: Table<MergeRequest>,
    pub projects: Table<Project>,
    pub groups: Table<Group>,
    pub epics: Table<Epic>,
    pub boards: Table<Board>,
    pub events: Table<Event>,
    pub timelogs: Table<Timelog>,
    pub noted: Table<NotedWrite>,
    pub avatars: Table<Avatar>,
    views: Keyspace,
    jobs: Keyspace,
    meta: Keyspace,
}

impl SyncStore {
    /// Writes are lazily durable: everything here is re-fetchable.
    pub fn open(db: &Database) -> Result<Self> {
        let ks = |name| db.keyspace(name, KeyspaceCreateOptions::default);
        Ok(Self {
            db: db.clone(),
            issues: Table::open(db)?,
            merge_requests: Table::open(db)?,
            projects: Table::open(db)?,
            groups: Table::open(db)?,
            epics: Table::open(db)?,
            boards: Table::open(db)?,
            events: Table::open(db)?,
            timelogs: Table::open(db)?,
            noted: Table::open(db)?,
            avatars: Table::open(db)?,
            views: ks(VIEWS_KEYSPACE)?,
            jobs: ks(JOBS_KEYSPACE)?,
            meta: ks(META_KEYSPACE)?,
        })
    }

    pub fn table<R: Stored>(&self) -> &Table<R> {
        R::table(self)
    }

    pub fn view(&self, name: &str) -> Result<Option<View>> {
        read_json(&self.views, name)
    }

    /// The persisted state of the job keyed `key`; never-run jobs read as
    /// the default.
    pub fn job_state(&self, key: &str) -> Result<JobState> {
        Ok(read_json(&self.jobs, key)?.unwrap_or_default())
    }

    /// Every persisted job state, by key.
    pub fn job_states(&self) -> Result<Vec<(String, JobState)>> {
        let mut out = Vec::new();
        for guard in self.jobs.iter() {
            let (k, v) = guard.into_inner()?;
            let key = String::from_utf8_lossy(&k).into_owned();
            match serde_json::from_slice(&v) {
                Ok(state) => out.push((key, state)),
                Err(e) => warn!(error = %e, key, "skipping unreadable job state"),
            }
        }
        Ok(out)
    }

    pub fn identity(&self) -> Result<Option<Identity>> {
        read_json(&self.meta, IDENTITY_KEY)
    }

    pub fn begin(&self) -> Commit<'_> {
        Commit {
            store: self,
            batch: self.db.batch(),
        }
    }
}

fn read_json<T: serde::de::DeserializeOwned>(ks: &Keyspace, key: &str) -> Result<Option<T>> {
    let Some(bytes) = ks.get(key)? else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_slice(&bytes)?))
}

/// One atomic batch of sync writes. Nothing lands until [`Commit::commit`];
/// dropping it discards everything staged.
pub struct Commit<'a> {
    store: &'a SyncStore,
    batch: fjall::OwnedWriteBatch,
}

impl Commit<'_> {
    pub fn upsert<R: Stored>(&mut self, rows: &[R]) -> Result<()> {
        let table = self.store.table::<R>();
        for row in rows {
            self.batch
                .insert(&table.ks, encode(row.key()), serde_json::to_vec(row)?);
        }
        Ok(())
    }

    pub fn remove<R: Stored>(&mut self, key: RowKey) {
        self.batch.remove(&self.store.table::<R>().ks, encode(key));
    }

    /// Remove every row in `scope` whose key `keep` rejects; returns how many.
    pub fn remove_where<R: Stored>(
        &mut self,
        scope: RowScope,
        keep: impl Fn(RowKey) -> bool,
    ) -> Result<usize> {
        let table = self.store.table::<R>();
        let mut removed = 0;
        for key in table.keys(scope)? {
            if !keep(key) {
                self.batch.remove(&table.ks, encode(key));
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Make `scope` hold exactly the rows `fetched` upserts: drop the rest.
    pub fn reconcile<R: Stored>(&mut self, scope: RowScope, fetched: &[R]) -> Result<usize> {
        let keep: HashSet<RowKey> = fetched.iter().map(Resource::key).collect();
        self.remove_where::<R>(scope, |k| keep.contains(&k))
    }

    /// The view `name` as committed, without this batch's changes.
    pub fn view(&self, name: &str) -> Result<Option<View>> {
        self.store.view(name)
    }

    /// The row at `key` as committed, without this batch's changes.
    pub fn get<R: Stored>(&self, key: RowKey) -> Result<Option<R>> {
        self.store.table::<R>().get(key)
    }

    /// The state of the job keyed `key` as committed.
    pub fn job_state(&self, key: &str) -> Result<JobState> {
        self.store.job_state(key)
    }

    pub fn set_view(&mut self, name: &str, view: &View) -> Result<()> {
        self.batch
            .insert(&self.store.views, name, serde_json::to_vec(view)?);
        Ok(())
    }

    pub fn remove_view(&mut self, name: &str) {
        self.batch.remove(&self.store.views, name);
    }

    pub fn set_job(&mut self, key: &str, state: &JobState) -> Result<()> {
        self.batch
            .insert(&self.store.jobs, key, serde_json::to_vec(state)?);
        Ok(())
    }

    pub fn remove_job(&mut self, key: &str) {
        self.batch.remove(&self.store.jobs, key);
    }

    pub fn set_identity(&mut self, identity: &Identity) -> Result<()> {
        self.batch.insert(
            &self.store.meta,
            IDENTITY_KEY,
            serde_json::to_vec(identity)?,
        );
        Ok(())
    }

    /// Drop every row, view and job state; the identity stays. The avatar
    /// files go with the worker's next sweep.
    pub fn wipe(&mut self) -> Result<()> {
        self.remove_where::<Issue>(RowScope::All, |_| false)?;
        self.remove_where::<MergeRequest>(RowScope::All, |_| false)?;
        self.remove_where::<Project>(RowScope::All, |_| false)?;
        self.remove_where::<Group>(RowScope::All, |_| false)?;
        self.remove_where::<Epic>(RowScope::All, |_| false)?;
        self.remove_where::<Board>(RowScope::All, |_| false)?;
        self.remove_where::<Event>(RowScope::All, |_| false)?;
        self.remove_where::<Timelog>(RowScope::All, |_| false)?;
        self.remove_where::<NotedWrite>(RowScope::All, |_| false)?;
        self.remove_where::<Avatar>(RowScope::All, |_| false)?;
        for ks in [&self.store.views, &self.store.jobs] {
            for guard in ks.iter() {
                self.batch.remove(ks, guard.key()?);
            }
        }
        Ok(())
    }

    pub fn commit(self) -> Result<()> {
        Ok(self.batch.commit()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    pub(crate) fn store() -> (SyncStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::builder(dir.path().join("db")).open().unwrap();
        (SyncStore::open(&db).unwrap(), dir)
    }

    fn issue(project_id: i64, iid: i64, title: &str) -> Issue {
        Issue {
            id: project_id * 1000 + iid,
            iid,
            project_id,
            title: title.into(),
            ..Default::default()
        }
    }

    fn timelog(id: u64, spent_at: u64) -> Timelog {
        Timelog {
            id,
            spent_at,
            iid: 1,
            ..Default::default()
        }
    }

    #[test]
    fn nothing_lands_until_commit() {
        let (s, _d) = store();
        let mut c = s.begin();
        c.upsert(&[issue(1, 1, "a")]).unwrap();
        c.set_job(
            "assigned/issues",
            &JobState {
                last_ok: 5,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(s.issues.get((1, 1)).unwrap().is_none());
        drop(c);
        assert!(
            s.issues.get((1, 1)).unwrap().is_none(),
            "dropped batch discards"
        );
        assert_eq!(s.job_state("assigned/issues").unwrap(), JobState::default());

        let mut c = s.begin();
        c.upsert(&[issue(1, 1, "a")]).unwrap();
        c.set_job(
            "assigned/issues",
            &JobState {
                last_ok: 5,
                ..Default::default()
            },
        )
        .unwrap();
        c.commit().unwrap();
        assert_eq!(s.issues.get((1, 1)).unwrap().unwrap().title, "a");
        assert_eq!(s.job_state("assigned/issues").unwrap().last_ok, 5);
    }

    #[test]
    fn prefix_scope_is_one_project() {
        let (s, _d) = store();
        let mut c = s.begin();
        c.upsert(&[
            issue(1, 1, "a"),
            issue(2, 1, "b"),
            issue(2, 9, "c"),
            issue(3, 1, "d"),
        ])
        .unwrap();
        c.commit().unwrap();
        let titles: Vec<String> = s
            .issues
            .scan(RowScope::Prefix(2))
            .unwrap()
            .into_iter()
            .map(|i| i.title)
            .collect();
        assert_eq!(titles, ["b", "c"]);
    }

    #[test]
    fn unreadable_rows_are_skipped_not_fatal() {
        let (s, _d) = store();
        let mut c = s.begin();
        c.upsert(&[issue(1, 1, "a"), issue(1, 3, "c")]).unwrap();
        c.commit().unwrap();
        s.issues
            .ks
            .insert(encode((1, 2)), b"not json".to_vec())
            .unwrap();
        assert_eq!(s.issues.scan(RowScope::All).unwrap().len(), 2);
        assert!(s.issues.get((1, 2)).unwrap().is_none());
    }

    #[test]
    fn wipe_keeps_the_identity() {
        let (s, _d) = store();
        let me = Identity {
            host: "gl".into(),
            user_id: 7,
        };
        let mut c = s.begin();
        c.upsert(&[issue(1, 1, "a")]).unwrap();
        c.upsert(&[timelog(1, 10)]).unwrap();
        c.upsert(&[Avatar {
            project_id: 1,
            file: "1-1.png".into(),
        }])
        .unwrap();
        c.set_view("v", &View::default()).unwrap();
        c.set_job(
            "j",
            &JobState {
                last_ok: 1,
                ..Default::default()
            },
        )
        .unwrap();
        c.set_identity(&me).unwrap();
        c.commit().unwrap();

        let mut c = s.begin();
        c.wipe().unwrap();
        c.commit().unwrap();
        assert!(s.issues.scan(RowScope::All).unwrap().is_empty());
        assert!(s.timelogs.scan(RowScope::All).unwrap().is_empty());
        assert!(s.avatars.scan(RowScope::All).unwrap().is_empty());
        assert!(s.view("v").unwrap().is_none());
        assert!(s.job_states().unwrap().is_empty());
        assert_eq!(s.identity().unwrap(), Some(me));
    }

    proptest! {
        /// Reconciling a scope removes exactly the unfetched rows inside it
        /// and never touches rows outside.
        #[test]
        fn reconcile_touches_only_its_scope(
            stored in proptest::collection::btree_set((1u64..4, 1u64..6), 0..20),
            fetched in proptest::collection::btree_set(1u64..6, 0..6),
            project in 1u64..4,
        ) {
            let (s, _d) = store();
            let rows: Vec<Issue> = stored
                .iter()
                .map(|&(p, i)| issue(p as i64, i as i64, "x"))
                .collect();
            let mut c = s.begin();
            c.upsert(&rows).unwrap();
            c.commit().unwrap();

            let fresh: Vec<Issue> = fetched
                .iter()
                .map(|&i| issue(project as i64, i as i64, "new"))
                .collect();
            let mut c = s.begin();
            c.upsert(&fresh).unwrap();
            c.reconcile(RowScope::Prefix(project), &fresh).unwrap();
            c.commit().unwrap();

            let after: std::collections::BTreeSet<RowKey> =
                s.issues.keys(RowScope::All).unwrap().into_iter().collect();
            let expected: std::collections::BTreeSet<RowKey> = stored
                .iter()
                .copied()
                .filter(|&(p, _)| p != project)
                .chain(fetched.iter().map(|&i| (project, i)))
                .collect();
            prop_assert_eq!(after, expected);
        }

        #[test]
        fn time_scopes_split_at_the_boundary(
            times in proptest::collection::btree_set(0u64..1000, 0..30),
            cut in 0u64..1000,
        ) {
            let (s, _d) = store();
            let rows: Vec<Timelog> = times.iter().map(|&t| timelog(t + 1, t)).collect();
            let mut c = s.begin();
            c.upsert(&rows).unwrap();
            c.commit().unwrap();
            let since = s.timelogs.keys(RowScope::Since(cut)).unwrap();
            let before = s.timelogs.keys(RowScope::Before(cut)).unwrap();
            prop_assert!(since.iter().all(|k| k.0 >= cut));
            prop_assert!(before.iter().all(|k| k.0 < cut));
            prop_assert_eq!(since.len() + before.len(), times.len());
        }
    }
}
