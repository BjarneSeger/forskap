//! The interactive pickers, drawn by `inquire` on stderr.
//!
//! Esc / Ctrl-C is an answer, not an error: every prompt returns `None` for
//! it and the caller decides what a dismissal means. The prompts block on
//! terminal I/O, so async callers run them through `spawn_blocking`.

use std::fmt;
use std::io::IsTerminal;

use anyhow::{Context, Result};
use forskap_api::Epic;
use inquire::{InquireError, Select};

use crate::cmd::epic::group_of;
use crate::item::Item;
use crate::refspec;

/// The user dismissed a picker the command can't continue without. `main`
/// exits on it without printing an error.
#[derive(Debug, thiserror::Error)]
#[error("cancelled")]
pub struct Cancelled;

/// A value under the line that stands for it in a picker. The generated
/// structs don't implement `Display`, and a line may depend on its neighbours
/// (column widths).
pub struct Labeled<T> {
    pub label: String,
    pub value: T,
}

impl<T> fmt::Display for Labeled<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label)
    }
}

/// Whether a picker can be shown at all: it reads keys from stdin and draws
/// on stderr. Scripts, pipes and the launchers fail this.
pub fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// Turn a prompt's outcome into `None` when the user dismissed it.
pub fn answered<T>(outcome: Result<T, InquireError>, what: &'static str) -> Result<Option<T>> {
    match outcome {
        Ok(answer) => Ok(Some(answer)),
        Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => Ok(None),
        Err(e) => Err(e).context(what),
    }
}

/// Ask for one of `choices`. Blocks.
pub fn select<T>(message: &str, choices: Vec<Labeled<T>>) -> Result<Option<T>> {
    Ok(answered(Select::new(message, choices).prompt(), "picker")?.map(|picked| picked.value))
}

/// Items of mixed kinds and numbers, told apart by the GitLab sigil: `#42`
/// for issues, `!7` for merge requests.
pub fn by_number(items: Vec<Item>) -> Vec<Labeled<Item>> {
    items
        .into_iter()
        .map(|item| Labeled {
            label: format!(
                "{}{:<5} {}",
                refspec::sigil(item.kind()),
                item.iid(),
                item.title()
            ),
            value: item,
        })
        .collect()
}

/// Items sharing a number, told apart by their project.
pub fn by_project(items: Vec<Item>) -> Vec<Labeled<Item>> {
    by_place(items, Item::project, Item::title)
}

/// Epics sharing a number, told apart by their group.
pub fn by_group(epics: Vec<Epic>) -> Vec<Labeled<Epic>> {
    let group = |e: &Epic| match group_of(e) {
        Some(path) => path.to_string(),
        None => format!("group {}", e.group_id),
    };
    by_place(epics, group, |e| &e.title)
}

/// Each value as its place and title, the titles aligned.
fn by_place<T>(
    values: Vec<T>,
    place: impl Fn(&T) -> String,
    title: impl Fn(&T) -> &str,
) -> Vec<Labeled<T>> {
    let places: Vec<String> = values.iter().map(place).collect();
    let width = places.iter().map(|p| p.chars().count()).max().unwrap_or(0);
    values
        .into_iter()
        .zip(places)
        .map(|(value, place)| Labeled {
            label: format!("{place:<width$}  {}", title(&value)),
            value,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::testing::item;
    use crate::refspec::RefKind;

    fn labels(choices: &[Labeled<Item>]) -> Vec<String> {
        choices.iter().map(|c| c.to_string()).collect()
    }

    #[test]
    fn by_number_shows_the_sigil() {
        let choices = by_number(vec![
            item(RefKind::Issue, 1, "team/api", 42, "Fix login"),
            item(RefKind::Mr, 1, "team/api", 7, "Bump deps"),
        ]);
        assert_eq!(labels(&choices), ["#42    Fix login", "!7     Bump deps"]);
    }

    #[test]
    fn by_project_aligns_the_titles() {
        let choices = by_project(vec![
            item(RefKind::Issue, 1, "team/api", 42, "Fix login"),
            item(RefKind::Issue, 2, "team/frontend", 42, "Dark mode"),
            item(RefKind::Issue, 3, "", 42, "No URL"),
        ]);
        assert_eq!(
            labels(&choices),
            [
                "team/api       Fix login",
                "team/frontend  Dark mode",
                "project 3      No URL",
            ]
        );
        assert_eq!(choices[1].value.project_id(), 2);
    }

    #[test]
    fn by_group_names_the_group_or_its_id() {
        let epic = |group_id, web_url: &str, title: &str| Epic {
            id: group_id,
            iid: 5,
            group_id,
            title: title.to_string(),
            web_url: web_url.to_string(),
            state: "opened".to_string(),
            open_count: 0,
            group_path: String::new(),
        };
        let choices = by_group(vec![
            epic(3, "https://gl/groups/team/backend/-/epics/5", "Accounts"),
            epic(4, "", "Billing"),
        ]);
        let labels: Vec<String> = choices.iter().map(|c| c.to_string()).collect();
        assert_eq!(labels, ["team/backend  Accounts", "group 4       Billing"]);
    }

    #[test]
    fn a_dismissed_prompt_is_no_answer() {
        for dismissal in [
            InquireError::OperationCanceled,
            InquireError::OperationInterrupted,
        ] {
            assert!(answered::<()>(Err(dismissal), "p").unwrap().is_none());
        }
        assert_eq!(answered(Ok(1), "p").unwrap(), Some(1));
        assert!(answered::<()>(Err(InquireError::NotTTY), "p").is_err());
    }
}
