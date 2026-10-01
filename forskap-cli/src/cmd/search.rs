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
use crate::item::is_epic;
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
    let filter = wire_filter(&kinds);
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
        .search(
            query,
            filter.kinds,
            limit,
            scope,
            filter.types,
            filter.exclude_types,
        )
        .call()
        .await
        .map_err(|e| friendly("Search", e))?;

    output::emit(format, &reply, |reply| {
        let (epics, issues): (Vec<_>, Vec<_>) = reply.work_items.iter().partition(|w| is_epic(w));
        if reply.work_items.is_empty()
            && reply.merge_requests.is_empty()
            && reply.projects.is_empty()
            && reply.groups.is_empty()
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
        if !issues.is_empty() {
            outln!("{}", style::heading("Issues:"))?;
            for i in issues {
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
        if !epics.is_empty() {
            outln!("{}", style::heading("Epics:"))?;
            for e in epics {
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

/// What `Search` is asked to keep; `None` for no filter.
#[derive(Debug, Default, PartialEq)]
pub struct Filter {
    pub kinds: Option<Vec<WireKind>>,
    pub types: Option<Vec<String>>,
    pub exclude_types: Option<Vec<String>>,
}

/// The `Search` filter `kinds` ask for. Issues and epics are both work
/// items, told apart by their type: an issue is any work item but an epic.
pub fn wire_filter(kinds: &[SearchKind]) -> Filter {
    if kinds.is_empty() {
        return Filter::default();
    }
    let mut wire = Vec::new();
    for kind in kinds {
        let kind = match kind {
            SearchKind::Issues | SearchKind::Epics => WireKind::work_items,
            SearchKind::Mrs => WireKind::merge_requests,
            SearchKind::Projects => WireKind::projects,
            SearchKind::Groups => WireKind::groups,
        };
        if !wire.contains(&kind) {
            wire.push(kind);
        }
    }
    let epic = || Some(vec!["epic".to_string()]);
    let (types, exclude_types) = match (
        kinds.contains(&SearchKind::Issues),
        kinds.contains(&SearchKind::Epics),
    ) {
        (true, false) => (None, epic()),
        (false, true) => (epic(), None),
        _ => (None, None),
    };
    Filter {
        kinds: Some(wire),
        types,
        exclude_types,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issues_and_epics_are_work_items_of_their_types() {
        let epic = Some(vec!["epic".to_string()]);
        assert_eq!(wire_filter(&[]), Filter::default());
        assert_eq!(
            wire_filter(&[SearchKind::Epics]),
            Filter {
                kinds: Some(vec![WireKind::work_items]),
                types: epic.clone(),
                exclude_types: None,
            }
        );
        assert_eq!(
            wire_filter(&[SearchKind::Issues, SearchKind::Mrs]),
            Filter {
                kinds: Some(vec![WireKind::work_items, WireKind::merge_requests]),
                types: None,
                exclude_types: epic,
            },
            "every type but the epic, those to come included"
        );
        assert_eq!(
            wire_filter(&[SearchKind::Epics, SearchKind::Projects, SearchKind::Issues]),
            Filter {
                kinds: Some(vec![WireKind::work_items, WireKind::projects]),
                ..Filter::default()
            },
            "every type"
        );
        let groups = wire_filter(&[SearchKind::Groups]);
        assert_eq!((groups.types, groups.exclude_types), (None, None));
    }
}
