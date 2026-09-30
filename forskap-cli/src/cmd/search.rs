//! `forskap search` — query the daemon's cached search corpus.
//!
//! Pure cache read: the daemon's background search sync owns freshness
//! (incremental `updated_after` pulls, a periodic full resync), so this just
//! serves whatever was last synced — no fetch, effectively free.

use anyhow::Result;
use forskap_api::VarlinkClientInterface;

use crate::cli::{OutputFormat, SearchKind};
use crate::friendly::friendly;
use crate::{client, output};

pub async fn run(
    query: Vec<String>,
    kinds: Vec<SearchKind>,
    limit: Option<i64>,
    format: OutputFormat,
) -> Result<()> {
    let client = client::connect_default().await?;
    let query = query.join(" ");
    let filter = (!kinds.is_empty()).then(|| kinds.iter().map(|k| wire_kind(*k)).collect());
    // No query → the daemon's "frequently opened" view (only items with opens).
    let frequent_only = query.trim().is_empty();
    let reply = client
        .search(query, filter, limit)
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
            outln!("Issues:")?;
            for i in &reply.issues {
                outln!(
                    "  #{:<5} {:<8} {}  {}{}",
                    i.iid,
                    i.state,
                    i.title,
                    i.web_url,
                    opened(i.open_count)
                )?;
            }
        }
        if !reply.merge_requests.is_empty() {
            outln!("Merge requests:")?;
            for m in &reply.merge_requests {
                outln!(
                    "  !{:<5} {:<8} {}  {}{}",
                    m.iid,
                    m.state,
                    m.title,
                    m.web_url,
                    opened(m.open_count)
                )?;
            }
        }
        if !reply.epics.is_empty() {
            outln!("Epics:")?;
            for e in &reply.epics {
                outln!(
                    "  &{:<5} {:<8} {}  {}{}",
                    e.iid,
                    e.state,
                    e.title,
                    e.web_url,
                    opened(e.open_count)
                )?;
            }
        }
        if !reply.projects.is_empty() {
            outln!("Projects:")?;
            for p in &reply.projects {
                outln!("  {}  {}", p.path, p.web_url)?;
            }
        }
        if !reply.groups.is_empty() {
            outln!("Groups:")?;
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

fn wire_kind(kind: SearchKind) -> String {
    match kind {
        SearchKind::Issues => "issues",
        SearchKind::Mrs => "merge_requests",
        SearchKind::Projects => "projects",
        SearchKind::Groups => "groups",
        SearchKind::Epics => "epics",
    }
    .to_string()
}
