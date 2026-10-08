//! `forskap issue` / `forskap mr` — the same verbs for both kinds; the command group
//! picks the [`RefKind`]. Only issues are created (`forskap issue create`).

mod assign;
mod close;
pub mod create;
mod list;
mod open;
mod unassign;
mod view;

use anyhow::{Result, bail};
use forskap_api::{Scope, SearchOptions, VarlinkClient, VarlinkClientInterface};

use crate::cli::{ItemCommand, TargetArgs};
use crate::cmd::{project, queue};
use crate::friendly::friendly;
use crate::item::{self, Item};
use crate::refspec::{self, RefKind};
use crate::{client, style};

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

/// What a write to `iid` came to: `done` ("closed"), or, where the daemon
/// only `queued` it, `doing` ("closing") under way and a line saying so. A
/// daemon too old to say reads as done, as it always did.
fn said(
    queued: Option<bool>,
    done: &str,
    doing: &str,
    kind: RefKind,
    iid: i64,
    project_id: i64,
) -> Result<()> {
    out!("{}", said_text(queued, done, doing, kind, iid, project_id))
}

fn said_text(
    queued: Option<bool>,
    done: &str,
    doing: &str,
    kind: RefKind,
    iid: i64,
    project_id: i64,
) -> String {
    let on = style::reference(refspec::sigil(kind), iid);
    let project = format!("(project {project_id})");
    let project = style::muted(&project);
    match queued {
        Some(true) => format!(
            "{} {doing} {on} {project}\n{}\n",
            style::state("queued"),
            style::muted(queue::NOT_SENT)
        ),
        _ => format!("{} {on} {project}\n", style::success(done)),
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
    // Scoped to the project: a title can contain `#iid` too, so exact-filter.
    // No epic passes a project scope, so none needs excluding.
    let options = SearchOptions {
        kinds: Some(vec![refspec::search_kind(kind)]),
        scope: Some(Scope {
            projects: Some(vec![project_id]),
            groups: None,
        }),
        ..Default::default()
    };
    let reply = client
        .search(format!("#{iid}"), Some(options))
        .call()
        .await
        .map_err(|e| friendly("Search", e))?;
    let from_corpus = match kind {
        RefKind::Issue => item::issues(reply.work_items).find(|i| i.iid() == iid),
        RefKind::Mr => reply
            .merge_requests
            .into_iter()
            .find(|m| m.iid == iid)
            .map(Item::Mr),
    };
    if let Some(item) = from_corpus.filter(|i| !i.web_url().is_empty()) {
        return Ok(item);
    }

    let from_assigned = match kind {
        RefKind::Issue => client
            .get_assigned_work_items(None)
            .call()
            .await
            .map_err(|e| friendly("GetAssignedWorkItems", e))?
            .work_items
            .into_iter()
            .find(|i| i.project_id == Some(project_id) && i.iid == iid)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A write the daemon only queued doesn't claim to be done: it says
    /// what is under way, and that it isn't sent yet. Applied, or from a
    /// daemon too old to tell, it reads as it always did.
    #[test]
    fn a_queued_write_says_so_instead_of_done() {
        for applied in [None, Some(false)] {
            assert_eq!(
                said_text(applied, "closed", "closing", RefKind::Issue, 42, 7),
                "closed #42 (project 7)\n"
            );
        }
        assert_eq!(
            said_text(Some(true), "closed", "closing", RefKind::Mr, 42, 7),
            "queued closing !42 (project 7)\n\
             not sent yet: the daemon sends it once it reaches GitLab (`forskap queue list`)\n"
        );
        assert_eq!(
            said_text(
                Some(true),
                "assigned to",
                "assigning you to",
                RefKind::Issue,
                3,
                7
            ),
            format!(
                "queued assigning you to #3 (project 7)\n{}\n",
                queue::NOT_SENT
            )
        );
    }
}
