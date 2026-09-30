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
//! 3. Several groups: a picker on a terminal, else bail with an explanation
//!    rather than guess.

mod open;
mod view;

use anyhow::{Result, bail};
use forskap_api::{Epic, Group, VarlinkClient, VarlinkClientInterface};

use crate::cli::{EpicArgs, EpicCommand};
use crate::friendly::friendly;
use crate::item::project_path;
use crate::{client, pick, state};

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
    let iid = target.iid;
    if let Some(group) = target.group.as_deref() {
        let group = group_id(&client, group).await?;
        let epic = in_group(cached(&client, iid).await?, iid, group)?;
        return Ok((client, epic));
    }
    let last = state::load()
        .ok()
        .and_then(|st| st.last_epic)
        .filter(|last| last.iid == iid)
        .map(|last| last.group_id);
    let found = match matches(cached(&client, iid).await?, iid, last, pick::interactive())? {
        Matches::Only(epic) => return Ok((client, epic)),
        Matches::Ask(found) => found,
    };
    let message = format!("&{iid} exists in {} groups — which one?", found.len());
    let picked = tokio::task::spawn_blocking(move || pick::select(&message, pick::by_group(found)))
        .await??;
    match picked {
        Some(epic) => Ok((client, epic)),
        None => Err(pick::Cancelled.into()),
    }
}

/// The group path inside an epic URL (`https://host/groups/<path>/-/epics/<iid>`).
pub fn group_path(web_url: &str) -> Option<&str> {
    project_path(web_url)?.strip_prefix("groups/")
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
    in_group(cached(client, iid).await?, iid, group_id)
}

/// The one of the cached epics numbered `iid` that is in `group`.
fn in_group(mut cached: Vec<Epic>, iid: i64, group: i64) -> Result<Epic> {
    match cached.iter().position(|e| e.group_id == group) {
        Some(at) => Ok(cached.swap_remove(at)),
        None => bail!(
            "&{iid} in group {group} is not in the daemon's caches — epics are synced for \
             the groups above the projects in the search corpus, on GitLab instances that \
             have them; try `forskap sync refresh --scope search`"
        ),
    }
}

/// What the cached epics carrying the wanted number leave to do.
#[derive(Debug)]
enum Matches {
    Only(Epic),
    /// Several, and a terminal to ask on.
    Ask(Vec<Epic>),
}

/// Choose among the cached epics numbered `iid` when no group was asked for:
/// the one in the `last` opened group, else the only one there is. Several
/// are only worth asking about when `interactive`: scripts and launchers
/// keep getting the error.
fn matches(
    mut cached: Vec<Epic>,
    iid: i64,
    last: Option<i64>,
    interactive: bool,
) -> Result<Matches> {
    let of_last = |e: &Epic| Some(e.group_id) == last;
    if let Some(at) = cached.iter().position(of_last) {
        return Ok(Matches::Only(cached.swap_remove(at)));
    }
    Ok(match cached.len() {
        1 => Matches::Only(cached.swap_remove(0)),
        0 => bail!("no known epic with the number {iid} — pass --group"),
        _ if interactive => Matches::Ask(cached),
        many => bail!("epic {iid} is ambiguous across {many} groups — pass --group"),
    })
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

    /// The group `matches` settled on without asking.
    fn only(cached: Vec<Epic>, last: Option<i64>, interactive: bool) -> Result<i64> {
        match matches(cached, 5, last, interactive)? {
            Matches::Only(epic) => Ok(epic.group_id),
            Matches::Ask(found) => bail!("asks about {}", found.len()),
        }
    }

    #[test]
    fn a_single_match_needs_no_group() {
        for interactive in [true, false] {
            assert_eq!(only(vec![epic(3, 5)], None, interactive).unwrap(), 3);
        }
    }

    #[test]
    fn several_groups_want_the_flag_or_a_pick() {
        let both = || vec![epic(3, 5), epic(4, 5)];
        let err = only(both(), None, false).unwrap_err().to_string();
        assert!(err.contains("ambiguous across 2 groups"), "{err}");
        assert!(err.contains("--group"), "{err}");
        assert!(matches!(
            matches(both(), 5, None, true),
            Ok(Matches::Ask(found)) if found.len() == 2
        ));
        assert_eq!(in_group(both(), 5, 4).unwrap().group_id, 4);
    }

    #[test]
    fn the_last_opened_group_breaks_the_tie() {
        let both = || vec![epic(3, 5), epic(4, 5)];
        for interactive in [true, false] {
            assert_eq!(only(both(), Some(4), interactive).unwrap(), 4);
        }
        // A last group without such an epic (any more) doesn't count.
        assert!(only(both(), Some(9), false).is_err());
    }

    #[test]
    fn an_unknown_epic_is_an_error() {
        for interactive in [true, false] {
            let err = only(Vec::new(), None, interactive).unwrap_err().to_string();
            assert!(err.contains("no known epic"), "{err}");
        }
        let err = in_group(vec![epic(3, 5)], 5, 4).unwrap_err();
        assert!(err.to_string().contains("&5 in group 4"), "{err}");
    }

    #[test]
    fn group_paths_come_from_the_epic_url() {
        let url = "https://gl/groups/team/backend/-/epics/5";
        assert_eq!(group_path(url), Some("team/backend"));
        assert_eq!(group_path("https://gl/team/api/-/issues/5"), None);
        assert_eq!(group_path(""), None);
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
