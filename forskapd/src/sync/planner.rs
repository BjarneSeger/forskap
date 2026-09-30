//! Which jobs the worker keeps scheduled, derived from the config and what
//! the store already holds.
//!
//! The per-project corpus follows the *tracked* projects: those where the
//! user has recent activity (an assignment, a contribution event, a
//! timelog). Evidence is read from the store each time, so nothing extra is
//! persisted: the event and timelog windows are the memory. Only tracked
//! projects the user is a member of get a corpus: an assigned MR in an
//! upstream like gitlab-org/gitlab must not pull in its whole history.
//!
//! Avatars follow the member projects, tracked or not: a project shows its
//! icon in search either way.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use super::avatars::Avatar;
use super::jobs::{ASSIGNED_ISSUES, ASSIGNED_MERGE_REQUESTS, Job};
use super::model::{Board, Event, Issue, MergeRequest, RowKey};
use super::schedule::{fingerprint, text_hash};
use super::store::{Commit, RowScope, SyncStore};
use crate::config::{Config, SearchPopulation};
use crate::error::Result;

#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    pub jobs: BTreeSet<Job>,
    pub tracked: BTreeSet<i64>,
    pub evidence: Evidence,
    /// Projects whose events show a membership the member listing lacks
    /// (joined or created since it ran).
    pub unlisted: BTreeSet<i64>,
    /// Projects whose issues and MRs are synced.
    pub corpus: usize,
    /// The member projects with an avatar, by the hash of its URL.
    pub avatars: BTreeMap<i64, u64>,
}

/// How many tracked projects each source contributed first, for the log.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Evidence {
    pub assigned: usize,
    pub events: usize,
    pub timelogs: usize,
}

/// The jobs planned whatever the store holds.
const BASE: [Job; 7] = [
    Job::AssignedIssues,
    Job::AssignedMergeRequests,
    Job::RecentTimelogs,
    Job::AllTimelogs,
    Job::Events,
    Job::MemberProjects,
    Job::MemberGroups,
];

impl Plan {
    /// Just the jobs every plan has; what runs while planning fails.
    pub fn base() -> Self {
        Self {
            jobs: BTreeSet::from(BASE),
            ..Default::default()
        }
    }

    /// [`Job::fingerprint`] plus what the plan knows: an avatar is valid for
    /// the URL it was fetched under, so a new one is fetched at once.
    pub fn fingerprint(&self, job: Job, c: &Config) -> u64 {
        let base = job.fingerprint(c);
        match job {
            Job::ProjectAvatar(p) => {
                fingerprint(&[base, self.avatars.get(&p).copied().unwrap_or(0)])
            }
            _ => base,
        }
    }
}

/// Plan the jobs for `population`, counting activity since `tracked_since`.
pub fn plan(store: &SyncStore, population: SearchPopulation, tracked_since: u64) -> Result<Plan> {
    let assigned_issues = view_projects(store, ASSIGNED_ISSUES)?;
    let assigned_mrs = view_projects(store, ASSIGNED_MERGE_REQUESTS)?;
    let events = store.events.scan(RowScope::Since(tracked_since))?;
    let (tracked, evidence) = tracked_projects(
        store,
        [&assigned_issues, &assigned_mrs],
        &events,
        tracked_since,
    )?;
    let projects = store.projects.scan(RowScope::All)?;
    let members: BTreeSet<i64> = projects.iter().map(|p| p.id).collect();
    let avatars: BTreeMap<i64, u64> = projects
        .iter()
        .filter(|p| !p.avatar_url.is_empty())
        .map(|p| (p.id, text_hash(&p.avatar_url)))
        .collect();
    let mut jobs = BTreeSet::from(BASE);
    jobs.extend(avatars.keys().map(|&p| Job::ProjectAvatar(p)));
    // Board columns are read for the assigned issues and the corpus. A
    // tracked project the user isn't a member of may be gone or closed.
    jobs.extend(
        assigned_issues
            .iter()
            .chain(tracked.intersection(&members))
            .map(|&p| Job::ProjectBoards(p)),
    );
    let unlisted = events
        .iter()
        .filter(|e| e.implies_membership() && !members.contains(&e.project_id))
        .map(|e| e.project_id)
        .collect();
    let corpus: Vec<i64> = match population {
        SearchPopulation::All => {
            jobs.extend([Job::AllIssues, Job::AllMergeRequests]);
            Vec::new()
        }
        SearchPopulation::Member => members.into_iter().collect(),
        SearchPopulation::Tracked => tracked.intersection(&members).copied().collect(),
    };
    for &p in &corpus {
        jobs.extend([Job::ProjectIssues(p), Job::ProjectMergeRequests(p)]);
    }
    Ok(Plan {
        jobs,
        tracked,
        evidence,
        unlisted,
        corpus: corpus.len(),
        avatars,
    })
}

