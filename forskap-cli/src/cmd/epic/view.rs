//! `forskap epic view` — what the daemon's caches hold on one epic.

use anyhow::Result;

use super::{group_of, locate};
use crate::cli::{EpicArgs, OutputFormat};
use crate::{output, style};

pub async fn run(target: EpicArgs, format: OutputFormat) -> Result<()> {
    let (_client, epic) = locate(&target).await?;
    output::emit(format, &epic, |e| {
        outln!("{} {}", style::reference('&', e.iid), e.title)?;
        field("state", &style::state(&e.state).to_string())?;
        let id = || e.group_id.unwrap_or_default().to_string();
        let group = group_of(e).map_or_else(id, str::to_string);
        field("group", &group)?;
        field("url", &e.web_url)?;
        if e.open_count > 0 {
            field("opened", &format!("{}×", e.open_count))?;
        }
        Ok(())
    })
}

fn field(name: &str, value: &str) -> Result<()> {
    if !value.is_empty() {
        outln!("  {:<11} {value}", format!("{name}:"))?;
    }
    Ok(())
}
