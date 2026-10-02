//! rofi's script mode (`man rofi-script`): rofi runs us without an entry for
//! the list, filters it itself while the user types, and runs us again with
//! what was picked, `ROFI_RETV` telling which. Nothing is asked per
//! keystroke, so the list holds every project up front (`match_all`) next to
//! the frequently opened items; typed text sent with Ctrl+Enter is searched in
//! the daemon with the shared grammar ([`query::parse`]).
//!
//! Each row carries its result id as `info`, which rofi hands back as
//! `ROFI_INFO`. A list without rows makes rofi quit, so a failure or an empty
//! answer is a row too, one that can't be picked.

use anyhow::Result;
use forskap_api::{SearchOptions, VarlinkClient, VarlinkClientInterface};

use super::Provider;
use super::query;
use super::results::{self, Row};
use crate::cli::SearchKind;
use crate::client;
use crate::cmd::search::wire_filter;
use crate::friendly::friendly;

/// The grammar for Ctrl+Enter (rofi's default `kb-accept-custom`), as Pango
/// markup: rofi renders the message bar with it.
const HINT: &str = "\0message\x1fCtrl+Enter searches: i|mr|e|g|all &lt;text&gt;, #42, !42, &amp;42";

/// What rofi asks of this run, from `ROFI_RETV`.
#[derive(Debug, PartialEq, Eq)]
enum Ask {
    /// The first run (0), or a key we don't use: list everything.
    List,
    /// A listed row was picked (1).
    Open,
    /// Text that is no row was entered (2).
    Search,
}

fn ask(retv: Option<&str>) -> Ask {
    match retv.map(str::trim) {
        Some("1") => Ask::Open,
        Some("2") => Ask::Search,
        _ => Ask::List,
    }
}

pub(super) async fn serve(provider: Provider, entry: Option<String>) -> Result<()> {
    match ask(std::env::var("ROFI_RETV").ok().as_deref()) {
        Ask::Open => {
            let Ok(id) = std::env::var("ROFI_INFO") else {
                return Ok(());
            };
            // Printing nothing closes rofi; a failure stays on screen.
            if let Err(e) = provider.activate(&id).await {
                outln!("{}", notice(&format!("{e:#}")))?;
            }
            Ok(())
        }
        Ask::Search => match entry.as_deref().map(str::trim) {
            Some(text) if !text.is_empty() => search(&provider, text).await,
            _ => list(&provider).await,
        },
        Ask::List => list(&provider).await,
    }
}

/// The frequently opened items, then every project.
async fn list(provider: &Provider) -> Result<()> {
    let all_projects = SearchOptions {
        match_all: Some(true),
        limit: Some(i64::MAX),
        ..wire_filter(&[SearchKind::Projects])
    };
    let lines = match client::connect(&provider.socket).await {
        Ok(client) => {
            let mut lines = Vec::new();
            for options in [SearchOptions::default(), all_projects] {
                // A daemon older than `match_all` refuses the second call;
                // the first one's rows still stand.
                match rows(&client, "", options).await {
                    Ok(rows) => lines.extend(rows.iter().map(entry)),
                    Err(e) => lines.push(notice(&format!("{e:#}"))),
                }
            }
            if lines.is_empty() {
                lines.push(notice("No projects or opened items cached yet"));
            }
            lines
        }
        Err(e) => vec![notice(&format!("{e:#}"))],
    };
    print(&lines)
}

/// What the user typed, read like the other launchers read it, but without
/// their trigger word and length gate: rofi only sends it on request.
async fn search(provider: &Provider, text: &str) -> Result<()> {
    let parsed = query::parse(text);
    let options = wire_filter(parsed.kinds().as_slice());
    let found = async {
        let client = client::connect(&provider.socket).await?;
        rows(&client, &parsed.query, options).await
    };
    let lines = match found.await {
        Ok(rows) if rows.is_empty() => vec![notice(&format!("No match for “{text}”"))],
        Ok(rows) => rows.iter().map(entry).collect(),
        Err(e) => vec![notice(&format!("{e:#}"))],
    };
    print(&lines)
}

async fn rows(client: &VarlinkClient, query: &str, options: SearchOptions) -> Result<Vec<Row>> {
    let reply = client
        .search(query.to_string(), Some(options))
        .call()
        .await
        .map_err(|e| friendly("Search", e))?;
    Ok(results::rows(&reply))
}

fn print(lines: &[String]) -> Result<()> {
    outln!("{HINT}")?;
    for line in lines {
        outln!("{line}")?;
    }
    Ok(())
}

/// A row: title and subtitle as its text, the avatar or kind icon, the result
/// id for `ROFI_INFO`.
fn entry(row: &Row) -> String {
    let text = if row.subtitle.is_empty() {
        clean(&row.title)
    } else {
        format!("{} · {}", clean(&row.title), clean(&row.subtitle))
    };
    format!(
        "{text}\0icon\x1f{}\x1finfo\x1f{}",
        clean(row.icon()),
        clean(&row.id)
    )
}

/// A row that only says something.
fn notice(text: &str) -> String {
    format!("{}\0nonselectable\x1ftrue", clean(text))
}

/// `\n` ends a row, `\0` and `\x1f` are the protocol's own; no control
/// character belongs in one.
fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(title: &str, subtitle: &str) -> Row {
        Row {
            id: "issues:7:42".into(),
            title: title.into(),
            subtitle: subtitle.into(),
            kind: SearchKind::Issues,
            score: 0,
            url: "https://gl.example.com/team/api/-/issues/42".into(),
            avatar: None,
        }
    }

    #[test]
    fn rows_carry_icon_and_result_id() {
        assert_eq!(
            entry(&row("#42 Fix login", "team/api · opened")),
            "#42 Fix login · team/api · opened\0icon\x1femblem-important-symbolic\x1finfo\x1fissues:7:42"
        );
        assert_eq!(
            entry(&row("team/api", "")),
            "team/api\0icon\x1femblem-important-symbolic\x1finfo\x1fissues:7:42"
        );
    }

    #[test]
    fn control_characters_never_reach_rofi() {
        let line = entry(&row("a\nb\0c\x1fd", "e\tf"));
        assert_eq!(line.matches('\0').count(), 1, "{line:?}");
        assert_eq!(line.matches('\x1f').count(), 3, "{line:?}");
        assert!(!line.contains('\n') && line.starts_with("a b c d · e f\0"));
        assert_eq!(
            notice("couldn't connect:\nrefused"),
            "couldn't connect: refused\0nonselectable\x1ftrue"
        );
    }

    #[test]
    fn retv_picks_what_to_do() {
        assert_eq!(ask(None), Ask::List);
        assert_eq!(ask(Some("0")), Ask::List);
        assert_eq!(ask(Some("1")), Ask::Open);
        assert_eq!(ask(Some("2")), Ask::Search);
        // Shift+Delete and custom keys aren't ours: stay open with the list.
        assert_eq!(ask(Some("3")), Ask::List);
        assert_eq!(ask(Some("10")), Ask::List);
    }

    #[test]
    fn the_hint_is_a_mode_option_in_valid_markup() {
        let (key, value) = HINT
            .strip_prefix('\0')
            .and_then(|o| o.split_once('\x1f'))
            .unwrap();
        assert_eq!(key, "message");
        assert!(!value.contains(['<', '>']), "{value}");
        let entity = |rest: &str| {
            ["&lt;", "&gt;", "&amp;"]
                .iter()
                .any(|e| rest.starts_with(e))
        };
        assert!(
            value.match_indices('&').all(|(i, _)| entity(&value[i..])),
            "{value}"
        );
    }
}