/// The projects the view `name` lists.
fn view_projects(store: &SyncStore, name: &str) -> Result<BTreeSet<i64>> {
    Ok(store
        .view(name)?
        .unwrap_or_default()
        .keys
        .into_iter()
        .map(|(project, _)| project as i64)
        .collect())
}

/// Projects in the assigned views, plus those with activity `events` or
/// timelogs at or after `since`.
fn tracked_projects(
    store: &SyncStore,
    assigned: [&BTreeSet<i64>; 2],
    events: &[Event],
    since: u64,
) -> Result<(BTreeSet<i64>, Evidence)> {
    let mut tracked = BTreeSet::new();
    let mut evidence = Evidence::default();
    for &project in assigned.into_iter().flatten() {
        evidence.assigned += usize::from(tracked.insert(project));
    }
    for e in events {
        if e.is_activity() {
            evidence.events += usize::from(tracked.insert(e.project_id));
        }
    }
    for t in store.timelogs.scan(RowScope::Since(since))? {
        if t.project_id > 0 {
            evidence.timelogs += usize::from(tracked.insert(t.project_id));
        }
    }
    Ok((tracked, evidence))
}

/// Whether a corpus job in `plan` keeps the issue (or, for `!issue`, MR)
/// row `key` fresh.
fn in_corpus(plan: &Plan, issue: bool, key: RowKey) -> bool {
    let project = key.0 as i64;
    if issue {
        plan.jobs.contains(&Job::AllIssues) || plan.jobs.contains(&Job::ProjectIssues(project))
    } else {
        plan.jobs.contains(&Job::AllMergeRequests)
            || plan.jobs.contains(&Job::ProjectMergeRequests(project))
    }
}

fn viewed(store: &SyncStore, name: &str) -> Result<HashSet<RowKey>> {
    Ok(store
        .view(name)?
        .unwrap_or_default()
        .keys
        .into_iter()
        .collect())
}

/// Stage the removal of rows no job in `plan` keeps fresh any more: issues
/// and MRs of unplanned projects (unless an assigned view lists them),
/// boards of untracked projects and avatars of projects that lost theirs or
/// left the memberships (their files go with the worker's sweep). Returns
/// how many.
pub fn collect_garbage(commit: &mut Commit<'_>, store: &SyncStore, plan: &Plan) -> Result<usize> {
    let mut removed = 0;
    if !plan.jobs.contains(&Job::AllIssues) {
        let kept = viewed(store, ASSIGNED_ISSUES)?;
        removed += commit.remove_where::<Issue>(RowScope::All, |k| {
            kept.contains(&k) || in_corpus(plan, true, k)
        })?;
    }
    if !plan.jobs.contains(&Job::AllMergeRequests) {
        let kept = viewed(store, ASSIGNED_MERGE_REQUESTS)?;
        removed += commit.remove_where::<MergeRequest>(RowScope::All, |k| {
            kept.contains(&k) || in_corpus(plan, false, k)
        })?;
    }
    removed += commit.remove_where::<Board>(RowScope::All, |k| {
        plan.jobs.contains(&Job::ProjectBoards(k.0 as i64))
    })?;
    removed += commit.remove_where::<Avatar>(RowScope::All, |k| {
        plan.jobs.contains(&Job::ProjectAvatar(k.0 as i64))
    })?;
    Ok(removed)
}

