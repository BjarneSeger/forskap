//! `forskap time log` — non-interactive time logging.
//!
//! Designed for scripting and one-off use ("I forgot to start the prompt,
//! just log 45m on #42" — or `'!42'` for a merge request). Tries hard to
//! avoid making the user supply `--project` — see [`crate::cmd::project`]
//! for the precedence rules.

use anyhow::{Context, Result};
use forskap_api::VarlinkClientInterface;

use crate::cmd::{project, queue};
use crate::friendly::friendly;
use crate::{client, refspec, state, style};

pub async fn run(
    reference: &str,
    duration: String,
    mr: bool,
    project: Option<String>,
    summary: Option<String>,
) -> Result<()> {
    let reference = refspec::parse(reference)?;
    let kind = refspec::resolve_kind(reference, mr)?;
    let iid = reference.iid;

    let client = client::connect_default().await?;
    let project_id = project::resolve(&client, kind, iid, project.as_deref()).await?;
    let posted = client
        .post_time(
            project_id,
            iid,
            refspec::wire(kind),
            duration.clone(),
            summary,
        )
        .call()
        .await
        .map_err(|e| friendly("PostTime", e))?;
    let queued = posted.queued == Some(true);

    let mut st = state::load().unwrap_or_default();
    st.last_issue = Some(state::LastIssue {
        project_id,
        issue_iid: iid,
        kind,
    });
    state::save(&st).context("saving state")?;

    outln!(
        "{} {duration} on {} {}",
        logged(queued),
        style::reference(refspec::sigil(kind), iid),
        style::muted(&format!("(project {project_id})"))
    )?;
    if queued {
        outln!("{}", style::muted(queue::NOT_SENT))?;
    }
    Ok(())
}

/// The first word of what a time entry came to: `logged`, or `queued` where
/// the daemon couldn't send it yet.
pub(super) fn logged(queued: bool) -> style::Painted<'static> {
    match queued {
        true => style::state("queued"),
        false => style::success("logged"),
    }
}
