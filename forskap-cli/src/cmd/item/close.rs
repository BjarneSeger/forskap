//! `forskap issue close` / `forskap mr close` — close one.

use anyhow::Result;
use forskap_api::VarlinkClientInterface;

use super::locate;
use crate::cli::TargetArgs;
use crate::friendly::friendly;
use crate::refspec::{self, RefKind};
use crate::style;

pub async fn run(kind: RefKind, target: TargetArgs) -> Result<()> {
    let iid = target.iid;
    let (client, project_id) = locate(kind, &target).await?;
    client
        .close(project_id, iid, refspec::wire(kind))
        .call()
        .await
        .map_err(|e| friendly("Close", e))?;

    outln!(
        "{} {} {}",
        style::success("closed"),
        style::reference(refspec::sigil(kind), iid),
        style::muted(&format!("(project {project_id})"))
    )?;
    Ok(())
}
