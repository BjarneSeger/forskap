//! `tt issue unassign` / `tt mr unassign` — remove yourself from the assignees. Other assignees stay in place.

use anyhow::Result;
use gitlab_trackr_api::VarlinkClientInterface;

use super::locate;
use crate::cli::TargetArgs;
use crate::friendly::friendly;
use crate::refspec::{self, RefKind};

pub async fn run(kind: RefKind, target: TargetArgs) -> Result<()> {
    let iid = target.iid;
    let (client, project_id) = locate(kind, &target).await?;
    client
        .unassign_self(project_id, iid, refspec::wire(kind))
        .call()
        .await
        .map_err(|e| friendly("UnassignSelf", e))?;

    println!(
        "unassigned from {}{iid} (project {project_id})",
        refspec::sigil(kind)
    );
    Ok(())
}
