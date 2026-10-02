//! Shared scaffolding for the Criterion benches: a dormant `Handlers` on a
//! temp-dir fjall database plus scale-parameterized corpus generators.
//!
//! Deliberately independent of the `#[cfg(test)]` fixtures in
//! `handlers/tests.rs` — benches are separate compilation units that link the
//! library without `cfg(test)`, so those helpers are invisible here. The
//! session is always dormant: every benched path is a pure store read, and
//! dormancy proves no network access is possible (the sync worker parks).
#![allow(dead_code)] // each bench target compiles this module independently

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::{Notify, RwLock};

use forskapd::config::SharedConfig;
use forskapd::error::DormancyReason;
use forskapd::gitlab::Issuable;
use forskapd::handlers::{ConnState, Handlers, SessionSlot};
use forskapd::queue::RetryQueue;
use forskapd::secrets::Keychain;
use forskapd::sync::jobs::ASSIGNED_MERGE_REQUESTS;
use forskapd::sync::model::{
    Board, BoardList, Epic, Group, Issue, LabelRef, MergeRequest, Project, Resource, Timelog,
    UserRef,
};
use forskapd::sync::schedule::JobState;
use forskapd::sync::store::{Stored, SyncStore, View};
use forskapd::sync::{Job, SyncHandle};
use forskapd::usage::UsageStats;

/// A `Handlers` on a fresh temp-dir fjall database. The `TempDir` is bundled
/// so it outlives the stores — dropping it deletes the database out from
/// under fjall. The runtime is bundled too: the queue and sync workers are
/// spawned at construction, and the async handler benches drive their
/// futures on the same runtime (`b.to_async(&env.rt)`).
pub struct BenchEnv {
    pub h: Handlers,
    pub rt: tokio::runtime::Runtime,
    _dir: tempfile::TempDir,
}

impl BenchEnv {
    pub fn store(&self) -> &SyncStore {
        self.h.sync.store()
    }
}

