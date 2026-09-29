//! Projections of stored GitLab rows onto the varlink wire types.

use forskap_api::{Group, HistoryEvent, IssuableKind, Issue, MergeRequest, Project};

use crate::gitlab::{Issuable, format_duration};
use crate::query::graph_status_from;
use crate::sync::model;

/// `board_labels` are the issue's project board lists, `None` when never
/// synced (then `graph_status` stays empty).
pub fn issue(
    i: model::Issue,
    board_labels: Option<&[String]>,
    open_count: i64,
    project_avatar: String,
) -> Issue {
    Issue {
        graph_status: graph_status_from(board_labels, &i.labels, &i.state),
        parent: i.parent_url().to_string(),
        total_time: i.total_time().to_string(),
        id: i.id,
        iid: i.iid,
        project_id: i.project_id,
        title: i.title,
        web_url: i.web_url,
        state: i.state,
        open_count,
        project_avatar,
    }
}

pub fn merge_request(
    m: model::MergeRequest,
    open_count: i64,
    project_avatar: String,
) -> MergeRequest {
    MergeRequest {
        id: m.id,
        iid: m.iid,
        project_id: m.project_id,
        title: m.title,
        web_url: m.web_url,
        state: m.state,
        assignees: m.assignees.into_iter().map(|a| a.username).collect(),
        open_count,
        project_avatar,
    }
}

pub fn project(p: model::Project, avatar: String) -> Project {
    Project {
        id: p.id,
        name: p.name,
        path: p.path_with_namespace,
        web_url: p.web_url,
        avatar,
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

/// A synced timelog as a `"gitlab"` history event.
pub fn timelog(t: model::Timelog) -> HistoryEvent {
    HistoryEvent {
        timestamp: t.spent_at as i64,
        source: "gitlab".to_string(),
        kind: kind(t.kind),
        project_id: t.project_id,
        iid: t.iid,
        title: t.title,
        web_url: t.web_url,
        duration: format_duration(t.time_spent),
        summary: t.summary,
    }
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
