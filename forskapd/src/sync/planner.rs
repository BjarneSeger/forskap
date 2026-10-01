//! Which jobs the worker keeps scheduled, derived from the config and what
//! the store already holds.
//!
//! The per-project corpus follows the *tracked* projects: those where the
//! user has recent activity (an assignment, a contribution event, a
//! timelog). Evidence is read from the store each time, so nothing extra is
//! persisted: the event and timelog windows are the memory. Only tracked
//! projects the user is a member of get a corpus: an assigned MR in an
//! upstream like gitlab-org/gitlab must not pull in its whole history.
//! The recent issue views are no evidence: an issue closed two months ago
//! doesn't say the user still works in its project.
//!
//! Epics follow the corpus one level up: the member groups a corpus project
//! lies in get their epics synced.
//!
//! Avatars follow the member projects, tracked or not: a project shows its
//! icon in search either way.
//!
//! A project's own settings leave jobs out: GitLab refuses to list the
//! issues or boards of a project whose issues are switched off, and the
//! merge requests of one whose merge requests (or repository) are. Only
//! what its member row says counts; a project without one, or one that
//! doesn't say, is planned as usual.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use super::avatars::Avatar;
use super::jobs::{ASSIGNED_ISSUES, ASSIGNED_MERGE_REQUESTS, ISSUE_VIEWS, Job};
use super::model::{Board, Epic, Event, Issue, MergeRequest, Project, RowKey};
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
    /// Groups whose epics are synced.
    pub epic_groups: usize,
    /// The member projects with an avatar, by the hash of its URL.
    pub avatars: BTreeMap<i64, u64>,
    /// The jobs left out because their project has the feature they read
    /// switched off.
    pub switched_off: BTreeSet<Job>,
}

/// How many tracked projects each source contributed first, for the log.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Evidence {
    pub assigned: usize,
    pub events: usize,
    pub timelogs: usize,
}

/// The jobs planned whatever the store holds.
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
    let no_issues: HashSet<i64> = projects
        .iter()
        .filter(|p| p.issues_disabled())
        .map(|p| p.id)
        .collect();
    let no_merge_requests: HashSet<i64> = projects
        .iter()
        .filter(|p| p.merge_requests_disabled())
        .map(|p| p.id)
        .collect();
    let switched_off = |job: &Job| match *job {
        Job::ProjectIssues(p) | Job::ProjectBoards(p) => no_issues.contains(&p),
        Job::ProjectMergeRequests(p) => no_merge_requests.contains(&p),
        _ => false,
    };
    let mut jobs = BTreeSet::from(BASE);
    let mut left_out = BTreeSet::new();
    let mut plan_unless_off = |jobs: &mut BTreeSet<Job>, job: Job| {
        if switched_off(&job) {
            left_out.insert(job);
        } else {
            jobs.insert(job);
        }
    };
    jobs.extend(avatars.keys().map(|&p| Job::ProjectAvatar(p)));
    // Board columns are read for the assigned issues and the corpus. A
    // tracked project the user isn't a member of may be gone or closed.
    for &p in assigned_issues.iter().chain(tracked.intersection(&members)) {
        plan_unless_off(&mut jobs, Job::ProjectBoards(p));
    }
    let unlisted = events
        .iter()
        .filter(|e| e.implies_membership() && !members.contains(&e.project_id))
        .map(|e| e.project_id)
        .collect();
    let corpus: BTreeSet<i64> = match population {
        SearchPopulation::All => {
            jobs.extend([Job::AllIssues, Job::AllMergeRequests]);
            BTreeSet::new()
        }
        SearchPopulation::Member => members,
        SearchPopulation::Tracked => tracked.intersection(&members).copied().collect(),
    };
    for &p in &corpus {
        plan_unless_off(&mut jobs, Job::ProjectIssues(p));
        plan_unless_off(&mut jobs, Job::ProjectMergeRequests(p));
    }
    // `all` has no per-project corpus: every member group counts.
    let everywhere = population == SearchPopulation::All;
    let above = ancestors(projects.iter().filter(|p| corpus.contains(&p.id)));
    let epic_groups: Vec<i64> = store
        .groups
        .scan(RowScope::All)?
        .into_iter()
        .filter(|g| everywhere || above.contains(g.full_path.as_str()))
        .map(|g| g.id)
        .collect();
    jobs.extend(epic_groups.iter().map(|&g| Job::GroupEpics(g)));
    Ok(Plan {
        jobs,
        tracked,
        evidence,
        unlisted,
        corpus: corpus.len(),
        epic_groups: epic_groups.len(),
        avatars,
        switched_off: left_out,
    })
}

