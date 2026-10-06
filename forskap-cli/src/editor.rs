//! The description editor: `$VISUAL`, else `$EDITOR`, on a temporary
//! Markdown file. Without either, nothing is opened: `vi` on a user who
//! never chose it is a trap, not a default.
//!
//! The command runs through `sh -c`, as git runs its editor, so `code --wait`
//! or `emacsclient -t` work as given. It inherits the terminal, so it may only
//! run where stdin, stdout and stderr are one ([`available`]) — a pipe on any
//! side (`-o json | jq`) means no editor — and it blocks: async callers run
//! it under `spawn_blocking`, as they run the pickers.

use std::io::{IsTerminal, Write};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Whether an editor could be opened at all.
pub fn available() -> bool {
    std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && std::io::stderr().is_terminal()
}

/// The editor the user chose, as a shell command: `$VISUAL`, else
/// `$EDITOR`; `None` with neither set.
pub fn configured() -> Option<String> {
    chosen(std::env::var("VISUAL").ok(), std::env::var("EDITOR").ok())
}

fn chosen(visual: Option<String>, editor: Option<String>) -> Option<String> {
    visual
        .into_iter()
        .chain(editor)
        .find(|command| !command.trim().is_empty())
}

/// Edit `initial` in `editor` and return what was saved.
pub fn edit(editor: &str, initial: &str) -> Result<String> {
    let mut file = tempfile::Builder::new()
        .prefix("forskap-issue-")
        .suffix(".md")
        .tempfile()
        .context("creating the file to edit")?;
    file.write_all(initial.as_bytes())
        .and_then(|()| file.flush())
        .context("writing the file to edit")?;
    // `sh -c '<editor> "$@"' <editor> <file>`: the command keeps its own
    // arguments, and the file name is passed along, never parsed.
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$@\""))
        .arg(editor)
        .arg(file.path())
        .status()
        .with_context(|| format!("running the editor `{editor}`"))?;
    if !status.success() {
        bail!("the editor `{editor}` failed ({status})");
    }
    std::fs::read_to_string(file.path()).context("reading the edited file")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visual_wins_then_editor_and_a_blank_one_is_none() {
        let set = |command: &str| Some(command.to_string());
        assert_eq!(
            chosen(set("code --wait"), set("vim")).as_deref(),
            Some("code --wait")
        );
        assert_eq!(chosen(None, set("vim")).as_deref(), Some("vim"));
        assert_eq!(chosen(set(""), set("vim")).as_deref(), Some("vim"));
        assert_eq!(chosen(set(""), set("  ")), None);
        assert_eq!(chosen(None, None), None);
    }

    #[test]
    fn the_editor_runs_as_a_command_on_a_markdown_file() {
        // An "editor" of its own arguments: appends `$1`, then the file's
        // own name, to the file it is given.
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("append.sh");
        std::fs::write(&script, "printf '%s\\n%s\\n' \"$1\" \"$2\" >> \"$2\"\n").unwrap();
        let editor = format!("sh {} '## Steps'", script.display());
        let edited = edit(&editor, "## Bug\n").unwrap();
        let mut lines = edited.lines();
        assert_eq!(lines.next(), Some("## Bug"));
        assert_eq!(lines.next(), Some("## Steps"));
        let file = lines.next().unwrap();
        assert!(file.ends_with(".md"), "{file}");
        assert!(!std::path::Path::new(file).exists(), "removed after");

        let failed = edit("false", "").unwrap_err().to_string();
        assert_eq!(failed, "the editor `false` failed (exit status: 1)");
    }
}
