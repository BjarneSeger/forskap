//! Resolve the project of an issue or merge request without bothering the
//! user.
//!
//! An explicit `--project` wins: a number is the project ID, anything else a
//! full path looked up in the cached projects. Without it:
//! 1. The cached `last_issue` in [`crate::state`] if its kind and IID match —
//!    covers re-acting on the item time was last logged on.
//! 2. The assigned list from the daemon, if it contains a match.
//! 3. The daemon's search corpus, exact-filtered on the iid, for items that
//!    aren't assigned to you.
//!
//! Several matches in one of these are never guessed across — picking the
//! wrong project would silently act on someone else's item. On a terminal the
//! user picks the one they meant ([`crate::pick`]); anywhere else that is an
//! error asking for `--project`.

use anyhow::{Result, bail};
use forskap_api::{Project, SearchKind, VarlinkClient, VarlinkClientInterface};

use crate::friendly::friendly;
use crate::item::{self, Item};
use crate::refspec::{self, RefKind};
use crate::{pick, state};

/// Generous per-kind cap for the corpus lookups: the daemon matches
/// substrings (and `#42` against titles too), so we over-fetch and
/// exact-filter client-side; the default 50 could truncate before the filter.
const SEARCH_LIMIT: i64 = 500;

pub async fn resolve(
    client: &VarlinkClient,
    kind: RefKind,
    iid: i64,
    project: Option<&str>,
) -> Result<i64> {
    match project {
        Some(p) => by_arg(client, p).await,
        None => by_iid(client, kind, iid).await,
    }
}

/// The project ID behind a `--project` value: a number as is, a path looked
/// up in the cached projects.
pub async fn by_arg(client: &VarlinkClient, project: &str) -> Result<i64> {
    if let Ok(id) = project.parse() {
        return Ok(id);
    }
    let path = project.trim_matches('/');
    let reply = client
        .search(
            path.to_string(),
            Some(vec![SearchKind::projects]),
            Some(SEARCH_LIMIT),
            None,
            None,
            None,
        )
        .call()
        .await
        .map_err(|e| friendly("Search", e))?;
    by_path(path, &reply.projects)
}

/// GitLab paths are case-insensitive.
fn by_path(path: &str, projects: &[Project]) -> Result<i64> {
    match projects.iter().find(|p| p.path.eq_ignore_ascii_case(path)) {
        Some(p) => Ok(p.id),
        None => bail!(
            "no cached project with the path {path:?} — pass the full path \
             (`forskap search --kind projects`) or the numeric ID. After a refresh \
             or a first login the cache may still be filling; the numeric ID \
             works meanwhile."
        ),
    }
}

async fn by_iid(client: &VarlinkClient, kind: RefKind, iid: i64) -> Result<i64> {
    if let Ok(st) = state::load()
        && let Some(last) = st.last_issue
        && last.kind == kind
        && last.issue_iid == iid
    {
        return Ok(last.project_id);
    }

    let assigned: Vec<Item> = match kind {
        RefKind::Issue => client
            .get_assigned_work_items(None)
            .call()
            .await
            .map_err(|e| friendly("GetAssignedWorkItems", e))?
            .work_items
            .into_iter()
            .map(Item::Issue)
            .collect(),
        RefKind::Mr => client
            .get_assigned_merge_requests(None)
            .call()
            .await
            .map_err(|e| friendly("GetAssignedMergeRequests", e))?
            .merge_requests
            .into_iter()
            .map(Item::Mr)
            .collect(),
    };
    if let Some(project_id) = settle(kind, iid, assigned, "assigned projects").await? {
        return Ok(project_id);
    }

    let reply = client
        .search(
            format!("#{iid}"),
            Some(vec![refspec::search_kind(kind)]),
            Some(SEARCH_LIMIT),
            None,
            None,
            refspec::excluded_types(kind),
        )
        .call()
        .await
        .map_err(|e| friendly("Search", e))?;
    let corpus: Vec<Item> = match kind {
        RefKind::Issue => item::issues(reply.work_items).collect(),
        RefKind::Mr => reply.merge_requests.into_iter().map(Item::Mr).collect(),
    };
    match settle(kind, iid, corpus, "projects").await? {
        Some(project_id) => Ok(project_id),
        None => bail!(
            "no known {} with the number {iid} — pass --project",
            refspec::noun(kind)
        ),
    }
}