/// The full paths of every group `projects` lie in, at any depth:
/// `team/backend/api` is in `team/backend` and in `team`.
fn ancestors<'a>(projects: impl Iterator<Item = &'a Project>) -> HashSet<&'a str> {
    let mut groups = HashSet::new();
    for project in projects {
        let mut path = project.path_with_namespace.as_str();
        while let Some((parent, _)) = path.rsplit_once('/') {
            if !groups.insert(parent) {
                break;
            }
            path = parent;
        }
    }
    groups
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

/// The rows any of the views `names` lists.
fn viewed(store: &SyncStore, names: &[&str]) -> Result<HashSet<RowKey>> {
    let mut keys = HashSet::new();
    for name in names {
        keys.extend(store.view(name)?.unwrap_or_default().keys);
    }
    Ok(keys)
}

/// Stage the removal of rows no job in `plan` keeps fresh any more: issues
/// and MRs of unplanned projects (unless a view lists them),
/// epics of unplanned groups, boards of untracked projects and avatars of projects that lost theirs or
/// left the memberships (their files go with the worker's sweep). Returns
/// how many.
pub fn collect_garbage(commit: &mut Commit<'_>, store: &SyncStore, plan: &Plan) -> Result<usize> {
    let mut removed = 0;
    if !plan.jobs.contains(&Job::AllIssues) {
        let kept = viewed(store, &ISSUE_VIEWS)?;
        removed += commit.remove_where::<Issue>(RowScope::All, |k| {
            kept.contains(&k) || in_corpus(plan, true, k)
        })?;
    }
    if !plan.jobs.contains(&Job::AllMergeRequests) {
        let kept = viewed(store, &[ASSIGNED_MERGE_REQUESTS])?;
        removed += commit.remove_where::<MergeRequest>(RowScope::All, |k| {
            kept.contains(&k) || in_corpus(plan, false, k)
        })?;
    }
    removed += commit.remove_where::<Epic>(RowScope::All, |k| {
        plan.jobs.contains(&Job::GroupEpics(k.0 as i64))
    })?;
    removed += commit.remove_where::<Board>(RowScope::All, |k| {
        plan.jobs.contains(&Job::ProjectBoards(k.0 as i64))
    })?;
    removed += commit.remove_where::<Avatar>(RowScope::All, |k| {
        plan.jobs.contains(&Job::ProjectAvatar(k.0 as i64))
    })?;
    Ok(removed)
}

