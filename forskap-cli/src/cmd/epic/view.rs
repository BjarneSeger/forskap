//! `forskap epic view` — what the daemon's caches hold on one epic.

use anyhow::Result;

use super::{group_of, locate};
use crate::cli::{EpicArgs, OutputFormat};
use crate::cmd::field;
use crate::{output, style};

pub async fn run(target: EpicArgs, format: OutputFormat) -> Result<()> {
    let (_client, epic) = locate(&target).await?;
    output::emit(format, &epic, |e| {
        outln!(
            "{} {}",
            style::reference('&', e.iid),
            style::strong(&e.title)
        )?;
        field("state", &style::state(&e.state).to_string())?;
        let id = || e.group_id.unwrap_or_default().to_string();
        let group = group_of(e).map_or_else(id, str::to_string);
        field("group", &style::path(&group).to_string())?;
        field("url", &style::muted(&e.web_url).to_string())?;
        if e.open_count > 0 {
            field("opened", &format!("{}×", e.open_count))?;
        }
        Ok(())
    })
}
