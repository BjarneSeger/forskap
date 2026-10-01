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

/// GitLab's work item types other than the epic: what `--kind issues` asks
/// for without `--kind epics`.
const ISSUE_TYPES: [&str; 8] = [
    "issue",
    "incident",
    "test_case",
    "requirement",
    "task",
    "objective",
    "key_result",
    "ticket",
];

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
    let (filter, types) = wire_filter(&kinds);
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
        .search(query, filter, limit, scope, types)
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

/// The `Search` kinds and work item types `kinds` ask for; `None` for no
/// filter. Issues and epics are both work items, told apart by their type.
pub fn wire_filter(kinds: &[SearchKind]) -> (Option<Vec<WireKind>>, Option<Vec<String>>) {
    if kinds.is_empty() {
        return (None, None);
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
    let types: &[&str] = match (
        kinds.contains(&SearchKind::Issues),
        kinds.contains(&SearchKind::Epics),
    ) {
        (true, false) => &ISSUE_TYPES,
        (false, true) => &["epic"],
        _ => &[],
    };
    let types = (!types.is_empty()).then(|| types.iter().map(|t| t.to_string()).collect());
    (Some(wire), types)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn types(kinds: &[SearchKind]) -> Option<Vec<String>> {
        wire_filter(kinds).1
    }

    #[test]
    fn issues_and_epics_are_work_items_of_their_types() {
        assert_eq!(wire_filter(&[]), (None, None));
        let (wire, epics) = wire_filter(&[SearchKind::Epics]);
        assert_eq!(wire, Some(vec![WireKind::work_items]));
        assert_eq!(epics, Some(vec!["epic".to_string()]));

        let issues = types(&[SearchKind::Issues, SearchKind::Mrs]).unwrap();
        assert!(issues.contains(&"task".to_string()), "{issues:?}");
        assert!(!issues.contains(&"epic".to_string()), "{issues:?}");

        let (wire, both) =
            wire_filter(&[SearchKind::Epics, SearchKind::Projects, SearchKind::Issues]);
        assert_eq!(wire, Some(vec![WireKind::work_items, WireKind::projects]));
        assert_eq!(both, None, "every type");
        assert_eq!(types(&[SearchKind::Groups]), None);
    }
}
