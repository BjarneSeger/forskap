//! Resolve the project of an issue or merge request without bothering the
//! user.
//!
//! An explicit `--project` wins: a number is the project ID, anything else a
//! full path looked up in the cached projects. Without it:
//! 1. The cached `last_issue` in [`crate::state`] if its kind and IID match —
//!    covers re-acting on the item time was last logged on.
//! 2. The assigned list from the daemon, if it contains exactly one match.
//! 3. The daemon's search corpus, exact-filtered on the iid, for items that
//!    aren't assigned to you.
//! 4. Bail with an explanation. We refuse to guess across ambiguous matches
//!    because picking the wrong project would silently act on someone else's
//!    item.

use anyhow::{Result, bail};
use forskap_api::{Project, VarlinkClient, VarlinkClientInterface};

use crate::friendly::friendly;
use crate::refspec::{self, RefKind};
use crate::state;

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

async fn by_arg(client: &VarlinkClient, project: &str) -> Result<i64> {
    if let Ok(id) = project.parse() {
        return Ok(id);
    }
    let path = project.trim_matches('/');
    let reply = client
        .search(
            path.to_string(),
            Some(vec!["projects".to_string()]),
            Some(SEARCH_LIMIT),
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
    let noun = refspec::noun(kind);

    let assigned: Vec<i64> = match kind {
        RefKind::Issue => client
            .get_assigned_issues(None)
            .call()
            .await
            .map_err(|e| friendly("GetAssignedIssues", e))?
            .issues
            .iter()
            .filter(|i| i.iid == iid)
            .map(|i| i.project_id)
            .collect(),
        RefKind::Mr => client
            .get_assigned_merge_requests(None)
            .call()
            .await
            .map_err(|e| friendly("GetAssignedMergeRequests", e))?
            .merge_requests
            .iter()
            .filter(|m| m.iid == iid)
            .map(|m| m.project_id)
            .collect(),
    };
    match assigned.as_slice() {
        [only] => return Ok(*only),
        [] => {}
        many => bail!(
            "{noun} {iid} is ambiguous across {} assigned projects — pass --project",
            many.len()
        ),
    }

    let reply = client
        .search(
            format!("#{iid}"),
            Some(vec![refspec::search_kind(kind).to_string()]),
            Some(SEARCH_LIMIT),
        )
        .call()
        .await
        .map_err(|e| friendly("Search", e))?;
    let matches: Vec<i64> = match kind {
        RefKind::Issue => reply
            .issues
            .iter()
            .filter(|i| i.iid == iid)
            .map(|i| i.project_id)
            .collect(),
        RefKind::Mr => reply
            .merge_requests
            .iter()
            .filter(|m| m.iid == iid)
            .map(|m| m.project_id)
            .collect(),
    };
    match matches.as_slice() {
        [only] => Ok(*only),
        [] => bail!("no known {noun} with the number {iid} — pass --project"),
        many => bail!(
            "{noun} {iid} is ambiguous across {} projects — pass --project",
            many.len()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(id: i64, path: &str) -> Project {
        Project {
            id,
            name: String::new(),
            path: path.to_string(),
            web_url: String::new(),
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
