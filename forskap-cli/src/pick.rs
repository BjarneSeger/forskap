//! The interactive pickers, drawn by `inquire` on stderr.
//!
//! Esc / Ctrl-C is an answer, not an error: every prompt returns `None` for
//! it and the caller decides what a dismissal means. The prompts block on
//! terminal I/O, so async callers run them through `spawn_blocking`.

use std::fmt;
use std::io::IsTerminal;

use anyhow::{Context, Result};
use inquire::{InquireError, Select};

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
    let projects: Vec<String> = items.iter().map(Item::project).collect();
    let width = projects
        .iter()
        .map(|p| p.chars().count())
        .max()
        .unwrap_or(0);
    items
        .into_iter()
        .zip(projects)
        .map(|(item, project)| Labeled {
            label: format!("{project:<width$}  {}", item.title()),
            value: item,
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
