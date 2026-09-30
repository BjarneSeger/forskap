//! `forskap epic view` — what the daemon's caches hold on one epic.

use anyhow::Result;

use super::locate;
use crate::cli::{EpicArgs, OutputFormat};
use crate::output;

pub async fn run(target: EpicArgs, format: OutputFormat) -> Result<()> {
    let (_client, epic) = locate(&target).await?;
    output::emit(format, &epic, |e| {
        outln!("&{} {}", e.iid, e.title)?;
        field("state", &e.state)?;
        field("group", &e.group_id.to_string())?;
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
