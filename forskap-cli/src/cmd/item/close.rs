//! `forskap issue close` / `forskap mr close` — close one.

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
        .close(project_id, iid, refspec::wire(kind))
        .call()
        .await
        .map_err(|e| friendly("Close", e))?;

    said(reply.queued, "closed", "closing", kind, iid, project_id)
}
