//! `forskap epic open` — open one in the browser and count the open in the
//! daemon, so `forskap search` (and the launcher integrations built on it) rank
//! it higher next time.

use anyhow::Result;
use forskap_api::VarlinkClientInterface;

use super::locate;
use crate::cli::EpicArgs;
use crate::friendly::friendly;
use crate::state::{self, LastEpic};
use crate::style;

pub async fn run(target: EpicArgs, no_browser: bool) -> Result<()> {
    let (client, epic) = locate(&target).await?;

    client
        .record_epic_open(epic.group_id, epic.iid)
        .call()
        .await
        .map_err(|e| friendly("RecordEpicOpen", e))?;
    remember(&epic);

    if !no_browser {
        // Detached: the caller (a shell, a launcher plugin) must not block on
        // the browser process.
        if let Err(e) = open::that_detached(&epic.web_url) {
            eprintln!("(couldn't open browser automatically: {e})");
        }
    }
    outln!(
        "opened {} {}",
        style::reference('&', epic.iid),
        epic.web_url
    )?;
    Ok(())
}

/// Best effort: a state file that can't be written only costs a `--group`
/// next time.
fn remember(epic: &forskap_api::Epic) {
    let Ok(mut st) = state::load() else {
        return;
    };
    st.last_epic = Some(LastEpic {
        group_id: epic.group_id,
        iid: epic.iid,
    });
    let _ = state::save(&st);
}