pub fn dormant_env() -> BenchEnv {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let _guard = rt.enter();
    let dir = tempfile::tempdir().unwrap();
    let db = fjall::Database::builder(dir.path().join("db"))
        .open()
        .unwrap();
    let session: SessionSlot = Arc::new(RwLock::new(ConnState::Dormant(
        DormancyReason::NoCredentials,
    )));
    let config: SharedConfig = Arc::new(std::sync::RwLock::new(forskapd::config::defaults()));
    let reconnect_signal = Arc::new(Notify::new());
    // A bench never reaches the OS keychain.
    let keychain = Keychain::disabled();
    let sync = SyncHandle::spawn(
        Arc::new(SyncStore::open(&db).unwrap()),
        forskapd::sync::AvatarDir::new(dir.path().join("avatars")),
        Arc::clone(&session),
        Arc::clone(&config),
        Arc::clone(&reconnect_signal),
        forskapd::reconnect::keychain_probe(keychain.clone()),
    );
    let queue = RetryQueue::new(Arc::clone(&session), &db, Arc::clone(&config)).unwrap();
    let usage = Arc::new(UsageStats::open(&db).unwrap());
    drop(_guard);
    BenchEnv {
        h: Handlers {
            session,
            sync,
            usage,
            queue,
            config,
            reconnect_signal,
            rotation: Default::default(),
            keychain,
        },
        rt,
        _dir: dir,
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Knuth-multiplicative shuffle so values derived from a sequential `i` are
/// non-monotonic — fjall iterates in key order, and a pre-sorted corpus would
/// flatter the `sort_by_key(Reverse(..))` in the handlers.
pub fn shuffled(i: u64, range: u64) -> u64 {
    i.wrapping_mul(2654435761) % range
}

const LABELS: [&str; 6] = ["bug", "feature", "backend", "urgent", "docs", "infra"];

/// 0–3 labels per entry so label matching does real work.
fn label_set(i: u64) -> Vec<String> {
    (0..i % 4)
        .map(|k| LABELS[((i + k) % 6) as usize].to_string())
        .collect()
}

/// ~1% of titles contain the marker `flaky` (the hit-variant needle); the
/// rest say `steady`. Component suffix varies so substring scans can't
/// short-circuit on a shared prefix.
fn title(i: u64, noun: &str) -> String {
    let marker = if i % 100 == 0 { "flaky" } else { "steady" };
    format!("{noun} {i}: {marker} sync in component {}", i % 97)
}

/// Half the corpus lives under the `team` namespace, half under `other`, so
/// group filters are selective.
fn namespace(i: u64) -> &'static str {
    if i % 2 == 0 { "team" } else { "other" }
}

/// 50 projects; `(project, iid)` is unique per `i`.
fn project_of(i: u64) -> i64 {
    (i % 50 + 1) as i64
}

fn iid_of(i: u64) -> i64 {
    (i / 50 + 1) as i64
}

pub fn issue(i: u64) -> Issue {
    Issue {
        id: i as i64 + 1,
        iid: iid_of(i),
        project_id: project_of(i),
        title: title(i, "Issue"),
        web_url: format!(
            "https://gl/{}/proj{}/-/issues/{}",
            namespace(i),
            project_of(i),
            iid_of(i)
        ),
        state: if i % 5 == 0 { "closed" } else { "opened" }.to_string(),
        labels: label_set(i),
        updated_at: shuffled(i, 1_000_000) + 1,
        ..Default::default()
    }
}

pub fn merge_request(i: u64) -> MergeRequest {
    let assignees = if i % 3 == 0 {
        vec![UserRef {
            id: 999,
            username: "other".to_string(),
        }]
    } else {
        Vec::new()
    };
    MergeRequest {
        id: i as i64 + 1,
        iid: iid_of(i),
        project_id: project_of(i),
        title: title(i, "MR"),
        web_url: format!(
            "https://gl/{}/proj{}/-/merge_requests/{}",
            namespace(i),
            project_of(i),
            iid_of(i)
        ),
        state: if i % 5 == 0 { "merged" } else { "opened" }.to_string(),
        labels: label_set(i),
        assignees,
        updated_at: shuffled(i, 1_000_000) + 1,
    }
}

pub fn project(i: u64) -> Project {
    Project {
        id: i as i64 + 1,
        name: format!("proj{i}"),
        path_with_namespace: format!("{}/proj{i}", namespace(i)),
        web_url: format!("https://gl/{}/proj{i}", namespace(i)),
        avatar_url: format!("https://gl/uploads/-/system/project/avatar/{i}/logo.png"),
        archived: false,
        issues_access_level: "enabled".into(),
        merge_requests_access_level: "enabled".into(),
        repository_access_level: "enabled".into(),
        issues_enabled: Some(true),
        merge_requests_enabled: Some(true),
    }
}

pub fn group(i: u64) -> Group {
    Group {
        id: i as i64 + 1,
        name: format!("group{i}"),
        full_path: format!("{}/group{i}", namespace(i)),
        web_url: format!("https://gl/{}/group{i}", namespace(i)),
    }
}

pub fn epic(i: u64) -> Epic {
    let group_id = (i % 10) as i64 + 1;
    Epic {
        id: i as i64 + 1,
        iid: (i / 10) as i64 + 1,
        group_id,
        work_item_id: i as i64 + 1_000_001,
        title: title(i, "Epic"),
        web_url: format!(
            "https://gl/groups/{}/group{group_id}/-/epics/{}",
            namespace(i),
            i / 10 + 1
        ),
        state: "opened".into(),
        labels: label_set(i),
        updated_at: shuffled(i, 1_000_000) + 1,
    }
}

/// A timelog spent uniformly over the 30 days before `now`, shuffled so
/// generation order is not time order.
pub fn timelog(i: u64, now: u64) -> Timelog {
    Timelog {
        id: i + 1,
        spent_at: now - shuffled(i, 30 * 86_400),
        kind: if i % 4 == 0 {
            Issuable::MergeRequest
        } else {
            Issuable::Issue
        },
        project_id: project_of(i),
        iid: iid_of(i),
        title: title(i, "Issue"),
        web_url: format!(
            "https://gl/team/proj{}/-/issues/{}",
            project_of(i),
            iid_of(i)
        ),
        time_spent: 5400,
        summary: "worked on it".to_string(),
    }
}

pub fn put<R: Stored>(env: &BenchEnv, rows: &[R]) {
    let mut c = env.store().begin();
    c.upsert(rows).unwrap();
    c.commit().unwrap();
}

/// Mark `jobs` synced, unlocking the readers' cold-cache guards.
pub fn mark_synced(env: &BenchEnv, jobs: &[Job]) {
    let mut c = env.store().begin();
    for job in jobs {
        c.set_job(
            &job.key(),
            &JobState {
                last_ok: 1_700_000_000,
                last_full: 1_700_000_000,
                ..Default::default()
            },
        )
        .unwrap();
    }
    c.commit().unwrap();
}

/// Seed the full search corpus: `n` issues, `n/2` MRs, `n/50` projects,
/// `n/100` groups, `n/20` epics, plus synced boards for every project the issues use, so
/// the per-hit `board_column` lookup finds something.
pub fn seed_search_corpus(env: &BenchEnv, n: u64) {
    put(env, &(0..n).map(issue).collect::<Vec<_>>());
    put(env, &(0..n / 2).map(merge_request).collect::<Vec<_>>());
    put(env, &(0..(n / 50).max(1)).map(project).collect::<Vec<_>>());
    put(env, &(0..(n / 100).max(1)).map(group).collect::<Vec<_>>());
    put(env, &(0..(n / 20).max(1)).map(epic).collect::<Vec<_>>());
    let lists = ["Doing", "Review", "Done"]
        .map(|name| BoardList {
            label: Some(LabelRef { name: name.into() }),
        })
        .to_vec();
    let boards: Vec<Board> = (1..=50)
        .map(|project_id| Board {
            id: 1,
            project_id,
            lists: lists.clone(),
        })
        .collect();
    put(env, &boards);
    let mut synced: Vec<Job> = (1..=50).map(Job::ProjectBoards).collect();
    synced.push(Job::MemberProjects);
    mark_synced(env, &synced);
}

/// `n` MRs, `assigned` of them listed in the assigned view.
pub fn seed_mr_corpus(env: &BenchEnv, n: u64, assigned: u64) {
    let mrs: Vec<_> = (0..n).map(merge_request).collect();
    put(env, &mrs);
    let mut c = env.store().begin();
    c.set_view(
        ASSIGNED_MERGE_REQUESTS,
        &View {
            keys: mrs
                .iter()
                .take(assigned as usize)
                .map(Resource::key)
                .collect(),
            fetched_at: 1_700_000_000,
        },
    )
    .unwrap();
    c.commit().unwrap();
    mark_synced(env, &[Job::AssignedMergeRequests]);
}

pub fn seed_history(env: &BenchEnv, n: u64, now: u64) {
    put(env, &(0..n).map(|i| timelog(i, now)).collect::<Vec<_>>());
}
