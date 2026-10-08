//! `forskap issue unassign` / `forskap mr unassign` — remove yourself from the assignees. Other assignees stay in place.

use anyhow::Result;
use forskap_api::VarlinkClientInterface;

use super::{locate, said};
use crate::cli::TargetArgs;
use crate::friendly::friendly;
use crate::refspec::{self, RefKind};

pub async fn run(kind: RefKind, target: TargetArgs) -> Result<()> {
    let iid = target.iid;
    let (client, project_id) = locate(kind, &target).await?;
    let reply = client
        .unassign_self(project_id, iid, refspec::wire(kind))
        .call()
        .await
        .map_err(|e| friendly("UnassignSelf", e))?;

    said(
        reply.queued,
        "unassigned from",
        "unassigning you from",
        kind,
        iid,
        project_id,
    )
}
