//! `forskap issue` / `forskap mr` — the same verbs for both kinds; the command group
//! picks the [`RefKind`].

mod assign;
mod close;
mod list;
mod open;
mod unassign;
mod view;

use anyhow::{Result, bail};
use forskap_api::{VarlinkClient, VarlinkClientInterface};

use crate::cli::{ItemCommand, TargetArgs};
use crate::client;
use crate::cmd::project;
use crate::friendly::friendly;
use crate::item::Item;
use crate::refspec::{self, RefKind};

/// Per-kind cap for the `#iid` lookup: titles can match the number too, so
/// the default 50 could truncate before the exact filter.
const LOOKUP_LIMIT: i64 = 500;

pub async fn run(kind: RefKind, command: ItemCommand) -> Result<()> {
    match command {
        ItemCommand::List { groups, output } => list::run(kind, groups, output.output).await,
        ItemCommand::View { target, output } => view::run(kind, target, output.output).await,
        ItemCommand::Open { target, no_browser } => open::run(kind, target, no_browser).await,
        ItemCommand::Close { target } => close::run(kind, target).await,
        ItemCommand::Assign { target } => assign::run(kind, target).await,
        ItemCommand::Unassign { target } => unassign::run(kind, target).await,
    }
}

/// Connect, and pin down the project the target lives in.
async fn locate(kind: RefKind, target: &TargetArgs) -> Result<(VarlinkClient, i64)> {
    let client = client::connect_default().await?;
    let project_id =
        project::resolve(&client, kind, target.iid, target.project.project.as_deref()).await?;
    Ok((client, project_id))
}

/// The row from the daemon's caches: the search corpus first, then the
/// assigned list, which is cached even when the corpus (member projects only,
/// or still syncing) doesn't cover the project.
pub async fn lookup(
    client: &VarlinkClient,
    kind: RefKind,
    project_id: i64,
    iid: i64,
) -> Result<Item> {
    let reply = client
        .search(
            format!("#{iid}"),
            Some(vec![refspec::search_kind(kind)]),
            Some(LOOKUP_LIMIT),
        )
        .call()
        .await
        .map_err(|e| friendly("Search", e))?;
    let from_corpus = match kind {
        RefKind::Issue => reply
            .issues
            .into_iter()
            .find(|i| i.project_id == project_id && i.iid == iid)
            .map(Item::Issue),
        RefKind::Mr => reply
            .merge_requests
            .into_iter()
            .find(|m| m.project_id == project_id && m.iid == iid)
            .map(Item::Mr),
    };
    if let Some(item) = from_corpus.filter(|i| !i.web_url().is_empty()) {
        return Ok(item);
    }

    let from_assigned = match kind {
        RefKind::Issue => client
            .get_assigned_issues(None)
            .call()
            .await
            .map_err(|e| friendly("GetAssignedIssues", e))?
            .issues
            .into_iter()
            .find(|i| i.project_id == project_id && i.iid == iid)
            .map(Item::Issue),
        RefKind::Mr => client
            .get_assigned_merge_requests(None)
            .call()
            .await
            .map_err(|e| friendly("GetAssignedMergeRequests", e))?
            .merge_requests
            .into_iter()
            .find(|m| m.project_id == project_id && m.iid == iid)
            .map(Item::Mr),
    };
    if let Some(item) = from_assigned.filter(|i| !i.web_url().is_empty()) {
        return Ok(item);
    }

    bail!(
        "{}{iid} in project {project_id} is not in the daemon's caches yet — try `forskap sync refresh --scope search`",
        refspec::sigil(kind)
    )
}
