//! Result rows for the desktop shells, mirrored from the noctalia plugin
//! (`forskap/launcher.luau`): same ids, titles and subtitles, so the
//! launchers behave alike and `forskap issue open` counts the same thing.

use forskap_api::Search_Reply;

use super::query::Kind;
use crate::refspec::RefKind;

/// One launcher entry. `score` is the daemon's open count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub kind: Kind,
    pub score: i64,
    pub url: String,
}

/// What a result id points at once the user picks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Issuable {
        kind: RefKind,
        project_id: i64,
        iid: i64,
    },
    Url(String),
}

/// Flatten a `Search` reply in display order: issues, MRs, projects, groups.
pub fn rows(reply: &Search_Reply) -> Vec<Row> {
    let mut out = Vec::new();
    for i in &reply.issues {
        out.push(Row {
            id: format!("issues:{}:{}", i.project_id, i.iid),
            title: format!("#{} {}", i.iid, i.title),
            subtitle: join(&[project_path(&i.web_url), &i.state, &i.total_time]),
            kind: Kind::Issues,
            score: i.open_count,
            url: i.web_url.clone(),
        });
    }
    for m in &reply.merge_requests {
        out.push(Row {
            id: format!("merge_requests:{}:{}", m.project_id, m.iid),
            title: format!("!{} {}", m.iid, m.title),
            subtitle: join(&[project_path(&m.web_url), &m.state]),
            kind: Kind::MergeRequests,
            score: m.open_count,
            url: m.web_url.clone(),
        });
    }
    for p in &reply.projects {
        out.push(Row {
            id: format!("url:{}", p.web_url),
            title: p.path.clone(),
            subtitle: p.name.clone(),
            kind: Kind::Projects,
            score: 0,
            url: p.web_url.clone(),
        });
    }
    for g in &reply.groups {
        out.push(Row {
            id: format!("url:{}", g.web_url),
            title: g.path.clone(),
            subtitle: g.name.clone(),
            kind: Kind::Groups,
            score: 0,
            url: g.web_url.clone(),
        });
    }
    out
}

/// `"https://gl/team/api/-/issues/42"` → `"team/api"`; empty when the URL
/// doesn't have GitLab's `/-/` separator.
pub fn project_path(web_url: &str) -> &str {
    let Some(rest) = web_url
        .strip_prefix("https://")
        .or_else(|| web_url.strip_prefix("http://"))
    else {
        return "";
    };
    let Some((_, path)) = rest.split_once('/') else {
        return "";
    };
    path.split_once("/-/").map_or("", |(p, _)| p)
}

/// Parse an id produced by [`rows`].
pub fn parse_id(id: &str) -> Option<Target> {
    if let Some(url) = id.strip_prefix("url:") {
        return (!url.is_empty()).then(|| Target::Url(url.to_string()));
    }
    let mut parts = id.splitn(3, ':');
    let kind = match parts.next()? {
        "issues" => RefKind::Issue,
        "merge_requests" => RefKind::Mr,
        _ => return None,
    };
    let project_id = parts.next()?.parse().ok()?;
    let iid = parts.next()?.parse().ok()?;
    Some(Target::Issuable {
        kind,
        project_id,
        iid,
    })
}

fn join(parts: &[&str]) -> String {
    parts
        .iter()
        .filter(|p| !p.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use forskap_api::{Group, Issue, MergeRequest, Project};

    fn reply() -> Search_Reply {
        Search_Reply {
            issues: vec![Issue {
                id: 1,
                iid: 42,
                project_id: 7,
                title: "Fix login".into(),
                web_url: "https://gl.example.com/team/api/-/issues/42".into(),
                state: "opened".into(),
                parent: String::new(),
                total_time: "1h".into(),
                graph_status: String::new(),
                open_count: 3,
            }],
            merge_requests: vec![MergeRequest {
                id: 2,
                iid: 9,
                project_id: 7,
                title: "Add OAuth".into(),
                web_url: "https://gl.example.com/team/api/-/merge_requests/9".into(),
                state: "merged".into(),
                assignees: vec![],
                open_count: 0,
            }],
            projects: vec![Project {
                id: 7,
                name: "API".into(),
                path: "team/api".into(),
                web_url: "https://gl.example.com/team/api".into(),
            }],
            groups: vec![Group {
                id: 3,
                name: "Team".into(),
                path: "team".into(),
                web_url: "https://gl.example.com/groups/team".into(),
            }],
        }
    }

    #[test]
    fn rows_follow_the_plugin_shape() {
        let rows = rows(&reply());
        let ids: Vec<_> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "issues:7:42",
                "merge_requests:7:9",
                "url:https://gl.example.com/team/api",
                "url:https://gl.example.com/groups/team",
            ]
        );
        assert_eq!(rows[0].title, "#42 Fix login");
        assert_eq!(rows[0].subtitle, "team/api · opened · 1h");
        assert_eq!(rows[0].score, 3);
        assert_eq!(rows[1].title, "!9 Add OAuth");
        assert_eq!(rows[1].subtitle, "team/api · merged");
        assert_eq!(rows[2].title, "team/api");
        assert_eq!(rows[2].subtitle, "API");
        assert_eq!(rows[3].kind, Kind::Groups);
    }

    #[test]
    fn project_path_needs_the_gitlab_separator() {
        assert_eq!(project_path("https://gl/team/api/-/issues/42"), "team/api");
        assert_eq!(project_path("http://gl/a/b/c/-/merge_requests/1"), "a/b/c");
        assert_eq!(project_path("https://gl/team/api"), "");
        assert_eq!(project_path(""), "");
    }

    #[test]
    fn ids_round_trip() {
        assert_eq!(
            parse_id("issues:7:42"),
            Some(Target::Issuable {
                kind: RefKind::Issue,
                project_id: 7,
                iid: 42
            })
        );
        assert_eq!(
            parse_id("merge_requests:7:9"),
            Some(Target::Issuable {
                kind: RefKind::Mr,
                project_id: 7,
                iid: 9
            })
        );
        assert_eq!(
            parse_id("url:https://gl/a:b"),
            Some(Target::Url("https://gl/a:b".into()))
        );
        assert_eq!(parse_id("url:"), None);
        assert_eq!(parse_id("issues:x:1"), None);
        assert_eq!(parse_id("boards:1:2"), None);
        assert_eq!(parse_id(""), None);
    }
}
