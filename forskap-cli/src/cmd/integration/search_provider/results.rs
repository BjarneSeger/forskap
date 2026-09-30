//! Result rows for the desktop shells, mirrored from the noctalia plugin
//! (`forskap/launcher.luau`): same ids, titles and subtitles, so the
//! launchers behave alike and `forskap issue open` counts the same thing.

use std::path::Path;

use forskap_api::Search_Reply;

use super::query::Kind;
use crate::item;
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
    /// The project's avatar file, as the daemon downloaded it.
    pub avatar: Option<String>,
}

impl Row {
    /// What the shells show next to the row. Both take an absolute path
    /// where they take a themed icon name. The avatars are cache files, so
    /// one that is gone falls back to the kind's icon instead of a blank.
    pub fn icon(&self) -> &str {
        match &self.avatar {
            Some(file) if Path::new(file).is_file() => file,
            _ => self.kind.icon(),
        }
    }
}

/// An avatar path off the wire, where empty means none.
fn avatar(path: &str) -> Option<String> {
    (!path.is_empty()).then(|| path.to_string())
}

/// What a result id points at once the user picks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Issuable {
        kind: RefKind,
        project_id: i64,
        iid: i64,
    },
    Epic {
        group_id: i64,
        iid: i64,
    },
    Url(String),
}

/// Flatten a `Search` reply in display order: issues, MRs, epics, projects,
/// groups.
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
            avatar: avatar(&i.project_avatar),
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
            avatar: avatar(&m.project_avatar),
        });
    }
    for e in &reply.epics {
        out.push(Row {
            id: format!("epic:{}:{}", e.group_id, e.iid),
            title: format!("&{} {}", e.iid, e.title),
            subtitle: join(&[group_path(&e.web_url), &e.state]),
            kind: Kind::Epics,
            score: e.open_count,
            url: e.web_url.clone(),
            avatar: None,
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
            avatar: avatar(&p.avatar),
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
            avatar: None,
        });
    }
    out
}

/// [`item::project_path`], empty when the URL doesn't give the path away.
fn project_path(web_url: &str) -> &str {
    item::project_path(web_url).unwrap_or_default()
}

/// `"https://gl/groups/team/backend/-/epics/5"` → `"team/backend"`: group
/// pages live under `/groups/`, which is no part of the group's path.
pub fn group_path(web_url: &str) -> &str {
    let path = project_path(web_url);
    path.strip_prefix("groups/").unwrap_or(path)
}

/// Parse an id produced by [`rows`].
pub fn parse_id(id: &str) -> Option<Target> {
    if let Some(url) = id.strip_prefix("url:") {
        return (!url.is_empty()).then(|| Target::Url(url.to_string()));
    }
    let mut parts = id.splitn(3, ':');
    let kind = parts.next()?;
    // The project of an issue or MR, the group of an epic.
    let owner = parts.next()?.parse().ok()?;
    let iid = parts.next()?.parse().ok()?;
    let kind = match kind {
        "issues" => RefKind::Issue,
        "merge_requests" => RefKind::Mr,
        "epic" => {
            return Some(Target::Epic {
                group_id: owner,
                iid,
            });
        }
        _ => return None,
    };
    Some(Target::Issuable {
        kind,
        project_id: owner,
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
    use forskap_api::{Epic, Group, Issue, MergeRequest, Project};

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
                project_avatar: "/cache/avatars/7-a.png".into(),
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
                project_avatar: "/cache/avatars/7-a.png".into(),
            }],
            projects: vec![Project {
                id: 7,
                name: "API".into(),
                path: "team/api".into(),
                web_url: "https://gl.example.com/team/api".into(),
                avatar: String::new(),
            }],
            groups: vec![Group {
                id: 3,
                name: "Team".into(),
                path: "team".into(),
                web_url: "https://gl.example.com/groups/team".into(),
            }],
            epics: vec![Epic {
                id: 30,
                iid: 5,
                group_id: 3,
                title: "Accounts".into(),
                web_url: "https://gl.example.com/groups/team/-/epics/5".into(),
                state: "opened".into(),
                open_count: 2,
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
                "epic:3:5",
                "url:https://gl.example.com/team/api",
                "url:https://gl.example.com/groups/team",
            ]
        );
        assert_eq!(rows[0].title, "#42 Fix login");
        assert_eq!(rows[0].subtitle, "team/api · opened · 1h");
        assert_eq!(rows[0].score, 3);
        assert_eq!(rows[1].title, "!9 Add OAuth");
        assert_eq!(rows[1].subtitle, "team/api · merged");
        assert_eq!(rows[2].title, "&5 Accounts");
        assert_eq!(rows[2].subtitle, "team · opened", "no `groups/` prefix");
        assert_eq!(rows[2].score, 2);
        assert_eq!(rows[3].title, "team/api");
        assert_eq!(rows[3].subtitle, "API");
        assert_eq!(rows[4].kind, Kind::Groups);
    }

    #[test]
    fn rows_carry_the_project_avatar() {
        let avatars: Vec<_> = rows(&reply()).into_iter().map(|r| r.avatar).collect();
        let of_project = Some("/cache/avatars/7-a.png".to_string());
        assert_eq!(avatars, [of_project.clone(), of_project, None, None, None]);
    }

    #[test]
    fn the_icon_is_the_avatar_while_its_file_exists() {
        let file = std::env::temp_dir().join(format!("forskap-icon-{}.png", std::process::id()));
        std::fs::write(&file, b"png").unwrap();
        let mut row = rows(&reply()).remove(0);
        row.avatar = Some(file.to_str().unwrap().to_string());
        assert_eq!(row.icon(), file.to_str().unwrap());

        std::fs::remove_file(&file).unwrap();
        assert_eq!(row.icon(), Kind::Issues.icon());
        row.avatar = None;
        assert_eq!(row.icon(), Kind::Issues.icon());
    }

    #[test]
    fn project_path_is_empty_without_a_resource() {
        assert_eq!(project_path("https://gl/team/api/-/issues/42"), "team/api");
        assert_eq!(project_path("http://gl/a/b/c/-/merge_requests/1"), "a/b/c");
        assert_eq!(project_path("https://gl/team/api"), "");
        assert_eq!(project_path(""), "");
    }

    #[test]
    fn group_path_drops_the_groups_prefix() {
        assert_eq!(group_path("https://gl/groups/team/-/epics/5"), "team");
        assert_eq!(group_path("https://gl/groups/a/b/-/epics/5"), "a/b");
        assert_eq!(group_path("https://gl/groups/team"), "");
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
            parse_id("epic:3:5"),
            Some(Target::Epic {
                group_id: 3,
                iid: 5
            })
        );
        assert_eq!(parse_id("epic:3"), None);
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
