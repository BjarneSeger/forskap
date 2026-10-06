//! `forskap issue unassign` / `forskap mr unassign` — remove yourself from the assignees. Other assignees stay in place.

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
        .unassign_self(project_id, iid, refspec::wire(kind))
        .call()
        .await
        .map_err(|e| friendly("UnassignSelf", e))?;

    outln!(
        "{} {} {}",
        style::success("unassigned from"),
        style::reference(refspec::sigil(kind), iid),
        style::muted(&format!("(project {project_id})"))
    )?;
    Ok(())
}
