//! `forskap epic` — view and open epics; `forskap issue create --epic` names
//! one the same way.
//!
//! Epics are the work items of the type `epic`. They belong to a group and
//! are numbered within it, so an epic is addressed by `(group_id, iid)`. An
//! explicit `--group` wins: a number is the group ID, anything else a full
//! path looked up in the cached groups.
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
use forskap_api::{
    Group, SearchKind, SearchOptions, VarlinkClient, VarlinkClientInterface, WorkItem,
};

use crate::cli::{EpicArgs, EpicCommand};
use crate::friendly::friendly;
use crate::item::{is_epic, project_path};
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
async fn locate(target: &EpicArgs) -> Result<(VarlinkClient, WorkItem)> {
    let client = client::connect_default().await?;
    let epic = resolve(&client, target.iid, target.group.as_deref()).await?;
    Ok((client, epic))
}

/// The cached epic numbered `iid`: the one in `group` (a numeric ID or a
/// full path) if given, else found as the module docs describe.
pub async fn resolve(client: &VarlinkClient, iid: i64, group: Option<&str>) -> Result<WorkItem> {
    if let Some(group) = group {
        let group = group_id(client, group).await?;
        return lookup(client, group, iid).await;
    }
    let last = state::load()
        .ok()
        .and_then(|st| st.last_epic)
        .filter(|last| last.iid == iid)
        .map(|last| last.group_id);
    let found = match matches(cached(client, iid).await?, iid, last, pick::interactive())? {
        Matches::Only(epic) => return Ok(*epic),
        Matches::Ask(found) => found,
    };
    let message = format!("&{iid} exists in {} groups — which one?", found.len());
    let picked = tokio::task::spawn_blocking(move || pick::select(&message, pick::by_group(found)))
        .await??;
    picked.ok_or_else(|| pick::Cancelled.into())
}

/// The group path the epic carries, else the one in its URL.
pub fn group_of(e: &WorkItem) -> Option<&str> {
    e.namespace_path
        .as_deref()
        .filter(|p| !p.is_empty())
        .or_else(|| group_path(&e.web_url))
}

/// The group path inside an epic URL (`https://host/groups/<path>/-/epics/<iid>`).
fn group_path(web_url: &str) -> Option<&str> {
    project_path(web_url)?.strip_prefix("groups/")
}

/// The cached epics numbered `iid`, one per group that has one.
async fn cached(client: &VarlinkClient, iid: i64) -> Result<Vec<WorkItem>> {
    let options = SearchOptions {
        kinds: Some(vec![SearchKind::work_items]),
        limit: Some(SEARCH_LIMIT),
        types: Some(vec!["epic".into()]),
        ..Default::default()
    };
    let reply = client
        .search(format!("&{iid}"), Some(options))
        .call()
        .await
        .map_err(|e| friendly("Search", e))?;
    let epics = reply.work_items.into_iter().filter(is_epic);
    Ok(epics.filter(|e| e.iid == iid).collect())
}

/// The epic of one known group, for callers that hold the id pair already.
pub async fn lookup(client: &VarlinkClient, group_id: i64, iid: i64) -> Result<WorkItem> {
    in_group(cached(client, iid).await?, iid, group_id)
}

/// The one of the cached epics numbered `iid` that is in `group`.
fn in_group(mut cached: Vec<WorkItem>, iid: i64, group: i64) -> Result<WorkItem> {
    match cached.iter().position(|e| e.group_id == Some(group)) {
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
    Only(Box<WorkItem>),
    /// Several, and a terminal to ask on.
    Ask(Vec<WorkItem>),
}

/// Choose among the cached epics numbered `iid` when no group was asked for:
/// the one in the `last` opened group, else the only one there is. Several
/// are only worth asking about when `interactive`: scripts and launchers
/// keep getting the error.
fn matches(
    mut cached: Vec<WorkItem>,
    iid: i64,
    last: Option<i64>,
    interactive: bool,
) -> Result<Matches> {
    let of_last = |e: &WorkItem| last.is_some() && e.group_id == last;
    if let Some(at) = cached.iter().position(of_last) {
        return Ok(Matches::Only(Box::new(cached.swap_remove(at))));
    }
    Ok(match cached.len() {
        1 => Matches::Only(Box::new(cached.swap_remove(0))),
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
    let options = SearchOptions {
        kinds: Some(vec![SearchKind::groups]),
        limit: Some(SEARCH_LIMIT),
        ..Default::default()
    };
    let reply = client
        .search(path.to_string(), Some(options))
        .call()
        .await
        .map_err(|e| friendly("Search", e))?;
    by_path(path, &reply.groups)
}

/// GitLab paths are case-insensitive.
fn by_path(path: &str, groups: &[Group]) -> Result<i64> {
    match groups
        .iter()
        .find(|g| g.full_path.eq_ignore_ascii_case(path))
    {
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

    use crate::item::testing::epic;

    /// The group `matches` settled on without asking.
    fn only(cached: Vec<WorkItem>, last: Option<i64>, interactive: bool) -> Result<i64> {
        match matches(cached, 5, last, interactive)? {
            Matches::Only(epic) => Ok(epic.group_id.unwrap()),
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
        assert_eq!(in_group(both(), 5, 4).unwrap().group_id, Some(4));
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
    fn the_carried_group_path_beats_the_url() {
        let mut e = epic(3, 5);
        assert_eq!(group_of(&e), Some("g3"));
        e.namespace_path = Some("team/backend".into());
        assert_eq!(group_of(&e), Some("team/backend"));
        e.web_url = String::new();
        assert_eq!(group_of(&e), Some("team/backend"));
        e.namespace_path = None;
        assert_eq!(group_of(&e), None);
    }

    #[test]
    fn group_paths_match_in_full() {
        let group = |id, path: &str| Group {
            id,
            name: String::new(),
            full_path: path.to_string(),
            web_url: String::new(),
        };
        // The daemon matches substrings: `team` also returns `team/backend`.
        let found = [group(1, "team/backend"), group(2, "Team")];
        assert_eq!(by_path("team", &found).unwrap(), 2);
        assert!(by_path("backend", &found).is_err());
    }
}