/// Stage the removal of rows that left the view `name` since it listed
/// `before`, that no other view of their kind lists and that no corpus job
/// in `plan` keeps fresh: nothing else would update them. Returns how many.
pub fn drop_unviewed(
    commit: &mut Commit<'_>,
    store: &SyncStore,
    plan: &Plan,
    name: &str,
    before: &[RowKey],
) -> Result<usize> {
    let issue = ISSUE_VIEWS.contains(&name);
    // Issues and MRs share their keys: a view of neither kind has no rows
    // to drop, rather than the other kind's.
    if !issue && name != ASSIGNED_MERGE_REQUESTS {
        return Ok(0);
    }
    let now = if issue {
        viewed(store, &ISSUE_VIEWS)?
    } else {
        viewed(store, &[ASSIGNED_MERGE_REQUESTS])?
    };
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
    use crate::sync::jobs::{RECENT_ASSIGNED_ISSUES, RECENT_AUTHORED_ISSUES};
    use crate::sync::model::{Event, Group, Project, Timelog};
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
            Event {
                target_type: "Project".into(),
                ..event(5, 7, "created", 500)
            },
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

    fn listing(keys: &[RowKey]) -> View {
        View {
            keys: keys.to_vec(),
            fetched_at: 0,
        }
    }

    /// An issue that closed leaves the assigned view while a recent one
    /// still names it, and the other way round once it ages out there.
    #[test]
    fn a_row_leaving_a_view_stays_while_another_lists_it() {
        let (s, _d) = store();
        let mut c = s.begin();
        c.upsert(&[issue(1, 1), issue(1, 2), issue(1, 3)]).unwrap();
        c.set_view(ASSIGNED_ISSUES, &listing(&[(1, 3)])).unwrap();
        c.set_view(RECENT_ASSIGNED_ISSUES, &listing(&[(1, 1)]))
            .unwrap();
        c.set_view(RECENT_AUTHORED_ISSUES, &listing(&[])).unwrap();
        c.commit().unwrap();
        let p = plan(&s, SearchPopulation::Tracked, 100).unwrap();

        // #1 and #2 left the assigned view; a recent one lists #1.
        let mut c = s.begin();
        let before = [(1, 1), (1, 2), (1, 3)];
        let removed = drop_unviewed(&mut c, &s, &p, ASSIGNED_ISSUES, &before).unwrap();
        c.commit().unwrap();
        assert_eq!(removed, 1);
        assert_eq!(s.issues.keys(RowScope::All).unwrap(), [(1, 1), (1, 3)]);

        // #3 left a recent view; the assigned one lists it.
        let mut c = s.begin();
        let removed = drop_unviewed(&mut c, &s, &p, RECENT_AUTHORED_ISSUES, &[(1, 3)]).unwrap();
        c.commit().unwrap();
        assert_eq!(removed, 0);
        assert_eq!(s.issues.keys(RowScope::All).unwrap(), [(1, 1), (1, 3)]);
    }

    /// Issues and merge requests number alike: a row leaving an issue view
    /// is an issue, whatever the view's name.
    #[test]
    fn a_recent_view_never_touches_the_merge_requests() {
        let (s, _d) = store();
        let mr = MergeRequest {
            id: 101,
            iid: 1,
            project_id: 1,
            ..Default::default()
        };
        let mut c = s.begin();
        c.upsert(&[mr]).unwrap();
        c.commit().unwrap();
        let p = plan(&s, SearchPopulation::Tracked, 100).unwrap();

        for name in [RECENT_AUTHORED_ISSUES, RECENT_ASSIGNED_ISSUES] {
            let mut c = s.begin();
            c.upsert(&[issue(1, 1)]).unwrap();
            c.commit().unwrap();
            let mut c = s.begin();
            let removed = drop_unviewed(&mut c, &s, &p, name, &[(1, 1)]).unwrap();
            c.commit().unwrap();
            assert_eq!(removed, 1, "{name}");
            assert!(s.issues.keys(RowScope::All).unwrap().is_empty(), "{name}");
            assert_eq!(s.merge_requests.keys(RowScope::All).unwrap(), [(1, 1)]);
        }
    }

    #[test]
    fn recent_views_keep_their_rows_from_the_garbage() {
        let (s, _d) = store();
        let mut c = s.begin();
        c.upsert(&[issue(1, 1), issue(2, 1), issue(3, 1)]).unwrap();
        c.set_view(RECENT_AUTHORED_ISSUES, &listing(&[(1, 1)]))
            .unwrap();
        c.set_view(RECENT_ASSIGNED_ISSUES, &listing(&[(2, 1)]))
            .unwrap();
        c.commit().unwrap();

        let p = plan(&s, SearchPopulation::Tracked, 100).unwrap();
        let mut c = s.begin();
        let removed = collect_garbage(&mut c, &s, &p).unwrap();
        c.commit().unwrap();
        assert_eq!(removed, 1, "only what no view lists");
        assert_eq!(s.issues.keys(RowScope::All).unwrap(), [(1, 1), (2, 1)]);
    }

    /// An issue closed two months ago is no sign of activity: its project
    /// gets neither a corpus nor boards from it.
    #[test]
    fn recent_views_track_no_project() {
        let (s, _d) = store();
        let mut c = s.begin();
        c.upsert(&[member(5), member(6)]).unwrap();
        c.set_view(RECENT_AUTHORED_ISSUES, &listing(&[(5, 1)]))
            .unwrap();
        c.set_view(RECENT_ASSIGNED_ISSUES, &listing(&[(6, 1)]))
            .unwrap();
        c.commit().unwrap();

        let plan = plan(&s, SearchPopulation::Tracked, 100).unwrap();
        assert!(plan.tracked.is_empty());
        assert_eq!(plan.evidence, Evidence::default());
        assert_eq!(
            plan.jobs,
            Plan::base().jobs,
            "both lists are always planned"
        );
        assert!(plan.jobs.contains(&Job::RecentAuthoredIssues));
        assert!(plan.jobs.contains(&Job::RecentAssignedIssues));
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

    fn project_at(id: i64, path: &str) -> Project {
        Project {
            id,
            path_with_namespace: path.into(),
            ..Default::default()
        }
    }

    fn group(id: i64, full_path: &str) -> Group {
        Group {
            id,
            full_path: full_path.into(),
            ..Default::default()
        }
    }

    fn epic_groups_of(plan: &Plan) -> BTreeSet<i64> {
        plan.jobs
            .iter()
            .filter_map(|j| match j {
                Job::GroupEpics(g) => Some(*g),
                _ => None,
            })
            .collect()
    }

    /// Epics are synced for the member groups above a corpus project, at
    /// any depth; a group that merely shares a path prefix isn't above it.
    #[test]
    fn epics_follow_the_groups_above_the_corpus() {
        let (s, _d) = store();
        let mut c = s.begin();
        c.upsert(&[
            project_at(7, "team/backend/api"),
            project_at(8, "other/web"),
        ])
        .unwrap();
        c.upsert(&[
            group(1, "team"),
            group(2, "team/backend"),
            group(3, "team/back"),
            group(4, "other"),
        ])
        .unwrap();
        c.upsert(&[event(1, 7, "opened", 500)]).unwrap();
        c.commit().unwrap();

        let tracked = plan(&s, SearchPopulation::Tracked, 100).unwrap();
        assert_eq!(epic_groups_of(&tracked), BTreeSet::from([1, 2]));
        assert_eq!(tracked.epic_groups, 2);

        let member = plan(&s, SearchPopulation::Member, 100).unwrap();
        assert_eq!(epic_groups_of(&member), BTreeSet::from([1, 2, 4]));

        let all = plan(&s, SearchPopulation::All, 100).unwrap();
        assert_eq!(epic_groups_of(&all), BTreeSet::from([1, 2, 3, 4]));
    }

    #[test]
    fn epics_of_unplanned_groups_are_garbage() {
        let (s, _d) = store();
        let epic = |group_id, iid| Epic {
            id: group_id * 100 + iid,
            iid,
            group_id,
            ..Default::default()
        };
        let mut c = s.begin();
        c.upsert(&[project_at(7, "team/api")]).unwrap();
        c.upsert(&[group(1, "team"), group(4, "other")]).unwrap();
        c.upsert(&[event(1, 7, "opened", 500)]).unwrap();
        c.upsert(&[epic(1, 1), epic(4, 1), epic(9, 1)]).unwrap();
        c.commit().unwrap();

        let p = plan(&s, SearchPopulation::Tracked, 100).unwrap();
        let mut c = s.begin();
        let removed = collect_garbage(&mut c, &s, &p).unwrap();
        c.commit().unwrap();
        assert_eq!(removed, 2, "4 has no corpus project, 9 is no member group");
        assert_eq!(s.epics.keys(RowScope::All).unwrap(), [(1, 1)]);
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

    /// A member project with `feature` (`"issues"`, `"merge_requests"`,
    /// `"repository"`) switched off.
    fn without(id: i64, feature: &str) -> Project {
        let mut p = member(id);
        let off = "disabled".to_string();
        match feature {
            "issues" => p.issues_access_level = off,
            "merge_requests" => p.merge_requests_access_level = off,
            "repository" => p.repository_access_level = off,
            other => panic!("no feature {other}"),
        }
        p
    }

    /// The per-project jobs `plan` holds for project `p`, by kind.
    fn per_project(plan: &Plan, p: i64) -> [bool; 3] {
        [
            plan.jobs.contains(&Job::ProjectIssues(p)),
            plan.jobs.contains(&Job::ProjectMergeRequests(p)),
            plan.jobs.contains(&Job::ProjectBoards(p)),
        ]
    }

    #[test]
    fn a_feature_switched_off_leaves_its_jobs_out() {
        let (s, _d) = store();
        let mut c = s.begin();
        c.upsert(&[
            without(1, "merge_requests"),
            without(2, "issues"),
            member(3),
            without(4, "repository"),
            // Members only: the account may be one, so it is planned.
            Project {
                issues_access_level: "private".into(),
                merge_requests_access_level: "private".into(),
                ..member(5)
            },
            // An instance from before the access levels.
            Project {
                issues_enabled: Some(false),
                merge_requests_enabled: Some(true),
                ..member(6)
            },
        ])
        .unwrap();
        let events: Vec<Event> = (1..=6).map(|p| event(p, p, "opened", 500)).collect();
        c.upsert(&events).unwrap();
        c.commit().unwrap();

        for population in [SearchPopulation::Tracked, SearchPopulation::Member] {
            let plan = plan(&s, population, 100).unwrap();
            // [issues, merge requests, boards]
            assert_eq!(per_project(&plan, 1), [true, false, true], "{population:?}");
            assert_eq!(
                per_project(&plan, 2),
                [false, true, false],
                "{population:?}"
            );
            assert_eq!(per_project(&plan, 3), [true, true, true], "{population:?}");
            assert_eq!(per_project(&plan, 4), [true, false, true], "{population:?}");
            assert_eq!(per_project(&plan, 5), [true, true, true], "{population:?}");
            assert_eq!(
                per_project(&plan, 6),
                [false, true, false],
                "{population:?}"
            );
            assert_eq!(
                plan.switched_off,
                BTreeSet::from([
                    Job::ProjectMergeRequests(1),
                    Job::ProjectIssues(2),
                    Job::ProjectBoards(2),
                    Job::ProjectMergeRequests(4),
                    Job::ProjectIssues(6),
                    Job::ProjectBoards(6),
                ]),
                "{population:?}"
            );
            // Still a corpus project: its epics and its avatar don't care.
            assert_eq!(plan.corpus, 6, "{population:?}");
        }
    }

    /// Boards are read for the assigned issues' projects too: one that
    /// switched its issues off has none, one without a member row is
    /// planned as before.
    #[test]
    fn boards_of_assigned_issues_follow_the_projects_issues() {
        let (s, _d) = store();
        let mut c = s.begin();
        c.upsert(&[without(2, "issues"), member(3)]).unwrap();
        c.set_view(ASSIGNED_ISSUES, &listing(&[(2, 1), (3, 1), (9, 1)]))
            .unwrap();
        c.commit().unwrap();

        let plan = plan(&s, SearchPopulation::Tracked, 100).unwrap();
        assert!(!plan.jobs.contains(&Job::ProjectBoards(2)));
        assert!(plan.jobs.contains(&Job::ProjectBoards(3)));
        assert!(
            plan.jobs.contains(&Job::ProjectBoards(9)),
            "no row: unknown"
        );
        // The assignment tracks it, so it is in the corpus too.
        assert_eq!(
            plan.switched_off,
            BTreeSet::from([Job::ProjectBoards(2), Job::ProjectIssues(2)])
        );
    }

    /// The member listing brings the setting: a project switching its
    /// issues off loses its issue and board jobs and their rows (but the
    /// items a view lists), and switching them back on brings the jobs back.
    #[test]
    fn switching_a_feature_off_and_on_again_drops_and_returns_its_jobs() {
        let (s, _d) = store();
        let mut c = s.begin();
        c.upsert(&[member(2)]).unwrap();
        c.upsert(&[event(1, 2, "opened", 500)]).unwrap();
        c.upsert(&[issue(2, 1), issue(2, 2)]).unwrap();
        c.upsert(&[Board {
            id: 1,
            project_id: 2,
            ..Default::default()
        }])
        .unwrap();
        c.set_view(RECENT_AUTHORED_ISSUES, &listing(&[(2, 2)]))
            .unwrap();
        c.commit().unwrap();
        let on = plan(&s, SearchPopulation::Tracked, 100).unwrap();
        assert_eq!(per_project(&on, 2), [true, true, true]);

        let mut c = s.begin();
        c.upsert(&[without(2, "issues")]).unwrap();
        c.commit().unwrap();
        let off = plan(&s, SearchPopulation::Tracked, 100).unwrap();
        assert_eq!(per_project(&off, 2), [false, true, false]);
        let mut c = s.begin();
        let removed = collect_garbage(&mut c, &s, &off).unwrap();
        c.commit().unwrap();
        assert_eq!(removed, 2, "issue #1 and the board");
        assert_eq!(
            s.issues.keys(RowScope::All).unwrap(),
            [(2, 2)],
            "a view still lists #2"
        );
        assert!(s.boards.keys(RowScope::All).unwrap().is_empty());

        let mut c = s.begin();
        c.upsert(&[Project {
            issues_access_level: "enabled".into(),
            ..member(2)
        }])
        .unwrap();
        c.commit().unwrap();
        let again = plan(&s, SearchPopulation::Tracked, 100).unwrap();
        assert_eq!(again.jobs, on.jobs);
        assert!(again.switched_off.is_empty());
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
