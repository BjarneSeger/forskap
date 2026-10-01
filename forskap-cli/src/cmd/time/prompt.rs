//! `forskap time prompt` — interactive issue/MR picker + time logger.
//!
//! Shared by [`crate::cmd::tick`], which feeds in an elapsed-time-based
//! duration suggestion.
//!
//! Cancellation contract: Esc / Ctrl-C at any of the three prompts (issue,
//! duration, summary) is a silent skip, not an error — the caller updates the
//! last-prompt timestamp regardless, so a user dismissing a tick prompt isn't
//! re-prompted immediately.

use anyhow::{Context, Result};
use forskap_api::VarlinkClientInterface;
use inquire::Text;

use crate::item::Item;
use crate::refspec;
use crate::{client, config, pick, state, style};

struct PromptAnswers {
    picked: Item,
    duration: String,
    summary: Option<String>,
}

pub async fn run() -> Result<()> {
    let suggested = state::load().unwrap_or_default().elapsed_suggestion();
    if run_with_default_duration(suggested).await? {
        // A successful log resets the interval — so nushell's `forskap tick
        // --mode remind` nudge stops and the next elapsed-time suggestion is
        // measured from now.
        let mut st = state::load().unwrap_or_default();
        st.last_prompt = state::now_secs();
        state::save(&st).context("saving state")?;
    }
    Ok(())
}

/// Run the interactive flow. `suggested_duration` pre-fills the duration
/// input (so the user just hits Enter to accept the elapsed-time suggestion);
/// `None` falls back to the configured `default_duration`. Returns `true` if a
/// time entry was logged, `false` if the user skipped or had no assigned issues.
pub async fn run_with_default_duration(suggested_duration: Option<String>) -> Result<bool> {
    let cfg = config::load()?;
    let client = client::connect(&client::socket(&cfg)?).await?;
    let issues = client
        .get_assigned_work_items(None)
        .call()
        .await
        .map_err(|e| crate::friendly::friendly("GetAssignedWorkItems", e))?
        .work_items;
    let mrs = client
        .get_assigned_merge_requests(None)
        .call()
        .await
        .map_err(|e| crate::friendly::friendly("GetAssignedMergeRequests", e))?
        .merge_requests;

    if issues.is_empty() && mrs.is_empty() {
        outln!("no assigned issues or merge requests")?;
        return Ok(false);
    }

    let suggested = suggested_duration.unwrap_or(cfg.default_duration);

    // inquire's prompts are synchronous, blocking terminal I/O. Run them off
    // the async executor thread so the single-threaded runtime isn't blocked
    // for the (potentially long) duration of user input.
    let answers = tokio::task::spawn_blocking(move || -> Result<Option<PromptAnswers>> {
        // Issues first (the primary tracking objects), MRs after — the daemon
        // pre-sorts MRs newest-updated first.
        let choices: Vec<Item> = issues
            .into_iter()
            .map(Item::Issue)
            .chain(mrs.into_iter().map(Item::Mr))
            .collect();

        let question = "What are you working on?";
        let Some(picked) = pick::select(question, pick::by_number(choices))? else {
            outln!("(skipped)")?;
            return Ok(None);
        };

        let duration = Text::new("Duration:")
            .with_initial_value(&suggested)
            .prompt();
        let Some(duration) = pick::answered(duration, "duration prompt")? else {
            outln!("(skipped)")?;
            return Ok(None);
        };

        let summary = Text::new("Summary (optional):").prompt();
        let summary = pick::answered(summary, "summary prompt")?.filter(|s| !s.trim().is_empty());

        Ok(Some(PromptAnswers {
            picked,
            duration,
            summary,
        }))
    })
    .await??;

    let Some(PromptAnswers {
        picked,
        duration,
        summary,
    }) = answers
    else {
        return Ok(false);
    };

    let kind = picked.kind();
    client
        .post_time(
            picked.project_id(),
            picked.iid(),
            refspec::wire(kind),
            duration.clone(),
            summary,
        )
        .call()
        .await
        .map_err(|e| crate::friendly::friendly("PostTime", e))?;

    let mut st = state::load().unwrap_or_default();
    st.last_issue = Some(state::LastIssue {
        project_id: picked.project_id(),
        issue_iid: picked.iid(),
        kind,
    });
    state::save(&st).context("saving state")?;

    outln!(
        "logged {duration} on {} ({})",
        style::reference(refspec::sigil(kind), picked.iid()),
        picked.title()
    )?;
    Ok(true)
}
