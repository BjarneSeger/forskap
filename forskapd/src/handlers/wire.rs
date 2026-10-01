//! Projections of stored GitLab rows onto the varlink wire types.

use forskap_api::{
    ActivityEvent, Group, HistoryEvent, HistorySource, IssuableKind, MergeRequest, Project,
    SyncJob, SyncJobStatus, WorkItem, WorkItemRef, WorkItemState,
};

use crate::gitlab::Issuable;
use crate::query::{graph_status_from, namespace_of};
use crate::sync::{JobInfo, JobStatus, model};

/// What a wire item shows of its project.
pub struct ProjectInfo {
    /// `path_with_namespace` of the stored project.
    pub path: Option<String>,
    /// The avatar file, `None` without one.
    pub avatar: Option<String>,
}

/// A tracked project the user is no member of has no row, but its items
/// carry the path in their link.
fn project_path(stored: Option<String>, web_url: &str) -> Option<String> {
    stored
        .and_then(some)
        .or_else(|| some(namespace_of(web_url)))
}

/// Unix seconds as stored, `None` for the 0 of an unreadable or missing time.
fn known(secs: u64) -> Option<i64> {
    (secs > 0).then_some(secs as i64)
}

/// The work item type of every epic.
pub const EPIC: &str = "epic";

/// `board_labels` are the issue's project board lists, `None` when never
/// synced (then `graph_status` stays empty). `epic_url` is the link of the
/// stored epic the issue names as its parent, `None` without a row.
pub fn issue(
    i: model::Issue,
    board_labels: Option<&[String]>,
    open_count: i64,
    project: ProjectInfo,
    epic_url: Option<String>,
) -> WorkItem {
    WorkItem {
        graph_status: graph_status_from(board_labels, &i.labels, &i.state),
        parent: i
            .epic
            .as_ref()
            .and_then(|e| parent(e, epic_url, &i.web_url)),
        time_spent: Some(i.time_spent() as i64),
        r#type: i.work_item_type().to_string(),
        id: i.id,
        iid: i.iid,
        project_id: Some(i.project_id),
        group_id: None,
        namespace_path: project_path(project.path, &i.web_url),
        title: i.title,
        state: i.state,
        open_count,
        project_avatar: project.avatar,
        web_url: i.web_url,
        updated_at: known(i.updated_at),
    }
}

/// The epic an issue names as its parent. GitLab links it relative to the
/// instance, so the link is the stored epic's, else made absolute by the
/// issue's own. `None` for a reference without its group and number, as
/// rows stored before them have.
fn parent(
    epic: &model::EpicRef,
    stored_url: Option<String>,
    issue_url: &str,
) -> Option<WorkItemRef> {
    if epic.group_id <= 0 || epic.iid <= 0 {
        return None;
    }
    Some(WorkItemRef {
        project_id: None,
        group_id: Some(epic.group_id),
        iid: epic.iid,
        r#type: Some(EPIC.into()),
        title: some(epic.title.clone()),
        web_url: stored_url
            .and_then(some)
            .or_else(|| absolute(&epic.url, issue_url)),
    })
}

/// `url` as an absolute link: as it is with a scheme, a path behind the
/// scheme and host of `base`; `None` for anything else.
fn absolute(url: &str, base: &str) -> Option<String> {
    if url.contains("://") {
        return Some(url.to_string());
    }
    let path = url.strip_prefix('/')?;
    let (scheme, rest) = base.split_once("://")?;
    let host = rest.split('/').next().filter(|h| !h.is_empty())?;
    Some(format!("{scheme}://{host}/{path}"))
}

pub fn merge_request(
    m: model::MergeRequest,
    open_count: i64,
    project: ProjectInfo,
) -> MergeRequest {
    MergeRequest {
        id: m.id,
        iid: m.iid,
        project_id: m.project_id,
        title: m.title,
        state: m.state,
        assignees: m.assignees.into_iter().map(|a| a.username).collect(),
        open_count,
        project_avatar: project.avatar,
        project_path: project_path(project.path, &m.web_url),
        web_url: m.web_url,
        updated_at: known(m.updated_at),
    }
}

