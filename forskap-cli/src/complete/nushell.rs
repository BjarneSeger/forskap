//! Nushell as a `COMPLETE=nushell` shell, which clap_complete doesn't bring
//! (clap-rs/clap#5840).

use std::ffi::OsString;
use std::io::Write;
use std::path::Path;

use clap_complete::env::EnvCompleter;
use serde::Serialize;

/// Also what `build.rs` writes, for the bare `forskap`.
const REGISTRATION: &str = include_str!("nushell.nu");

pub struct Nushell;

/// One candidate, as nushell's completers return them.
#[derive(Serialize)]
struct Row {
    value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

impl EnvCompleter for Nushell {
    fn name(&self) -> &'static str {
        "nushell"
    }

    fn is(&self, name: &str) -> bool {
        name == "nushell" || name == "nu"
    }

    fn write_registration(
        &self,
        _var: &str,
        _name: &str,
        _bin: &str,
        completer: &str,
        buf: &mut dyn Write,
    ) -> Result<(), std::io::Error> {
        buf.write_all(REGISTRATION.replace("{completer}", completer).as_bytes())
    }

    fn write_complete(
        &self,
        cmd: &mut clap::Command,
        mut args: Vec<OsString>,
        current_dir: Option<&Path>,
        buf: &mut dyn Write,
    ) -> Result<(), std::io::Error> {
        let index = args.len() - 1;
        if let Some(word) = args[index].to_str().map(unquote).map(OsString::from) {
            args[index] = word;
        }
        let rows: Vec<Row> = clap_complete::engine::complete(cmd, args, index, current_dir)?
            .into_iter()
            .map(|candidate| Row {
                value: quote(&candidate.get_value().to_string_lossy()),
                description: candidate
                    .get_help()
                    .map(|help| help.to_string().lines().next().unwrap_or_default().into()),
            })
            .collect();
        Ok(serde_json::to_writer(buf, &rows)?)
    }
}

/// The word at the cursor without the quotes nushell hands over with it:
/// `'#1` while it is being typed, `'#12'` once closed.
fn unquote(word: &str) -> &str {
    match word.chars().next() {
        Some(mark @ ('\'' | '"' | '`')) => {
            let rest = &word[1..];
            rest.strip_suffix(mark).unwrap_or(rest)
        }
        _ => word,
    }
}

/// A value the way nushell reads it back as one word: a bare `#12` would
/// start a comment.
fn quote(value: &str) -> String {
    let bare = !value.starts_with('#')
        && !value.contains(|c: char| c.is_whitespace() || matches!(c, '\'' | '"' | '`'));
    if bare {
        value.into()
    } else if !value.contains('\'') {
        format!("'{value}'")
    } else {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

#[cfg(test)]
mod tests {
    use clap::{Arg, Command};
    use clap_complete::{ArgValueCompleter, CompletionCandidate};

    use super::*;

    /// `demo log <REF>`, offering two refs in an order that isn't the
    /// alphabetical one.
    fn demo() -> Command {
        let refs = ArgValueCompleter::new(|current: &std::ffi::OsStr| {
            let current = current.to_string_lossy().into_owned();
            [("#7", Some("Seven")), ("#12", None)]
                .into_iter()
                .filter(|(value, _)| value.starts_with(&current))
                .enumerate()
                .map(|(rank, (value, help))| {
                    CompletionCandidate::new(value)
                        .help(help.map(Into::into))
                        .display_order(Some(rank))
                })
                .collect()
        });
        Command::new("demo").subcommand(Command::new("log").arg(Arg::new("reference").add(refs)))
    }

    fn complete(words: &[&str]) -> serde_json::Value {
        let mut out = Vec::new();
        let args = words.iter().map(OsString::from).collect();
        Nushell
            .write_complete(&mut demo(), args, None, &mut out)
            .unwrap();
        serde_json::from_slice(&out).unwrap()
    }

    #[test]
    fn candidates_keep_their_order_and_description() {
        assert_eq!(
            complete(&["demo", "log", "'#"]),
            serde_json::json!([
                {"value": "'#7'", "description": "Seven"},
                {"value": "'#12'"},
            ])
        );
    }

    #[test]
    fn the_word_completes_with_or_without_its_quotes() {
        let twelve = serde_json::json!([{"value": "'#12'"}]);
        assert_eq!(complete(&["demo", "log", "'#1"]), twelve);
        assert_eq!(complete(&["demo", "log", "'#12'"]), twelve);
        assert_eq!(complete(&["demo", "log", "\"#1"]), twelve);
    }

    #[test]
    fn subcommands_stay_bare() {
        let found = complete(&["demo", "l"]);
        assert_eq!(found[0]["value"], "log");
    }

    #[test]
    fn only_words_nushell_would_misread_are_quoted() {
        assert_eq!(quote("12"), "12");
        assert_eq!(quote("--project"), "--project");
        assert_eq!(quote("group/project"), "group/project");
        assert_eq!(quote("#12"), "'#12'");
        assert_eq!(quote("two words"), "'two words'");
        assert_eq!(quote("it's \"x\""), r#""it's \"x\"""#);
    }

    #[test]
    fn registration_calls_the_given_completer() {
        let mut script = Vec::new();
        Nushell
            .write_registration(
                "COMPLETE",
                "forskap",
                "forskap",
                "/opt/forskap",
                &mut script,
            )
            .unwrap();
        let script = String::from_utf8(script).unwrap();
        assert!(script.contains("^r#'/opt/forskap'# -- ...$words"));
        assert!(!script.contains("{completer}"));
    }
}