/// What the rows carrying the wanted number leave to do.
enum Matches {
    None,
    Only(i64),
    /// Several, and a terminal to ask on.
    Ask(Vec<Item>),
}

/// Exact-filter `rows` on the iid. Several matches are only worth asking
/// about when `interactive`: scripts and launchers keep getting the error.
/// `scope` names in it where the rows came from.
fn matches(
    kind: RefKind,
    iid: i64,
    rows: Vec<Item>,
    scope: &str,
    interactive: bool,
) -> Result<Matches> {
    let mut found: Vec<Item> = rows.into_iter().filter(|i| i.iid() == iid).collect();
    Ok(match found.len() {
        0 => Matches::None,
        1 => Matches::Only(found.remove(0).project_id()),
        _ if interactive => Matches::Ask(found),
        n => bail!(
            "{} {iid} is ambiguous across {n} {scope} — pass --project",
            refspec::noun(kind)
        ),
    })
}

/// The project of the one row with the wanted number — the user's pick if
/// there are several — or `None` without any.
async fn settle(kind: RefKind, iid: i64, rows: Vec<Item>, scope: &str) -> Result<Option<i64>> {
    let found = match matches(kind, iid, rows, scope, pick::interactive())? {
        Matches::None => return Ok(None),
        Matches::Only(project_id) => return Ok(Some(project_id)),
        Matches::Ask(found) => found,
    };
    let message = format!(
        "{}{iid} exists in {} {scope} — which one?",
        refspec::sigil(kind),
        found.len()
    );
    let picked =
        tokio::task::spawn_blocking(move || pick::select(&message, pick::by_project(found)))
            .await??;
    match picked {
        Some(item) => Ok(Some(item.project_id())),
        None => Err(pick::Cancelled.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::testing::item;

    fn project(id: i64, path: &str) -> Project {
        Project {
            id,
            name: String::new(),
            path: path.to_string(),
            web_url: String::new(),
            avatar: None,
            archived: false,
        }
    }

    #[test]
    fn several_matches_ask_only_on_a_terminal() {
        let rows = || {
            vec![
                item(RefKind::Issue, 1, "team/api", 42, "a"),
                item(RefKind::Issue, 2, "team/web", 42, "b"),
                item(RefKind::Issue, 2, "team/web", 420, "c"),
            ]
        };
        let Matches::Ask(found) = matches(RefKind::Issue, 42, rows(), "projects", true).unwrap()
        else {
            panic!("should ask");
        };
        let projects: Vec<i64> = found.iter().map(Item::project_id).collect();
        assert_eq!(projects, [1, 2]);

        // Scripts match on these texts.
        for (kind, scope, text) in [
            (
                RefKind::Issue,
                "assigned projects",
                "issue 42 is ambiguous across 2 assigned projects — pass --project",
            ),
            (
                RefKind::Mr,
                "projects",
                "merge request 42 is ambiguous across 2 projects — pass --project",
            ),
        ] {
            let Err(e) = matches(kind, 42, rows(), scope, false) else {
                panic!("should refuse");
            };
            assert_eq!(e.to_string(), text);
        }
    }

    #[test]
    fn one_match_needs_no_terminal() {
        let rows = || {
            vec![
                item(RefKind::Mr, 1, "team/api", 7, "a"),
                item(RefKind::Mr, 2, "team/web", 70, "b"),
            ]
        };
        for interactive in [true, false] {
            assert!(matches!(
                matches(RefKind::Mr, 7, rows(), "projects", interactive),
                Ok(Matches::Only(1))
            ));
            assert!(matches!(
                matches(RefKind::Mr, 8, rows(), "projects", interactive),
                Ok(Matches::None)
            ));
        }
    }

    #[test]
    fn by_path_wants_the_full_path() {
        // The daemon matches substrings, so a search for `team/api` also
        // returns `team/api-tests`.
        let found = [project(1, "team/api-tests"), project(2, "Team/API")];
        assert_eq!(by_path("team/api", &found).unwrap(), 2);
        assert!(by_path("api", &found).is_err());
    }
}
