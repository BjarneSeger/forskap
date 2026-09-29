//! Persisted open statistics: how often, and how recently, each issue/MR was
//! opened through `RecordOpen`. The `Search` handler ranks frequently opened
//! items first from this record; nothing else reads it.
//!
//! One JSON record in its own keyspace: a read-modify-write under an internal
//! lock, lazy durability. Opens are human-rate events and the record is capped, so
//! rewriting it whole is cheaper than a per-entry keyspace would be. There
//! is no background pruning — every write drops entries past the retention
//! cutoff and enforces the cap, so the record stays bounded without a timer
//! and regardless of session state.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::db::KvStore;
use crate::error::Result;
use crate::gitlab::Issuable;

/// Hard cap on tracked issuables; the lowest-count entries go first.
pub const MAX_ENTRIES: usize = 1000;

const USAGE_KEYSPACE: &str = "open_usage_v1";
const KEY: &str = "opens";

/// Per-issuable counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageEntry {
    pub count: u64,
    pub last_opened_secs: u64,
}

/// The whole record, keyed by [`usage_key`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageRecord {
    pub entries: BTreeMap<String, UsageEntry>,
}

impl UsageRecord {
    /// Counters for one issuable, if it was ever opened.
    pub fn get(&self, kind: Issuable, project_id: i64, iid: i64) -> Option<UsageEntry> {
        self.entries.get(&usage_key(kind, project_id, iid)).copied()
    }
}

/// Record key for an issuable: `issues:<project_id>:<iid>` /
/// `merge_requests:<project_id>:<iid>`.
pub fn usage_key(kind: Issuable, project_id: i64, iid: i64) -> String {
    format!("{}:{project_id}:{iid}", kind.path_segment())
}

/// fjall-backed store for the single [`UsageRecord`].
pub struct UsageStats {
    store: KvStore<&'static str, UsageRecord>,
    /// Serializes the read-modify-write in [`Self::record`].
    write_lock: std::sync::Mutex<()>,
}

impl UsageStats {
    /// Lazy durability — losing the newest open on power failure costs one
    /// count.
    pub fn open(db: &fjall::Database) -> Result<Self> {
        Ok(Self {
            store: KvStore::open(db, USAGE_KEYSPACE)?,
            write_lock: std::sync::Mutex::new(()),
        })
    }

    /// The current record (empty when nothing was ever opened).
    pub fn snapshot(&self) -> Result<UsageRecord> {
        Ok(self.store.get(KEY)?.unwrap_or_default())
    }

    /// Count one open of `key` at `now`, then drop entries last opened before
    /// `cutoff` and enforce [`MAX_ENTRIES`].
    pub fn record(&self, key: &str, now: u64, cutoff: u64) -> Result<()> {
        let _lock = self.write_lock.lock().unwrap();
        let mut record = self.store.get(KEY)?.unwrap_or_default();
        let entry = record.entries.entry(key.to_string()).or_default();
        entry.count += 1;
        entry.last_opened_secs = now;
        prune(&mut record, cutoff);
        self.store.put(KEY, &record)
    }

    /// Forget every open.
    pub fn clear(&self) -> Result<()> {
        self.store.clear()
    }
}

/// Drop entries older than `cutoff`, then the lowest-count ones past the cap
/// (ties broken by staleness).
fn prune(record: &mut UsageRecord, cutoff: u64) {
    record.entries.retain(|_, e| e.last_opened_secs >= cutoff);
    let excess = record.entries.len().saturating_sub(MAX_ENTRIES);
    if excess == 0 {
        return;
    }
    let mut ranked: Vec<(u64, u64, String)> = record
        .entries
        .iter()
        .map(|(k, e)| (e.count, e.last_opened_secs, k.clone()))
        .collect();
    ranked.sort();
    for (_, _, key) in ranked.into_iter().take(excess) {
        record.entries.remove(&key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats() -> (UsageStats, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = fjall::Database::builder(dir.path().join("db"))
            .open()
            .unwrap();
        (UsageStats::open(&db).unwrap(), dir)
    }

    #[test]
    fn usage_key_is_kind_scoped() {
        assert_eq!(usage_key(Issuable::Issue, 7, 42), "issues:7:42");
        assert_eq!(
            usage_key(Issuable::MergeRequest, 7, 42),
            "merge_requests:7:42"
        );
    }

    #[test]
    fn snapshot_defaults_to_empty() {
        let (s, _td) = stats();
        assert!(s.snapshot().unwrap().entries.is_empty());
    }

    #[test]
    fn record_counts_and_stamps_last_open() {
        let (s, _td) = stats();
        let key = usage_key(Issuable::Issue, 1, 2);
        s.record(&key, 100, 0).unwrap();
        s.record(&key, 200, 0).unwrap();
        let e = s.snapshot().unwrap().get(Issuable::Issue, 1, 2).unwrap();
        assert_eq!(e.count, 2);
        assert_eq!(e.last_opened_secs, 200);
        assert!(
            s.snapshot()
                .unwrap()
                .get(Issuable::MergeRequest, 1, 2)
                .is_none(),
            "kinds don't share counters"
        );
    }

    #[test]
    fn record_prunes_entries_past_the_cutoff() {
        let (s, _td) = stats();
        s.record("issues:1:1", 100, 0).unwrap();
        // The cutoff applies to everything, including entries written earlier.
        s.record("issues:1:2", 500, 400).unwrap();
        let r = s.snapshot().unwrap();
        assert!(
            r.get(Issuable::Issue, 1, 1).is_none(),
            "stale entry dropped"
        );
        assert!(r.get(Issuable::Issue, 1, 2).is_some());
    }

    #[test]
    fn prune_caps_the_record_dropping_lowest_counts_first() {
        let mut r = UsageRecord::default();
        for i in 0..(MAX_ENTRIES as u64 + 2) {
            r.entries.insert(
                format!("issues:1:{i}"),
                UsageEntry {
                    count: i + 1,
                    last_opened_secs: 10,
                },
            );
        }
        prune(&mut r, 0);
        assert_eq!(r.entries.len(), MAX_ENTRIES);
        assert!(r.entries.get("issues:1:0").is_none(), "count 1 dropped");
        assert!(r.entries.get("issues:1:1").is_none(), "count 2 dropped");
        assert!(r.entries.get("issues:1:2").is_some());
    }

    #[test]
    fn clear_forgets_everything() {
        let (s, _td) = stats();
        s.record("issues:1:1", 1, 0).unwrap();
        s.clear().unwrap();
        assert!(s.snapshot().unwrap().entries.is_empty());
    }

    #[test]
    fn survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        {
            let db = fjall::Database::builder(&path).open().unwrap();
            let s = UsageStats::open(&db).unwrap();
            s.record("issues:1:1", 7, 0).unwrap();
        }
        let db = fjall::Database::builder(&path).open().unwrap();
        let s = UsageStats::open(&db).unwrap();
        assert_eq!(
            s.snapshot()
                .unwrap()
                .get(Issuable::Issue, 1, 1)
                .unwrap()
                .count,
            1
        );
    }
}
