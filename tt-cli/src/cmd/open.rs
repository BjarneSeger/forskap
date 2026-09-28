//! `tt open <ref>` — open an issue (`42`, `#42`) or merge request (`!42`) in
//! the browser and count the open in the daemon, so `tt search` (and the
//! launcher integrations built on it) rank it higher next time.
//!
//! The URL comes from the daemon's caches: the search corpus first, the
//! assigned-issue list as a fallback for issues the corpus doesn't cover.
//! Project ID resolves through [`crate::cmd::project`] when not supplied.

use anyhow::{Result, bail};
use gitlab_trackr_api::VarlinkClientInterface;

use crate::cmd::project;
use crate::refspec::RefKind;
use crate::{client, config, refspec};

/// Per-kind cap for the `#iid` lookup — same reasoning as
/// `project::SEARCH_FALLBACK_LIMIT`: titles can match the number too, so the
/// default 50 could truncate before the exact filter.
const LOOKUP_LIMIT: i64 = 500;

pub async fn run(
    issuable: &str,
    mr: bool,
    project_id: Option<i64>,
    no_browser: bool,
) -> Result<()> {
    let reference = refspec::parse(issuable)?;
    let kind = refspec::resolve_kind(reference, mr)?;
    let iid = reference.iid;
    let cfg = config::load()?;
    let socket = cfg.socket.unwrap_or_else(client::default_socket);

    let project_id = match project_id {
        Some(p) => p,
        None => project::resolve(iid, kind, &socket).await?,
    };

    let client = client::connect(&socket).await?;
    let web_url = lookup_url(&client, kind, project_id, iid).await?;

    client
        .record_open(project_id, iid, refspec::wire(kind))
        .call()
        .await
        .map_err(|e| crate::friendly::friendly("RecordOpen", e))?;

    if !no_browser {
        // Detached: the caller (a shell, a launcher plugin) must not block on
        // the browser process.
        if let Err(e) = open::that_detached(&web_url) {
            eprintln!("(couldn't open browser automatically: {e})");
        }
    }
    println!("opened {}{iid} {web_url}", refspec::sigil(kind));
    Ok(())
}

/// The issuable's `web_url` from the daemon's caches, or an error naming the
/// ref when neither cache knows it.
async fn lookup_url(
    client: &gitlab_trackr_api::VarlinkClient,
    kind: RefKind,
    project_id: i64,
    iid: i64,
) -> Result<String> {
    let wire_kind = match kind {
        RefKind::Issue => "issues",
        RefKind::Mr => "merge_requests",
    };
    let reply = client
        .search(
            format!("#{iid}"),
            Some(vec![wire_kind.to_string()]),
            Some(LOOKUP_LIMIT),
        )
        .call()
        .await
        .map_err(|e| crate::friendly::friendly("Search", e))?;
    let from_corpus = match kind {
        RefKind::Issue => reply
            .issues
            .iter()
            .find(|i| i.project_id == project_id && i.iid == iid)
            .map(|i| i.web_url.clone()),
        RefKind::Mr => reply
            .merge_requests
            .iter()
            .find(|m| m.project_id == project_id && m.iid == iid)
            .map(|m| m.web_url.clone()),
    };
    if let Some(url) = from_corpus.filter(|u| !u.is_empty()) {
        return Ok(url);
    }

    // Issues assigned to you are cached even when the search population
    // (member-only, or a corpus still syncing) doesn't cover their project.
    if kind == RefKind::Issue {
        let reply = client
            .get_assigned_issues(None)
            .call()
            .await
            .map_err(|e| crate::friendly::friendly("GetAssignedIssues", e))?;
        if let Some(i) = reply
            .issues
            .iter()
            .find(|i| i.project_id == project_id && i.iid == iid)
            .filter(|i| !i.web_url.is_empty())
        {
            return Ok(i.web_url.clone());
        }
    }

    bail!(
        "{}{iid} in project {project_id} is not in the daemon's caches yet — try `tt refresh --search`",
        refspec::sigil(kind)
    )
}
