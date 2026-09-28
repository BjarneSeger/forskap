//! Which jobs the worker keeps scheduled, derived from the config and what
//! the store already holds.
//!
//! The per-project corpus follows the *tracked* projects: those where the
//! user has recent activity (an assignment, a contribution event, a
//! timelog). Evidence is read from the store each time, so nothing extra is
//! persisted: the event and timelog windows are the memory. Only tracked
//! projects the user is a member of get a corpus: an assigned MR in an
//! upstream like gitlab-org/gitlab must not pull in its whole history.

use std::collections::{BTreeSet, HashSet};

use super::jobs::{ASSIGNED_ISSUES, ASSIGNED_MERGE_REQUESTS, Job};
use super::model::{Board, Issue, MergeRequest, RowKey};
use super::store::{Commit, RowScope, SyncStore};
use crate::config::SearchPopulation;
use crate::error::Result;

#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    pub jobs: BTreeSet<Job>,
    pub tracked: BTreeSet<i64>,
    pub evidence: Evidence,
    /// Projects whose issues and MRs are synced.
    pub corpus: usize,
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
}

/// Plan the jobs for `population`, counting activity since `tracked_since`.
pub fn plan(store: &SyncStore, population: SearchPopulation, tracked_since: u64) -> Result<Plan> {
    let (tracked, evidence) = tracked_projects(store, tracked_since)?;
    let mut jobs = BTreeSet::from(BASE);
    jobs.extend(tracked.iter().map(|&p| Job::ProjectBoards(p)));
    let members: BTreeSet<i64> = store
        .projects
        .keys(RowScope::All)?
        .into_iter()
        .map(|(id, _)| id as i64)
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
        corpus: corpus.len(),
    })
}

/// Projects in the assigned views, plus those with activity events or
/// timelogs at or after `since`.
fn tracked_projects(store: &SyncStore, since: u64) -> Result<(BTreeSet<i64>, Evidence)> {
    let mut tracked = BTreeSet::new();
    let mut evidence = Evidence::default();
    for name in [ASSIGNED_ISSUES, ASSIGNED_MERGE_REQUESTS] {
        for (project, _) in store.view(name)?.unwrap_or_default().keys {
            evidence.assigned += usize::from(tracked.insert(project as i64));
        }
    }
    for e in store.events.scan(RowScope::Since(since))? {
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

/// Stage the removal of rows no job in `plan` keeps fresh any more: issues
/// and MRs of unplanned projects (unless an assigned view lists them) and
/// boards of untracked projects. Returns how many.
pub fn collect_garbage(commit: &mut Commit<'_>, store: &SyncStore, plan: &Plan) -> Result<usize> {
    let viewed = |name| -> Result<HashSet<RowKey>> {
        Ok(store
            .view(name)?
            .unwrap_or_default()
            .keys
            .into_iter()
            .collect())
    };
    let mut removed = 0;

    if !plan.jobs.contains(&Job::AllIssues) {
        let kept = viewed(ASSIGNED_ISSUES)?;
        removed += commit.remove_where::<Issue>(RowScope::All, |k| {
            kept.contains(&k) || plan.jobs.contains(&Job::ProjectIssues(k.0 as i64))
        })?;
    }
    if !plan.jobs.contains(&Job::AllMergeRequests) {
        let kept = viewed(ASSIGNED_MERGE_REQUESTS)?;
        removed += commit.remove_where::<MergeRequest>(RowScope::All, |k| {
            kept.contains(&k) || plan.jobs.contains(&Job::ProjectMergeRequests(k.0 as i64))
        })?;
    }
    removed += commit.remove_where::<Board>(RowScope::All, |k| {
        plan.jobs.contains(&Job::ProjectBoards(k.0 as i64))
    })?;
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
    /// (via the view) and gets boards, but no corpus.
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
        assert!(plan.jobs.contains(&Job::ProjectBoards(278964)));
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
            member.jobs.contains(&Job::ProjectBoards(3)),
            "boards follow tracking"
        );

        let all = plan(&s, SearchPopulation::All, 100).unwrap();
        assert!(projects_of(&all).is_empty());
        assert!(all.jobs.contains(&Job::AllIssues) && all.jobs.contains(&Job::AllMergeRequests));
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