pub fn project(p: model::Project, avatar: Option<String>) -> Project {
    Project {
        id: p.id,
        name: p.name,
        path: p.path_with_namespace,
        web_url: p.web_url,
        avatar,
        archived: p.archived,
    }
}

pub fn group(g: model::Group) -> Group {
    Group {
        id: g.id,
        name: g.name,
        path: g.full_path,
        web_url: g.web_url,
    }
}

/// Epic pages live under `/groups/`, which is no part of the group's path.
pub fn group_path(stored: Option<String>, web_url: &str) -> String {
    stored.and_then(some).unwrap_or_else(|| {
        let ns = namespace_of(web_url);
        match ns.strip_prefix("groups/") {
            Some(path) => path.to_string(),
            None => ns,
        }
    })
}

/// `group_path` is the stored group's `full_path`, `None` without a row. The
/// id is the epic's work item id, never its legacy one.
pub fn epic(e: model::Epic, open_count: i64, group_path: Option<String>) -> WorkItem {
    WorkItem {
        namespace_path: some(self::group_path(group_path, &e.web_url)),
        id: e.work_item_id,
        iid: e.iid,
        r#type: EPIC.into(),
        project_id: None,
        group_id: Some(e.group_id),
        title: e.title,
        web_url: e.web_url,
        state: e.state,
        parent: None,
        time_spent: None,
        graph_status: String::new(),
        open_count,
        project_avatar: None,
        updated_at: known(e.updated_at),
    }
}

/// A synced timelog as a history event.
pub fn timelog(t: model::Timelog) -> HistoryEvent {
    HistoryEvent {
        timestamp: t.spent_at as i64,
        source: HistorySource::gitlab,
        kind: kind(t.kind),
        project_id: t.project_id,
        iid: t.iid,
        title: some(t.title),
        web_url: some(t.web_url),
        time_spent: Some(t.time_spent as i64),
        duration: None,
        summary: some(t.summary),
    }
}

/// A sync job as the worker reported it; a job that never ran has no
/// `last_ok`. `unavailable` is on every job, and `fetched` on every running
/// one, so a client tells this daemon from one too old to say.
pub fn sync_job(j: JobInfo) -> SyncJob {
    SyncJob {
        key: j.key,
        status: match j.status {
            JobStatus::Running => SyncJobStatus::running,
            JobStatus::Demanded => SyncJobStatus::demanded,
            JobStatus::Due => SyncJobStatus::due,
            JobStatus::Waiting => SyncJobStatus::waiting,
            JobStatus::BackingOff => SyncJobStatus::backing_off,
        },
        last_ok: Some(j.last_ok as i64).filter(|&at| at > 0),
        next_due: j.next_due.map(|at| at as i64),
        running_since: j.running_since.map(|at| at as i64),
        failures: i64::from(j.failures),
        last_error: j.last_error,
        unavailable: Some(j.unavailable),
        full: j.full,
        fetched: j.fetched.map(|rows| rows as i64),
        expected: j.expected.map(|rows| rows as i64),
    }
}