/// Stage the removal of rows that left the assigned view `name` since it
/// listed `before` and that no corpus job in `plan` keeps fresh: nothing
/// else would update them. Returns how many.
pub fn drop_unviewed(
    commit: &mut Commit<'_>,
    store: &SyncStore,
    plan: &Plan,
    name: &str,
    before: &[RowKey],
) -> Result<usize> {
    let now = viewed(store, name)?;
    let issue = name == ASSIGNED_ISSUES;
    let mut removed = 0;
    for &key in before {
        if now.contains(&key) || in_corpus(plan, issue, key) {
            continue;
        }
        if issue {
            commit.remove::<Issue>(key);
        } else {
            commit.remove::<MergeRequest>(key);
        }
        removed += 1;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::model::{Event, Project, Timelog};
    use crate::sync::store::View;

    fn store() -> (SyncStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = fjall::Database::builder(dir.path().join("db"))
            .open()
            .unwrap();
        (SyncStore::open(&db).unwrap(), dir)
    }

    fn event(id: i64, project_id: i64, action: &str, created_at: u64) -> Event {
        Event {
            id,
            project_id,
            action_name: action.into(),
            created_at,
            ..Default::default()
        }
    }

    fn timelog(id: u64, project_id: i64, spent_at: u64) -> Timelog {
        Timelog {
            id,
            project_id,
            iid: 1,
            spent_at,
            ..Default::default()
        }
    }

    fn issue(project_id: i64, iid: i64) -> Issue {
        Issue {
            id: project_id * 100 + iid,
            iid,
            project_id,
            ..Default::default()
        }
    }

    fn projects_of(plan: &Plan) -> BTreeSet<i64> {
        plan.jobs
            .iter()
            .filter_map(|j| match j {
                Job::ProjectIssues(p) => Some(*p),
                _ => None,
            })
            .collect()
    }

    fn member(id: i64) -> Project {
        Project {
            id,
            ..Default::default()
        }
    }

    #[test]
    fn activity_alone_tracks_a_project() {
        let (s, _d) = store();
        let mut c = s.begin();
        c.upsert(&[member(10), member(11), member(12), member(13)])
            .unwrap();
        c.upsert(&[
            event(1, 10, "pushed to", 500),
            event(2, 11, "opened", 500),
            event(3, 12, "joined", 500),
            event(4, 13, "pushed new", 50),
            event(5, 0, "opened", 500),
        ])
        .unwrap();
        c.commit().unwrap();

        let plan = plan(&s, SearchPopulation::Tracked, 100).unwrap();
        assert_eq!(
            plan.tracked,
            BTreeSet::from([10, 11]),
            "membership, stale and project-less events don't count"
        );
        assert_eq!(projects_of(&plan), BTreeSet::from([10, 11]));
        assert!(plan.jobs.contains(&Job::ProjectBoards(10)));
        assert!(plan.jobs.contains(&Job::ProjectMergeRequests(11)));
    }

    /// An assigned MR in an upstream you aren't a member of keeps its row
    /// (via the view), but gets no corpus and no boards (an MR shows none).
    #[test]
    fn only_member_projects_get_a_corpus() {
        let (s, _d) = store();
        let mut c = s.begin();
        c.upsert(&[member(3)]).unwrap();
        c.upsert(&[event(1, 3, "opened", 500)]).unwrap();
        c.set_view(
            ASSIGNED_MERGE_REQUESTS,
            &View {
                keys: vec![(278964, 1)],
                fetched_at: 0,
            },
        )
        .unwrap();
        c.commit().unwrap();

        let plan = plan(&s, SearchPopulation::Tracked, 100).unwrap();
        assert_eq!(plan.tracked, BTreeSet::from([3, 278964]));
        assert_eq!(projects_of(&plan), BTreeSet::from([3]));
        assert_eq!(plan.corpus, 1);
        assert!(!plan.jobs.contains(&Job::ProjectBoards(278964)));
    }

    /// Boards are fetched where they are read: the assigned issues' projects
    /// and tracked member projects. Activity alone in a project that may
    /// be gone or closed fetches nothing.
    #[test]
    fn boards_follow_the_assigned_issues_and_tracked_members() {
        let (s, _d) = store();
        let mut c = s.begin();
        c.upsert(&[member(3)]).unwrap();
        c.upsert(&[event(1, 3, "opened", 500), event(2, 4, "opened", 500)])
            .unwrap();
        c.set_view(
            ASSIGNED_ISSUES,
            &View {
                keys: vec![(5, 1)],
                fetched_at: 0,
            },
        )
        .unwrap();
        c.commit().unwrap();

        let plan = plan(&s, SearchPopulation::Tracked, 100).unwrap();
        let boards: BTreeSet<i64> = plan
            .jobs
            .iter()
            .filter_map(|j| match j {
                Job::ProjectBoards(p) => Some(*p),
                _ => None,
            })
            .collect();
        assert_eq!(boards, BTreeSet::from([3, 5]));
    }

    #[test]
    fn membership_events_outside_the_listing_are_unlisted() {
        let (s, _d) = store();
        let mut c = s.begin();
        c.upsert(&[member(3)]).unwrap();
        c.upsert(&[
            event(1, 3, "pushed to", 500),
            event(2, 4, "joined", 500),
            event(3, 5, "pushed new", 500),
            event(4, 6, "commented on", 500),
            event(5, 7, "created", 500),
            event(6, 8, "created", 50),
        ])
        .unwrap();
        c.commit().unwrap();

        let plan = plan(&s, SearchPopulation::Tracked, 100).unwrap();
        assert_eq!(plan.unlisted, BTreeSet::from([4, 5, 7]));
    }

    #[test]
    fn rows_leaving_a_view_go_unless_a_corpus_covers_them() {
        let (s, _d) = store();
        let mut c = s.begin();
        c.upsert(&[issue(1, 1), issue(2, 1), issue(2, 2)]).unwrap();
        c.upsert(&[member(2)]).unwrap();
        c.upsert(&[event(1, 2, "opened", 500)]).unwrap();
        c.set_view(
            ASSIGNED_ISSUES,
            &View {
                keys: vec![(2, 2)],
                fetched_at: 0,
            },
        )
        .unwrap();
        c.commit().unwrap();

        // #1 of project 1 (no corpus) and #1 of project 2 left the view.
        let p = plan(&s, SearchPopulation::Tracked, 100).unwrap();
        let mut c = s.begin();
        let before = [(1, 1), (2, 1), (2, 2)];
        let removed = drop_unviewed(&mut c, &s, &p, ASSIGNED_ISSUES, &before).unwrap();
        c.commit().unwrap();
        assert_eq!(removed, 1);
        assert_eq!(s.issues.keys(RowScope::All).unwrap(), [(2, 1), (2, 2)]);
    }

    #[test]
    fn every_evidence_source_counts() {
        let (s, _d) = store();
        let mut c = s.begin();
        c.set_view(
            ASSIGNED_ISSUES,
            &View {
                keys: vec![(1, 5)],
                fetched_at: 0,
            },
        )
        .unwrap();
        c.set_view(
            ASSIGNED_MERGE_REQUESTS,
            &View {
                keys: vec![(2, 5)],
                fetched_at: 0,
            },
        )
        .unwrap();
        c.upsert(&[event(1, 3, "commented on", 500)]).unwrap();
        c.upsert(&[timelog(1, 4, 500), timelog(2, 0, 500), timelog(3, 5, 10)])
            .unwrap();
        c.commit().unwrap();

        let plan = plan(&s, SearchPopulation::Tracked, 100).unwrap();
        assert_eq!(plan.tracked, BTreeSet::from([1, 2, 3, 4]));
        assert_eq!(
            plan.evidence,
            Evidence {
                assigned: 2,
                events: 1,
                timelogs: 1,
            }
        );
    }

    #[test]
    fn populations_pick_their_corpus_jobs() {
        let (s, _d) = store();
        let mut c = s.begin();
        c.upsert(&[event(1, 3, "opened", 500)]).unwrap();
        c.upsert(&[
            Project {
                id: 7,
                ..Default::default()
            },
            Project {
                id: 8,
                ..Default::default()
            },
        ])
        .unwrap();
        c.commit().unwrap();

        let member = plan(&s, SearchPopulation::Member, 100).unwrap();
        assert_eq!(projects_of(&member), BTreeSet::from([7, 8]));
        assert!(
            !member
                .jobs
                .iter()
                .any(|j| matches!(j, Job::ProjectBoards(_))),
            "boards follow tracked members, and 3 is no member"
        );

        let all = plan(&s, SearchPopulation::All, 100).unwrap();
        assert!(projects_of(&all).is_empty());
        assert!(all.jobs.contains(&Job::AllIssues) && all.jobs.contains(&Job::AllMergeRequests));
    }

    fn with_avatar(id: i64, url: &str) -> Project {
        Project {
            id,
            avatar_url: url.into(),
            ..Default::default()
        }
    }

    /// Every member project with an avatar gets its job, tracked or not;
    /// the job's fingerprint follows the avatar's URL.
    #[test]
    fn avatars_follow_the_member_projects() {
        let (s, _d) = store();
        let mut c = s.begin();
        c.upsert(&[with_avatar(7, "https://gl/a.png"), member(8)])
            .unwrap();
        c.commit().unwrap();
        let cfg = crate::config::defaults();

        let before = plan(&s, SearchPopulation::Tracked, 100).unwrap();
        assert!(before.jobs.contains(&Job::ProjectAvatar(7)));
        assert!(!before.jobs.contains(&Job::ProjectAvatar(8)), "no avatar");
        assert!(before.tracked.is_empty());

        let again = plan(&s, SearchPopulation::Tracked, 100).unwrap();
        assert_eq!(
            again.fingerprint(Job::ProjectAvatar(7), &cfg),
            before.fingerprint(Job::ProjectAvatar(7), &cfg)
        );

        let mut c = s.begin();
        c.upsert(&[with_avatar(7, "https://gl/b.png")]).unwrap();
        c.commit().unwrap();
        let after = plan(&s, SearchPopulation::Tracked, 100).unwrap();
        assert_eq!(after.jobs, before.jobs);
        assert_ne!(
            after.fingerprint(Job::ProjectAvatar(7), &cfg),
            before.fingerprint(Job::ProjectAvatar(7), &cfg)
        );
        assert_eq!(
            after.fingerprint(Job::MemberProjects, &cfg),
            Job::MemberProjects.fingerprint(&cfg)
        );
    }

    #[test]
    fn avatars_of_projects_without_one_are_garbage() {
        let (s, _d) = store();
        let avatar = |project_id| Avatar {
            project_id,
            file: format!("{project_id}-1.png"),
        };
        let mut c = s.begin();
        c.upsert(&[with_avatar(7, "https://gl/a.png"), member(8)])
            .unwrap();
        c.upsert(&[avatar(7), avatar(8), avatar(9)]).unwrap();
        c.commit().unwrap();

        let p = plan(&s, SearchPopulation::Tracked, 100).unwrap();
        let mut c = s.begin();
        let removed = collect_garbage(&mut c, &s, &p).unwrap();
        c.commit().unwrap();
        assert_eq!(removed, 2, "8 lost its avatar, 9 its membership");
        assert_eq!(s.avatars.keys(RowScope::All).unwrap(), [(7, 0)]);
    }

    #[test]
    fn garbage_is_what_no_planned_job_covers() {
        let (s, _d) = store();
        let mut c = s.begin();
        c.upsert(&[issue(1, 1), issue(2, 1), issue(3, 1), issue(3, 2)])
            .unwrap();
        c.upsert(&[
            Board {
                id: 1,
                project_id: 1,
                ..Default::default()
            },
            Board {
                id: 2,
                project_id: 3,
                ..Default::default()
            },
        ])
        .unwrap();
        c.set_view(
            ASSIGNED_ISSUES,
            &View {
                keys: vec![(3, 2)],
                fetched_at: 0,
            },
        )
        .unwrap();
        c.upsert(&[event(1, 1, "opened", 500)]).unwrap();
        c.upsert(&[member(1), member(3)]).unwrap();
        c.commit().unwrap();

        // Tracked: project 1 (event) and 3 (assigned view).
        let p = plan(&s, SearchPopulation::Tracked, 100).unwrap();
        let mut c = s.begin();
        let removed = collect_garbage(&mut c, &s, &p).unwrap();
        c.commit().unwrap();
        assert_eq!(removed, 1, "only project 2's issue");
        let keys = s.issues.keys(RowScope::All).unwrap();
        assert_eq!(keys, [(1, 1), (3, 1), (3, 2)]);

        // Switching to `all` keeps every issue; untracked boards still go.
        let mut all = plan(&s, SearchPopulation::All, 100).unwrap();
        all.jobs.remove(&Job::ProjectBoards(3));
        let mut c = s.begin();
        collect_garbage(&mut c, &s, &all).unwrap();
        c.commit().unwrap();
        assert_eq!(s.issues.keys(RowScope::All).unwrap().len(), 3);
        assert_eq!(s.boards.keys(RowScope::All).unwrap(), [(1, 1)]);
    }
}
