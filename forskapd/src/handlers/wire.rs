//! Projections of stored GitLab rows onto the varlink wire types.

use forskap_api::{
    ActivityEvent, Epic, Group, HistoryEvent, HistorySource, IssuableKind, Issue, MergeRequest,
    Project, SyncJob, SyncJobStatus,
};

use crate::gitlab::{Issuable, format_duration};
use crate::query::{graph_status_from, namespace_of};
use crate::sync::{JobInfo, JobStatus, model};

/// What a wire item shows of its project.
pub struct ProjectInfo {
    /// `path_with_namespace` of the stored project.
    pub path: Option<String>,
    pub avatar: String,
}

/// A tracked project the user is no member of has no row, but its items
/// carry the path in their link.
fn project_path(stored: Option<String>, web_url: &str) -> String {
    stored
        .and_then(some)
        .unwrap_or_else(|| namespace_of(web_url))
}

/// `board_labels` are the issue's project board lists, `None` when never
/// synced (then `graph_status` stays empty).
pub fn issue(
    i: model::Issue,
    board_labels: Option<&[String]>,
    open_count: i64,
    project: ProjectInfo,
) -> Issue {
    Issue {
        graph_status: graph_status_from(board_labels, &i.labels, &i.state),
        parent: i.parent_url().to_string(),
        total_time: i.total_time().to_string(),
        id: i.id,
        iid: i.iid,
        project_id: i.project_id,
        title: i.title,
        state: i.state,
        open_count,
        project_avatar: project.avatar,
        project_path: project_path(project.path, &i.web_url),
        web_url: i.web_url,
        updated_at: i.updated_at as i64,
    }
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
        updated_at: m.updated_at as i64,
    }
}

pub fn project(p: model::Project, avatar: String) -> Project {
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

/// `group_path` is the stored group's `full_path`, `None` without a row.
pub fn epic(e: model::Epic, open_count: i64, group_path: Option<String>) -> Epic {
    Epic {
        group_path: self::group_path(group_path, &e.web_url),
        id: e.id,
        iid: e.iid,
        group_id: e.group_id,
        title: e.title,
        web_url: e.web_url,
        state: e.state,
        open_count,
        updated_at: e.updated_at as i64,
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
        title: t.title,
        web_url: t.web_url,
        duration: format_duration(t.time_spent),
        summary: t.summary,
    }
}

/// A sync job as the worker reported it; a job that never ran has no
/// `last_ok`.
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
    }
}

/// A contribution event. `project` is its project where that is stored,
/// `item_url` the stored issue's or merge request's link; without the item
/// the link is built from the project's, and without both it stays empty.
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
        target_type,
        target_iid: (target_iid > 0).then_some(target_iid),
        target_title: some(e.target_title),
        project_id: e.project_id,
        project_path,
        web_url,
        commit_count: is_push.then_some(push.commit_count),
        r#ref: some(push.git_ref),
        commit_title: some(push.commit_title),
        description: some(description),
    }
}

/// `None` for the empty string.
fn some(s: String) -> Option<String> {
    (!s.is_empty()).then_some(s)
}

/// Internal → wire issuable kind.
pub fn kind(kind: Issuable) -> IssuableKind {
    match kind {
        Issuable::Issue => IssuableKind::issue,
        Issuable::MergeRequest => IssuableKind::merge_request,
    }
}

/// Wire → internal issuable kind. The only place the generated enum's
/// lowercase variants are matched.
pub fn internal_kind(kind: &IssuableKind) -> Issuable {
    match kind {
        IssuableKind::issue => Issuable::Issue,
        IssuableKind::merge_request => Issuable::MergeRequest,
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

    #[test]
    fn items_name_the_stored_project_or_the_one_in_their_link() {
        let item = || model::MergeRequest {
            web_url: "https://gl/other/big/-/merge_requests/5".into(),
            ..Default::default()
        };
        let info = |path: Option<&str>| ProjectInfo {
            path: path.map(str::to_string),
            avatar: String::new(),
        };
        let stored = merge_request(item(), 0, info(Some("team/api")));
        assert_eq!(stored.project_path, "team/api");
        let foreign = merge_request(item(), 0, info(None));
        assert_eq!(foreign.project_path, "other/big");
        let unknown = issue(model::Issue::default(), None, 0, info(None));
        assert_eq!(unknown.project_path, "");
    }

    #[test]
    fn epics_name_the_stored_group_or_the_one_in_their_link() {
        let item = || model::Epic {
            web_url: "https://gl/groups/other/big/-/epics/7".into(),
            ..Default::default()
        };
        let stored = epic(item(), 0, Some("team".into()));
        assert_eq!(stored.group_path, "team");
        let foreign = epic(item(), 0, None);
        assert_eq!(
            foreign.group_path, "other/big",
            "without the `groups/` prefix"
        );
        let unknown = epic(model::Epic::default(), 0, Some(String::new()));
        assert_eq!(unknown.group_path, "");
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
        assert_eq!(unknown.project_id, 7);
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
        assert_eq!(comment.target_type, "MergeRequest");
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
