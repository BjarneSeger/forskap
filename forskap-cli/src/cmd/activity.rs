//! `forskap activity` — your contribution events of the last `days` days,
//! newest first: what the daemon's sync already keeps to tell which projects
//! you are active in.

use anyhow::Result;
use chrono::{DateTime, Local};
use forskap_api::{ActivityEvent, VarlinkClientInterface};

use crate::cli::OutputFormat;
use crate::friendly::friendly;
use crate::{client, output, style};

pub async fn run(days: u32, format: OutputFormat) -> Result<()> {
    let client = client::connect_default().await?;
    let reply = client
        .get_activity(Some(i64::from(days)))
        .call()
        .await
        .map_err(|e| friendly("GetActivity", e))?;

    output::emit(format, &reply.events, |events| {
        if events.is_empty() {
            let none = format!("no activity in the last {days} days");
            return outln!("{}", style::muted(&none));
        }
        // One heading per day, in local time.
        let mut day = None;
        for e in events {
            let at = DateTime::from_timestamp(e.timestamp, 0)
                .unwrap_or_default()
                .with_timezone(&Local);
            if day != Some(at.date_naive()) {
                if day.is_some() {
                    outln!("")?;
                }
                let date = at.format("%a %Y-%m-%d").to_string();
                outln!("{}", style::heading(&date))?;
                day = Some(at.date_naive());
            }
            let time = at.format("%H:%M").to_string();
            outln!("  {}  {}", style::muted(&time), describe(e))?;
        }
        Ok(())
    })
}

/// What happened, to which item, in which project.
fn describe(e: &ActivityEvent) -> String {
    let project = match (&e.project_path, e.project_id) {
        (Some(path), _) => path.clone(),
        (None, Some(id)) => format!("project {id}"),
        (None, None) => String::new(),
    };
    let sigil = match e.target_type.as_deref() {
        Some("Issue" | "WorkItem") => "#",
        Some("MergeRequest") => "!",
        Some("Milestone") => "%",
        _ => "",
    };
    let place = style::path(&project);
    let item = match e.target_iid {
        Some(iid) if !sigil.is_empty() => format!("{place}{}", style::reference(sigil, iid)),
        _ => place.to_string(),
    };
    let detail = match (&e.r#ref, &e.commit_title) {
        (Some(git_ref), Some(title)) => match e.commit_count {
            Some(n) if n > 1 => format!("{git_ref}: {title} (+{} more)", n - 1),
            _ => format!("{git_ref}: {title}"),
        },
        (Some(git_ref), None) => git_ref.clone(),
        // Of a comment: what it says, after the item it is on.
        (None, _) => match (&e.target_title, &e.description) {
            (Some(title), Some(text)) => format!("{title} — {text}"),
            (title, text) => title.clone().or(text.clone()).unwrap_or_default(),
        },
    };
    let mut line = format!("{:<12}", style::action(&e.action));
    for part in [item, detail] {
        if !part.is_empty() {
            line.push_str("  ");
            line.push_str(&part);
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(action: &str, target_type: &str, target_iid: Option<i64>) -> ActivityEvent {
        ActivityEvent {
            timestamp: 0,
            action: action.into(),
            target_type: (!target_type.is_empty()).then(|| target_type.into()),
            target_iid,
            target_title: None,
            project_id: Some(7),
            project_path: Some("team/api".into()),
            web_url: None,
            r#ref: None,
            commit_count: None,
            commit_title: None,
            description: None,
        }
    }

    #[test]
    fn describes_items_pushes_and_bare_events() {
        let mut closed = event("closed", "Issue", Some(3));
        closed.target_title = Some("Fix the login".into());
        assert_eq!(describe(&closed), "closed        team/api#3  Fix the login");

        let mut comment = event("commented on", "MergeRequest", Some(12));
        comment.target_title = Some("Add x".into());
        assert_eq!(describe(&comment), "commented on  team/api!12  Add x");
        comment.description = Some("lgtm".into());
        assert_eq!(
            describe(&comment),
            "commented on  team/api!12  Add x — lgtm"
        );
        comment.target_title = None;
        assert_eq!(describe(&comment), "commented on  team/api!12  lgtm");

        let mut push = event("pushed to", "", None);
        push.r#ref = Some("main".into());
        push.commit_count = Some(3);
        push.commit_title = Some("Fix it".into());
        push.description = push.commit_title.clone();
        assert_eq!(
            describe(&push),
            "pushed to     team/api  main: Fix it (+2 more)"
        );
        push.commit_count = Some(1);
        assert_eq!(describe(&push), "pushed to     team/api  main: Fix it");
        push.commit_title = None;
        assert_eq!(describe(&push), "pushed to     team/api  main");

        assert_eq!(
            describe(&event("joined", "", None)),
            "joined        team/api"
        );

        style::force(true);
        assert_eq!(
            describe(&closed),
            "\x1b[31mclosed      \x1b[0m  \x1b[34mteam/api\x1b[0m\x1b[36m#3\x1b[0m  Fix the login"
        );
        assert_eq!(
            describe(&push),
            "\x1b[34mpushed to   \x1b[0m  \x1b[34mteam/api\x1b[0m  main"
        );
        style::force(false);

        // A note's number is no item number, and an unknown project shows its id.
        let mut wiki = event("created", "WikiPage::Meta", Some(9));
        wiki.project_path = None;
        wiki.target_title = Some("Home".into());
        assert_eq!(describe(&wiki), "created       project 7  Home");
        // Outside any project.
        wiki.project_id = None;
        assert_eq!(describe(&wiki), "created       Home");
    }
}