/// A contribution event. `project` is its project where that is stored,
/// `item_url` the stored issue's or merge request's link; without the item
/// the link is built from the project's, and without both it is `None`.
pub fn activity(
    e: model::Event,
    project: Option<&model::Project>,
    item_url: Option<String>,
) -> ActivityEvent {
    let (target_type, target_iid) = {
        let (kind, iid) = e.target();
        (kind.to_string(), iid)
    };
    let push = e.push_data;
    let description = if e.note.body.is_empty() {
        push.commit_title.clone()
    } else {
        e.note.body
    };
    let is_push = !push.git_ref.is_empty();
    let base = project
        .map(|p| p.web_url.as_str())
        .filter(|u| !u.is_empty());
    let web_url = item_url.and_then(some).or_else(|| {
        let base = base?;
        Some(match target_type.as_str() {
            "Issue" if target_iid > 0 => format!("{base}/-/issues/{target_iid}"),
            "MergeRequest" if target_iid > 0 => format!("{base}/-/merge_requests/{target_iid}"),
            "WorkItem" if target_iid > 0 => format!("{base}/-/work_items/{target_iid}"),
            // A deleted branch has no commits to show.
            _ if is_push && !push.commit_title.is_empty() => {
                format!("{base}/-/commits/{}", push.git_ref)
            }
            _ => base.to_string(),
        })
    });
    let project_path = project
        .map(|p| p.path_with_namespace.clone())
        .and_then(some)
        // A tracked project the user is no member of has no row, but its
        // assigned items carry the path in their link.
        .or_else(|| web_url.as_deref().map(namespace_of).and_then(some));
    ActivityEvent {
        timestamp: e.created_at as i64,
        action: e.action_name,
        target_type: some(target_type),
        target_iid: (target_iid > 0).then_some(target_iid),
        target_title: some(e.target_title),
        project_id: (e.project_id > 0).then_some(e.project_id),
        project_path,
        web_url,
        commit_count: is_push.then_some(push.commit_count),
        r#ref: some(push.git_ref),
        commit_title: some(push.commit_title),
        description: some(description),
    }
}

/// `None` for the empty string.
pub fn some(s: String) -> Option<String> {
    (!s.is_empty()).then_some(s)
}

/// Internal → wire issuable kind: an issue is a work item.
pub fn kind(kind: Issuable) -> IssuableKind {
    match kind {
        Issuable::Issue => IssuableKind::work_item,
        Issuable::MergeRequest => IssuableKind::merge_request,
    }
}

/// Wire → internal issuable kind. The only place the generated enum's
/// lowercase variants are matched.
pub fn internal_kind(kind: &IssuableKind) -> Issuable {
    match kind {
        IssuableKind::work_item => Issuable::Issue,
        IssuableKind::merge_request => Issuable::MergeRequest,
    }
}

