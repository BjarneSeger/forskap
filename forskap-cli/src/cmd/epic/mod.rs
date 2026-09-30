//! `forskap epic` — view and open epics.
//!
//! Epics belong to a group and are numbered within it, so an epic is
//! addressed by `(group_id, iid)`. An explicit `--group` wins: a number is
//! the group ID, anything else a full path looked up in the cached groups.
//! Without it:
//! 1. The cached `last_epic` in [`crate::state`] if its IID matches — covers
//!    re-acting on the epic opened last.
//! 2. The daemon's search corpus, exact-filtered on the iid, if exactly one
//!    group has such an epic.
//! 3. Bail with an explanation rather than guess across groups.

mod open;
mod view;

use anyhow::{Result, bail};
use forskap_api::{Epic, Group, VarlinkClient, VarlinkClientInterface};

use crate::cli::{EpicArgs, EpicCommand};
use crate::friendly::friendly;
use crate::{client, state};

/// Generous cap for the corpus lookups: the daemon matches substrings (and
/// `&5` against titles too), so we over-fetch and exact-filter client-side;
/// the default 50 could truncate before the filter.
const SEARCH_LIMIT: i64 = 500;

pub async fn run(command: EpicCommand) -> Result<()> {
    match command {
        EpicCommand::View { target, output } => view::run(target, output.output).await,
        EpicCommand::Open { target, no_browser } => open::run(target, no_browser).await,
    }
}

/// Connect, and find the cached epic the target names.
async fn locate(target: &EpicArgs) -> Result<(VarlinkClient, Epic)> {
    let client = client::connect_default().await?;
    let group = match target.group.as_deref() {
        Some(g) => Some(group_id(&client, g).await?),
        None => None,
    };
    let last = state::load()
        .ok()
        .and_then(|st| st.last_epic)
        .filter(|last| last.iid == target.iid)
        .map(|last| last.group_id);
    let cached = cached(&client, target.iid).await?;
    let epic = pick(cached, target.iid, group, last)?;
    Ok((client, epic))
}

/// The cached epics numbered `iid`, one per group that has one.
async fn cached(client: &VarlinkClient, iid: i64) -> Result<Vec<Epic>> {
    let reply = client
        .search(
            format!("&{iid}"),
            Some(vec!["epics".to_string()]),
            Some(SEARCH_LIMIT),
        )
        .call()
        .await
        .map_err(|e| friendly("Search", e))?;
    Ok(reply.epics.into_iter().filter(|e| e.iid == iid).collect())
}

/// The epic of one known group, for callers that hold the id pair already.
pub async fn lookup(client: &VarlinkClient, group_id: i64, iid: i64) -> Result<Epic> {
    pick(cached(client, iid).await?, iid, Some(group_id), None)
}

/// Choose among the cached epics numbered `iid`: the one in `group` when a
/// group was asked for, else the one in the `last` opened group, else the
/// only one there is.
fn pick(mut cached: Vec<Epic>, iid: i64, group: Option<i64>, last: Option<i64>) -> Result<Epic> {
    let of = |cached: &[Epic], group: i64| cached.iter().position(|e| e.group_id == group);
    if let Some(group) = group {
        return match of(&cached, group) {
            Some(at) => Ok(cached.swap_remove(at)),
            None => bail!(
                "&{iid} in group {group} is not in the daemon's caches — epics are synced for \
                 the groups above the projects in the search corpus, on GitLab instances that \
                 have them; try `forskap sync refresh --scope search`"
            ),
        };
    }
    if let Some(at) = last.and_then(|group| of(&cached, group)) {
        return Ok(cached.swap_remove(at));
    }
    match cached.len() {
        1 => Ok(cached.swap_remove(0)),
        0 => bail!("no known epic with the number {iid} — pass --group"),
        many => bail!("epic {iid} is ambiguous across {many} groups — pass --group"),
    }
}

async fn group_id(client: &VarlinkClient, group: &str) -> Result<i64> {
    if let Ok(id) = group.parse() {
        return Ok(id);
    }
    let path = group.trim_matches('/');
    let reply = client
        .search(
            path.to_string(),
            Some(vec!["groups".to_string()]),
            Some(SEARCH_LIMIT),
        )
        .call()
        .await
        .map_err(|e| friendly("Search", e))?;
    by_path(path, &reply.groups)
}

/// GitLab paths are case-insensitive.
fn by_path(path: &str, groups: &[Group]) -> Result<i64> {
    match groups.iter().find(|g| g.path.eq_ignore_ascii_case(path)) {
        Some(g) => Ok(g.id),
        None => bail!(
            "no cached group with the path {path:?} — pass the full path \
             (`forskap search --kind groups`) or the numeric ID"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn epic(group_id: i64, iid: i64) -> Epic {
        Epic {
            id: group_id * 100 + iid,
            iid,
            group_id,
            title: String::new(),
            web_url: format!("https://gl/groups/g{group_id}/-/epics/{iid}"),
            state: "opened".into(),
            open_count: 0,
        }
    }

    #[test]
    fn a_single_match_needs_no_group() {
        assert_eq!(pick(vec![epic(3, 5)], 5, None, None).unwrap().group_id, 3);
    }

    #[test]
    fn several_groups_want_the_flag() {
        let both = || vec![epic(3, 5), epic(4, 5)];
        let err = pick(both(), 5, None, None).unwrap_err().to_string();
        assert!(err.contains("ambiguous across 2 groups"), "{err}");
        assert!(err.contains("--group"), "{err}");
        assert_eq!(pick(both(), 5, Some(4), None).unwrap().group_id, 4);
    }

    #[test]
    fn the_last_opened_group_breaks_the_tie() {
        let both = || vec![epic(3, 5), epic(4, 5)];
        assert_eq!(pick(both(), 5, None, Some(4)).unwrap().group_id, 4);
        // An explicit group still wins, and a last group without such an
        // epic (any more) doesn't count.
        assert_eq!(pick(both(), 5, Some(3), Some(4)).unwrap().group_id, 3);
        assert!(pick(both(), 5, None, Some(9)).is_err());
    }

    #[test]
    fn an_unknown_epic_is_an_error() {
        let err = pick(Vec::new(), 5, None, None).unwrap_err().to_string();
        assert!(err.contains("no known epic"), "{err}");
        let err = pick(vec![epic(3, 5)], 5, Some(4), None).unwrap_err();
        assert!(err.to_string().contains("&5 in group 4"), "{err}");
    }

    #[test]
    fn group_paths_match_in_full() {
        let group = |id, path: &str| Group {
            id,
            name: String::new(),
            path: path.to_string(),
            web_url: String::new(),
        };
        // The daemon matches substrings: `team` also returns `team/backend`.
        let found = [group(1, "team/backend"), group(2, "Team")];
        assert_eq!(by_path("team", &found).unwrap(), 2);
        assert!(by_path("backend", &found).is_err());
    }
}
