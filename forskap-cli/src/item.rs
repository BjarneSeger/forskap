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

    /// The project's full path, if known.
    pub fn project_path(&self) -> Option<&str> {
        let path = match self {
            Item::Issue(i) => &i.project_path,
            Item::Mr(m) => &m.project_path,
        };
        project_of(path, self.web_url())
    }

    /// The project's full path, else `project <id>`.
    pub fn project(&self) -> String {
        match self.project_path() {
            Some(path) => path.to_string(),
            None => format!("project {}", self.project_id()),
        }
    }
}

/// The project path a row carries, else the one in its URL.
pub fn project_of<'a>(project_path: &'a str, web_url: &'a str) -> Option<&'a str> {
    Some(project_path)
        .filter(|p| !p.is_empty())
        .or_else(|| self::project_path(web_url))
}

/// The project path inside an issue or MR URL
/// (`https://host/<path>/-/issues/<iid>`).
pub fn project_path(web_url: &str) -> Option<&str> {
    let (_, rest) = web_url.split_once("://")?;
    let (_, path) = rest.split_once('/')?;
    // GitLab before 12 had no `/-/` in front of the resource.
    let end = path.find("/-/").or_else(|| {
        ["/issues/", "/merge_requests/"]
            .iter()
            .find_map(|resource| path.rfind(resource))
    })?;
    Some(&path[..end]).filter(|p| !p.is_empty())
}

#[cfg(test)]
pub mod testing {
    use super::*;

    /// A row in `project_id` at `path`; an empty `path` leaves it and the
    /// URL out.
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
                project_path: path.to_string(),
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
                project_path: path.to_string(),
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
            ("http://host:8080/team/api/issues/42", Some("team/api")),
            // A project may itself be called `issues`.
            ("https://host/team/issues/-/issues/1", Some("team/issues")),
            ("https://host/team/issues/issues/1", Some("team/issues")),
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

    #[test]
    fn project_of_prefers_the_field_over_the_url() {
        let url = "https://gitlab.com/old/name/-/issues/1";
        assert_eq!(project_of("team/api", url), Some("team/api"));
        assert_eq!(project_of("", url), Some("old/name"));
        assert_eq!(project_of("", ""), None);
    }
}
