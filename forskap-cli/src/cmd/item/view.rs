//! `forskap issue view` / `forskap mr view` — what the daemon's caches hold on one item.

use anyhow::Result;

use super::{locate, lookup};
use crate::cli::{OutputFormat, TargetArgs};
use crate::cmd::{field, time};
use crate::item::Item;
use crate::refspec::{self, RefKind};
use crate::{output, style};

pub async fn run(kind: RefKind, target: TargetArgs, format: OutputFormat) -> Result<()> {
    let (client, project_id) = locate(kind, &target).await?;
    match lookup(&client, kind, project_id, target.iid).await? {
        Item::Issue(i) => output::emit(format, &i, |i| {
            let sigil = refspec::sigil(kind);
            outln!(
                "{} {}",
                style::reference(sigil, i.iid),
                style::strong(&i.title)
            )?;
            field("state", &style::state(&i.state).to_string())?;
            field("project", &i.project_id.unwrap_or_default().to_string())?;
            field("url", &style::muted(&i.web_url).to_string())?;
            let spent = i.time_spent.filter(|&s| s > 0).map(time::spent);
            field("time spent", spent.as_deref().unwrap_or_default())?;
            field("status", i.board_column.as_deref().unwrap_or_default())?;
            let parent = i.parent.as_ref().and_then(|p| p.web_url.as_deref());
            field(
                "parent",
                &style::muted(parent.unwrap_or_default()).to_string(),
            )?;
            opened(i.open_count)
        }),
        Item::Mr(m) => output::emit(format, &m, |m| {
            let sigil = refspec::sigil(kind);
            outln!(
                "{} {}",
                style::reference(sigil, m.iid),
                style::strong(&m.title)
            )?;
            field("state", &style::state(&m.state).to_string())?;
            field("project", &m.project_id.to_string())?;
            field("url", &style::muted(&m.web_url).to_string())?;
            field("assignees", &m.assignees.join(", "))?;
            opened(m.open_count)
        }),
    }
}

fn opened(count: i64) -> Result<()> {
    if count > 0 {
        field("opened", &format!("{count}×"))?;
    }
    Ok(())
}
