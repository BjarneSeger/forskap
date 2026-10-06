//! `forskap epic open` — open one in the browser and count the open in the
//! daemon, so `forskap search` (and the launcher integrations built on it) rank
//! it higher next time.

use anyhow::Result;
use forskap_api::{IssuableKind, VarlinkClientInterface, WorkItem};

use super::locate;
use crate::cli::EpicArgs;
use crate::friendly::friendly;
use crate::state::{self, LastEpic};
use crate::style;

pub async fn run(target: EpicArgs, no_browser: bool) -> Result<()> {
    let (client, epic) = locate(&target).await?;

    client
        .record_open(IssuableKind::work_item, epic.iid, None, epic.group_id)
        .call()
        .await
        .map_err(|e| friendly("RecordOpen", e))?;
    remember(&epic);

    if !no_browser {
        // Detached: the caller (a shell, a launcher plugin) must not block on
        // the browser process.
        if let Err(e) = open::that_detached(&epic.web_url) {
            eprintln!("(couldn't open browser automatically: {e})");
        }
    }
    outln!(
        "{} {} {}",
        style::success("opened"),
        style::reference('&', epic.iid),
        style::muted(&epic.web_url)
    )?;
    Ok(())
}

/// Best effort: a state file that can't be written only costs a `--group`
/// next time.
fn remember(epic: &WorkItem) {
    let (Ok(mut st), Some(group_id)) = (state::load(), epic.group_id) else {
        return;
    };
    st.last_epic = Some(LastEpic {
        group_id,
        iid: epic.iid,
    });
    let _ = state::save(&st);
}
