//! `forskap issue view` / `forskap mr view` — what the daemon's caches hold on one item.

use anyhow::Result;

use super::{locate, lookup};
use crate::cli::{OutputFormat, TargetArgs};
use crate::item::Item;
use crate::refspec::{self, RefKind};
use crate::{output, style};

pub async fn run(kind: RefKind, target: TargetArgs, format: OutputFormat) -> Result<()> {
    let (client, project_id) = locate(kind, &target).await?;
    match lookup(&client, kind, project_id, target.iid).await? {
        Item::Issue(i) => output::emit(format, &i, |i| {
            let sigil = refspec::sigil(kind);
            outln!("{} {}", style::reference(sigil, i.iid), i.title)?;
            field("state", &style::state(&i.state).to_string())?;
            field("project", &i.project_id.to_string())?;
            field("url", &i.web_url)?;
            field("time spent", &i.total_time)?;
            field("status", &i.graph_status)?;
            field("parent", &i.parent)?;
            opened(i.open_count)
        }),
        Item::Mr(m) => output::emit(format, &m, |m| {
            let sigil = refspec::sigil(kind);
            outln!("{} {}", style::reference(sigil, m.iid), m.title)?;
            field("state", &style::state(&m.state).to_string())?;
            field("project", &m.project_id.to_string())?;
            field("url", &m.web_url)?;
            field("assignees", &m.assignees.join(", "))?;
            opened(m.open_count)
        }),
    }
}

fn field(name: &str, value: &str) -> Result<()> {
    if !value.is_empty() {
        outln!("  {:<11} {value}", format!("{name}:"))?;
    }
    Ok(())
}

fn opened(count: i64) -> Result<()> {
    if count > 0 {
        field("opened", &format!("{count}×"))?;
    }
    Ok(())
}
