//! A cached issue or merge request row, whichever kind it is.

use forskap_api::{Issue, MergeRequest};

use crate::refspec::RefKind;

/// Wraps the generated structs, which we don't own and which share no trait.
pub enum Item {
    Issue(Issue),
    Mr(MergeRequest),
}

impl Item {
    pub fn kind(&self) -> RefKind {
        match self {
            Item::Issue(_) => RefKind::Issue,
            Item::Mr(_) => RefKind::Mr,
        }
    }

    pub fn project_id(&self) -> i64 {
        match self {
            Item::Issue(i) => i.project_id,
            Item::Mr(m) => m.project_id,
        }
    }

    pub fn iid(&self) -> i64 {
        match self {
            Item::Issue(i) => i.iid,
            Item::Mr(m) => m.iid,
        }
    }

    pub fn title(&self) -> &str {
        match self {
            Item::Issue(i) => &i.title,
            Item::Mr(m) => &m.title,
        }
    }

    pub fn web_url(&self) -> &str {
        match self {
            Item::Issue(i) => &i.web_url,
            Item::Mr(m) => &m.web_url,
        }
    }

    /// The project's full path if the URL gives it away, else `project <id>`.
    pub fn project(&self) -> String {
        match project_path(self.web_url()) {
            Some(path) => path.to_string(),
            None => format!("project {}", self.project_id()),
        }
    }
}

/// The project path inside an issue or MR URL
/// (`https://host/<path>/-/issues/<iid>`): the rows carry only the project ID.
pub fn project_path(web_url: &str) -> Option<&str> {
    let (_, rest) = web_url.split_once("://")?;
    let (_, path) = rest.split_once('/')?;
    let (project, _) = path.split_once("/-/")?;
    Some(project).filter(|p| !p.is_empty())
}

#[cfg(test)]
pub mod testing {
    use super::*;

    /// A row in `project_id` whose URL names `path`; an empty `path` leaves
    /// the URL out.
    pub fn item(kind: RefKind, project_id: i64, path: &str, iid: i64, title: &str) -> Item {
        let url = |resource: &str| match path {
            "" => String::new(),
            path => format!("https://gitlab.example.com/{path}/-/{resource}/{iid}"),
        };
        match kind {
            RefKind::Issue => Item::Issue(Issue {
                id: project_id * 1000 + iid,
                iid,
                project_id,
                title: title.to_string(),
                web_url: url("issues"),
                state: "opened".to_string(),
                parent: String::new(),
                total_time: String::new(),
                graph_status: String::new(),
                open_count: 0,
                project_avatar: String::new(),
            }),
            RefKind::Mr => Item::Mr(MergeRequest {
                id: project_id * 1000 + iid,
                iid,
                project_id,
                title: title.to_string(),
                web_url: url("merge_requests"),
                state: "opened".to_string(),
                assignees: Vec::new(),
                open_count: 0,
                project_avatar: String::new(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_path_is_what_precedes_the_resource() {
        for (url, path) in [
            ("https://gitlab.com/team/api/-/issues/42", Some("team/api")),
            (
                "https://gitlab.example.com/a/b/c/-/merge_requests/7",
                Some("a/b/c"),
            ),
            ("http://host:8080/team/api/-/issues/42", Some("team/api")),
            // A project may itself be called `issues`.
            ("https://host/team/issues/-/issues/1", Some("team/issues")),
            // GitLab before 12 had no `/-/`; it is not supported.
            ("https://host/team/api/issues/42", None),
            ("https://host/team/api", None),
            ("", None),
        ] {
            assert_eq!(project_path(url), path, "{url:?}");
        }
    }

    #[test]
    fn project_falls_back_to_the_id() {
        let named = testing::item(RefKind::Mr, 7, "team/api", 3, "t");
        assert_eq!(named.project(), "team/api");
        let bare = testing::item(RefKind::Issue, 7, "", 3, "t");
        assert_eq!(bare.project(), "project 7");
    }
}
