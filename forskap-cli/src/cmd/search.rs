//! `forskap search` — query the daemon's cached search corpus.
//!
//! Pure cache read: the daemon's background search sync owns freshness
//! (incremental `updated_after` pulls, a periodic full resync), so this just
//! serves whatever was last synced — no fetch, effectively free.

use anyhow::Result;
use forskap_api::{SearchKind as WireKind, SearchScope, VarlinkClientInterface};

use crate::cli::{OutputFormat, SearchKind};
use crate::cmd::project;
use crate::friendly::friendly;
use crate::{client, output, style};

pub async fn run(
    query: Vec<String>,
    kinds: Vec<SearchKind>,
    limit: Option<i64>,
    project: Option<String>,
    groups: Vec<String>,
    format: OutputFormat,
) -> Result<()> {
    let client = client::connect_default().await?;
    let query = query.join(" ");
    let filter = (!kinds.is_empty()).then(|| kinds.iter().map(|k| wire_kind(*k)).collect());
    let projects = match project {
        Some(p) => Some(vec![project::by_arg(&client, &p).await?]),
        None => None,
    };
    let scope = (projects.is_some() || !groups.is_empty()).then(|| SearchScope {
        projects,
        groups: (!groups.is_empty()).then_some(groups),
    });
    // No query → the daemon's "frequently opened" view (only items with opens).
    let frequent_only = query.trim().is_empty();
    let reply = client
        .search(query, filter, limit, scope)
        .call()
        .await
        .map_err(|e| friendly("Search", e))?;

    output::emit(format, &reply, |reply| {
        if reply.issues.is_empty()
            && reply.merge_requests.is_empty()
            && reply.projects.is_empty()
            && reply.groups.is_empty()
            && reply.epics.is_empty()
        {
            return outln!(
                "{}",
                if frequent_only {
                    "no frequently opened items yet (see `forskap issue open`)"
                } else {
                    "no matches"
                }
            );
        }
        if !reply.issues.is_empty() {
            outln!("{}", style::heading("Issues:"))?;
            for i in &reply.issues {
                outln!(
                    "  {:<6} {:<8} {}  {}{}",
                    style::reference('#', i.iid),
                    style::state(&i.state),
                    i.title,
                    i.web_url,
                    opened(i.open_count)
                )?;
            }
        }
        if !reply.merge_requests.is_empty() {
            outln!("{}", style::heading("Merge requests:"))?;
            for m in &reply.merge_requests {
                outln!(
                    "  {:<6} {:<8} {}  {}{}",
                    style::reference('!', m.iid),
                    style::state(&m.state),
                    m.title,
                    m.web_url,
                    opened(m.open_count)
                )?;
            }
        }
        if !reply.epics.is_empty() {
            outln!("{}", style::heading("Epics:"))?;
            for e in &reply.epics {
                outln!(
                    "  {:<6} {:<8} {}  {}{}",
                    style::reference('&', e.iid),
                    style::state(&e.state),
                    e.title,
                    e.web_url,
                    opened(e.open_count)
                )?;
            }
        }
        if !reply.projects.is_empty() {
            outln!("{}", style::heading("Projects:"))?;
            for p in &reply.projects {
                outln!("  {}  {}", p.path, p.web_url)?;
            }
        }
        if !reply.groups.is_empty() {
            outln!("{}", style::heading("Groups:"))?;
            for g in &reply.groups {
                outln!("  {}  {}", g.path, g.web_url)?;
            }
        }
        Ok(())
    })
}

/// Trailing open-count marker for the text rows; empty when never opened.
fn opened(count: i64) -> String {
    if count > 0 {
        format!("  (opened {count}×)")
    } else {
        String::new()
    }
}

pub fn wire_kind(kind: SearchKind) -> WireKind {
    match kind {
        SearchKind::Issues => WireKind::issues,
        SearchKind::Mrs => WireKind::merge_requests,
        SearchKind::Projects => WireKind::projects,
        SearchKind::Groups => WireKind::groups,
        SearchKind::Epics => WireKind::epics,
    }
}
