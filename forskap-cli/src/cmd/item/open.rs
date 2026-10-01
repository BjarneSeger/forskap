//! `forskap issue open` / `forskap mr open` — open one in the browser and count the open
//! in the daemon, so `forskap search` (and the launcher integrations built on it)
//! rank it higher next time.

use anyhow::Result;
use forskap_api::VarlinkClientInterface;

use super::{locate, lookup};
use crate::cli::TargetArgs;
use crate::friendly::friendly;
use crate::refspec::{self, RefKind};
use crate::style;

pub async fn run(kind: RefKind, target: TargetArgs, no_browser: bool) -> Result<()> {
    let iid = target.iid;
    let (client, project_id) = locate(kind, &target).await?;
    let item = lookup(&client, kind, project_id, iid).await?;
    let web_url = item.web_url();

    client
        .record_open(refspec::wire(kind), iid, Some(project_id), None)
        .call()
        .await
        .map_err(|e| friendly("RecordOpen", e))?;

    if !no_browser {
        // Detached: the caller (a shell, a launcher plugin) must not block on
        // the browser process.
        if let Err(e) = open::that_detached(web_url) {
            eprintln!("(couldn't open browser automatically: {e})");
        }
    }
    outln!(
        "opened {} {web_url}",
        style::reference(refspec::sigil(kind), iid)
    )?;
    Ok(())
}