/// Wire → GitLab's name of a work item state, which is what the stored rows
/// (and `WorkItem.state`) carry.
pub fn issue_state(state: &WorkItemState) -> &'static str {
    match state {
        WorkItemState::opened => "opened",
        WorkItemState::closed => "closed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> model::Project {
        model::Project {
            id: 7,
            path_with_namespace: "team/api".into(),
            web_url: "https://gl/team/api".into(),
            ..Default::default()
        }
    }

    fn event(action: &str, target_type: &str, target_iid: i64) -> model::Event {
        model::Event {
            id: 1,
            project_id: 7,
            action_name: action.into(),
            target_type: target_type.into(),
            target_iid,
            created_at: 100,
            ..Default::default()
        }
    }

    fn info(key: &str, status: JobStatus) -> JobInfo {
        JobInfo {
            key: key.into(),
            status,
            last_ok: 0,
            next_due: None,
            running_since: None,
            full: None,
            fetched: None,
            expected: None,
            failures: 0,
            last_error: None,
            unavailable: false,
        }
    }

    /// An unavailable job says so on the wire and keeps its schedule and its
    /// error; every other job carries an explicit `false`.
    #[test]
    fn a_sync_job_says_whether_it_is_unavailable() {
        let refused = sync_job(JobInfo {
            last_ok: 0,
            next_due: Some(1_800_086_400),
            failures: 3,
            last_error: Some("GitLab error: 403 Forbidden".into()),
            unavailable: true,
            ..info("project/9/boards", JobStatus::Waiting)
        });
        assert_eq!(refused.unavailable, Some(true));
        assert_eq!(refused.status, SyncJobStatus::waiting);
        assert_eq!(refused.next_due, Some(1_800_086_400));
        assert_eq!(refused.failures, 3);
        assert_eq!(
            refused.last_error.as_deref(),
            Some("GitLab error: 403 Forbidden")
        );
        assert_eq!(refused.last_ok, None);
        let json = serde_json::to_value(&refused).unwrap();
        assert_eq!(json["unavailable"], true);

        let failing = sync_job(JobInfo {
            failures: 1,
            last_error: Some("GitLab error: 403 Forbidden".into()),
            ..info("project/9/boards", JobStatus::BackingOff)
        });
        assert_eq!(failing.unavailable, Some(false));
        assert_eq!(failing.status, SyncJobStatus::backing_off);
        let json = serde_json::to_value(&failing).unwrap();
        assert_eq!(json["unavailable"], false);
        assert_eq!(
            sync_job(info("events", JobStatus::Running)).unavailable,
            Some(false)
        );
    }

    /// Progress is a running job's: any other job carries none.
    #[test]
    fn a_sync_job_carries_its_progress_only_while_it_runs() {
        let running = sync_job(JobInfo {
            running_since: Some(1_800_000_000),
            full: Some(true),
            fetched: Some(400),
            expected: Some(1000),
            ..info("project/9/issues", JobStatus::Running)
        });
        assert_eq!(
            (running.full, running.fetched, running.expected),
            (Some(true), Some(400), Some(1000))
        );
        let json = serde_json::to_value(&running).unwrap();
        assert_eq!(
            (&json["full"], &json["fetched"], &json["expected"]),
            (&true.into(), &400.into(), &1000.into())
        );

        let waiting = sync_job(info("project/9/issues", JobStatus::Waiting));
        let json = serde_json::to_value(&waiting).unwrap();
        for key in ["full", "fetched", "expected"] {
            assert!(json.get(key).is_none(), "{key} in {json}");
        }
    }

    #[test]
    fn items_name_the_stored_project_or_the_one_in_their_link() {
        let item = || model::MergeRequest {
            web_url: "https://gl/other/big/-/merge_requests/5".into(),
            ..Default::default()
        };
        let info = |path: Option<&str>| ProjectInfo {
            path: path.map(str::to_string),
            avatar: None,
        };
        let stored = merge_request(item(), 0, info(Some("team/api")));
        assert_eq!(stored.project_path.as_deref(), Some("team/api"));
        let foreign = merge_request(item(), 0, info(None));
        assert_eq!(foreign.project_path.as_deref(), Some("other/big"));
        let unknown = issue(model::Issue::default(), None, 0, info(None), None);
        assert_eq!(unknown.namespace_path, None);
        let json = serde_json::to_value(&unknown).unwrap();
        for absent in ["namespace_path", "project_avatar", "updated_at"] {
            assert!(json.get(absent).is_none(), "{absent} in {json}");
        }
    }

    /// Known times are sent, a row's 0 (unread or unknown) is left out.
    #[test]
    fn an_unknown_update_time_is_absent() {
        let mr = |updated_at| model::MergeRequest {
            updated_at,
            ..Default::default()
        };
        assert_eq!(merge_request(mr(0), 0, no_project()).updated_at, None);
        let known = merge_request(mr(1_782_900_000), 0, no_project());
        assert_eq!(known.updated_at, Some(1_782_900_000));
    }

    fn no_project() -> ProjectInfo {
        ProjectInfo {
            path: None,
            avatar: None,
        }
    }

    /// An issue is a project's work item of its type, in the epic it names.
    #[test]
    fn an_issue_is_a_work_item_of_its_project() {
        let row = || model::Issue {
            id: 7042,
            iid: 42,
            project_id: 7,
            web_url: "https://gl.test:8443/team/api/-/work_items/42".into(),
            issue_type: "task".into(),
            epic: Some(model::EpicRef {
                id: 30,
                iid: 5,
                group_id: 3,
                title: "Accounts".into(),
                url: "/groups/team/-/epics/5".into(),
            }),
            ..Default::default()
        };
        let task = issue(row(), None, 0, no_project(), None);
        assert_eq!((task.id, task.iid), (7042, 42));
        assert_eq!(task.r#type, "task");
        assert_eq!((task.project_id, task.group_id), (Some(7), None));
        assert_eq!(task.namespace_path.as_deref(), Some("team/api"));
        assert_eq!(
            task.parent,
            Some(WorkItemRef {
                project_id: None,
                group_id: Some(3),
                iid: 5,
                r#type: Some("epic".into()),
                title: Some("Accounts".into()),
                web_url: Some("https://gl.test:8443/groups/team/-/epics/5".into()),
            }),
            "the link made absolute by the issue's own"
        );

        let stored = Some("https://gl.test/groups/team/-/epics/5".to_string());
        let parent = issue(row(), None, 0, no_project(), stored.clone()).parent;
        assert_eq!(
            parent.unwrap().web_url,
            stored,
            "the stored epic's link wins"
        );

        let untyped = model::Issue {
            issue_type: String::new(),
            epic: None,
            ..row()
        };
        let plain = issue(untyped, None, 0, no_project(), None);
        assert_eq!(plain.r#type, "issue");
        assert_eq!(plain.parent, None);
    }

    /// In seconds, 0 when none is logged; a timelog's too, and only a queued
    /// one carries a duration.
    #[test]
    fn time_spent_is_in_seconds() {
        let spent = model::Issue {
            time_stats: Some(model::TimeStats {
                total_time_spent: 5400,
            }),
            ..Default::default()
        };
        assert_eq!(
            issue(spent, None, 0, no_project(), None).time_spent,
            Some(5400)
        );
        let none = issue(model::Issue::default(), None, 0, no_project(), None);
        assert_eq!(none.time_spent, Some(0));

        let logged = timelog(model::Timelog {
            time_spent: 1800,
            ..Default::default()
        });
        assert_eq!((logged.time_spent, logged.duration), (Some(1800), None));
    }

    #[test]
    fn a_parent_link_is_absolute_or_none() {
        let base = "https://gl.test/team/api/-/issues/1";
        assert_eq!(
            absolute("https://other.test/groups/g/-/epics/1", base).as_deref(),
            Some("https://other.test/groups/g/-/epics/1"),
            "an absolute link stays"
        );
        assert_eq!(
            absolute("/groups/g/-/epics/1", base).as_deref(),
            Some("https://gl.test/groups/g/-/epics/1")
        );
        assert_eq!(absolute("/groups/g/-/epics/1", ""), None);
        assert_eq!(absolute("/groups/g/-/epics/1", "gl.test/team"), None);
        assert_eq!(absolute("groups/g/-/epics/1", base), None);
        assert_eq!(absolute("", base), None);

        // A reference stored before it had its group and number: no parent.
        let old = model::EpicRef {
            url: "/groups/g/-/epics/1".into(),
            ..Default::default()
        };
        assert_eq!(parent(&old, None, base), None);
        let unlinked = model::EpicRef {
            iid: 1,
            group_id: 3,
            ..Default::default()
        };
        let unlinked = parent(&unlinked, None, base).unwrap();
        assert_eq!((unlinked.web_url, unlinked.title), (None, None));
    }

    /// An epic is a group's work item, identified by its work item id.
    #[test]
    fn an_epic_is_a_work_item_of_its_group() {
        let e = epic(
            model::Epic {
                id: 30,
                iid: 5,
                group_id: 3,
                work_item_id: 9001,
                web_url: "https://gl/groups/team/-/epics/5".into(),
                ..Default::default()
            },
            2,
            None,
        );
        assert_eq!((e.id, e.iid), (9001, 5), "never the legacy id");
        assert_eq!(e.r#type, "epic");
        assert_eq!((e.project_id, e.group_id), (None, Some(3)));
        assert_eq!(e.namespace_path.as_deref(), Some("team"));
        assert_eq!(e.parent, None);
        assert_eq!(e.open_count, 2);
        assert_eq!((e.time_spent, e.project_avatar), (None, None));
        assert!(e.graph_status.is_empty());
    }

    #[test]
    fn epics_name_the_stored_group_or_the_one_in_their_link() {
        let item = || model::Epic {
            web_url: "https://gl/groups/other/big/-/epics/7".into(),
            ..Default::default()
        };
        let stored = epic(item(), 0, Some("team".into()));
        assert_eq!(stored.namespace_path.as_deref(), Some("team"));
        let foreign = epic(item(), 0, None);
        assert_eq!(
            foreign.namespace_path.as_deref(),
            Some("other/big"),
            "without the `groups/` prefix"
        );
        let unknown = epic(model::Epic::default(), 0, Some(String::new()));
        assert_eq!(unknown.namespace_path, None);
    }

    #[test]
    fn activity_links_the_item_the_project_or_nothing() {
        let p = project();
        let stored = activity(
            event("closed", "Issue", 3),
            Some(&p),
            Some("https://gl/team/api/-/issues/3".into()),
        );
        assert_eq!(
            stored.web_url.as_deref(),
            Some("https://gl/team/api/-/issues/3")
        );
        assert_eq!(stored.project_path.as_deref(), Some("team/api"));
        assert_eq!(stored.target_iid, Some(3));
        assert_eq!(stored.r#ref, None);
        assert_eq!(stored.commit_count, None);

        let built = activity(event("opened", "MergeRequest", 4), Some(&p), None);
        assert_eq!(
            built.web_url.as_deref(),
            Some("https://gl/team/api/-/merge_requests/4")
        );

        let joined = activity(event("joined", "", 0), Some(&p), None);
        assert_eq!(joined.web_url.as_deref(), Some("https://gl/team/api"));
        assert_eq!(joined.target_type, None);
        assert_eq!(joined.target_iid, None);
        assert_eq!(joined.target_title, None);

        // No project row: the path comes from the item's link.
        let foreign = activity(
            event("opened", "Issue", 5),
            None,
            Some("https://gl/other/big/-/issues/5".into()),
        );
        assert_eq!(foreign.project_path.as_deref(), Some("other/big"));

        let unknown = activity(event("opened", "Issue", 5), None, None);
        assert_eq!(unknown.web_url, None);
        assert_eq!(unknown.project_path, None);
        assert_eq!(unknown.project_id, Some(7));

        // A group's or the user's own event has no project.
        let outside = model::Event {
            project_id: 0,
            ..event("joined", "", 0)
        };
        let outside = activity(outside, None, None);
        assert_eq!(outside.project_id, None);
        assert_eq!(outside.target_type, None);
        let json = serde_json::to_value(&outside).unwrap();
        assert!(json.get("project_id").is_none() && json.get("target_type").is_none());
    }

    #[test]
    fn activity_shows_pushes_and_what_a_comment_is_on() {
        let p = project();
        // GitLab sends the project's id as a push's target number.
        let mut push = event("pushed to", "Project", 7);
        push.push_data = model::PushData {
            git_ref: "feat/x".into(),
            commit_count: 2,
            commit_title: "Fix it".into(),
        };
        let mut deleted = push.clone();
        deleted.action_name = "deleted".into();
        deleted.push_data.commit_count = 0;
        deleted.push_data.commit_title.clear();

        let push = activity(push, Some(&p), None);
        assert_eq!(push.r#ref.as_deref(), Some("feat/x"));
        assert_eq!(push.target_iid, None);
        assert_eq!(push.commit_count, Some(2));
        assert_eq!(push.commit_title.as_deref(), Some("Fix it"));
        assert_eq!(push.description.as_deref(), Some("Fix it"));
        assert_eq!(
            push.web_url.as_deref(),
            Some("https://gl/team/api/-/commits/feat/x")
        );

        let deleted = activity(deleted, Some(&p), None);
        assert_eq!(deleted.commit_count, Some(0));
        assert_eq!(deleted.commit_title, None);
        assert_eq!(deleted.description, None);
        assert_eq!(deleted.web_url.as_deref(), Some("https://gl/team/api"));

        let mut comment = event("commented on", "DiffNote", 9001);
        comment.target_title = "Add x".into();
        comment.note = model::NoteRef {
            body: "lgtm".into(),
            noteable_type: "MergeRequest".into(),
            noteable_iid: 12,
        };
        let comment = activity(comment, Some(&p), None);
        assert_eq!(comment.target_type.as_deref(), Some("MergeRequest"));
        assert_eq!(comment.target_iid, Some(12));
        assert_eq!(comment.target_title.as_deref(), Some("Add x"));
        assert_eq!(comment.description.as_deref(), Some("lgtm"));
        assert_eq!(comment.commit_title, None);
        assert_eq!(
            comment.web_url.as_deref(),
            Some("https://gl/team/api/-/merge_requests/12")
        );
    }
}
