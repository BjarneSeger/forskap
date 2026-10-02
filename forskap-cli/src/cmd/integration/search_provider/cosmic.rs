//! pop-launcher's plugin protocol, which COSMIC's launcher searches through:
//! one JSON request per line on stdin, one JSON response per line on stdout.
//! The launcher runs the executable `plugin.ron` names without arguments —
//! the wrapper script `forskap integration search-provider install` writes —
//! keeps it running between queries and logs its stderr.
//!
//! The types mirror pop-launcher's `Request`, `PluginResponse` and
//! `PluginSearchResult`; serde's externally tagged form is the wire form. The
//! `pop-launcher` crate itself is a git dependency with its own runtime.

use std::io::BufRead as _;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::Provider;
use super::results::Row;

/// Ids are the indices of the rows the last `Search` appended.
#[derive(Debug, Deserialize, PartialEq)]
// Only `Search` and `Activate` carry anything we read.
#[allow(dead_code)]
enum Request {
    Activate(u32),
    ActivateContext { id: u32, context: u32 },
    Complete(u32),
    Context(u32),
    Exit,
    Close,
    Interrupt,
    Quit(u32),
    Search(String),
}

#[derive(Debug, Serialize, PartialEq)]
enum Response {
    Append(SearchResult),
    /// Hide the launcher: the pick is open.
    Close,
    /// Every row of the search is appended.
    Finished,
}

/// Every field is sent, `null` where unused, as pop-launcher's own plugins do.
#[derive(Debug, Serialize, PartialEq)]
struct SearchResult {
    id: u32,
    name: String,
    description: String,
    keywords: Option<Vec<String>>,
    icon: Option<Icon>,
    exec: Option<String>,
    window: Option<(u32, u32)>,
}

#[derive(Debug, Serialize, PartialEq)]
enum Icon {
    /// A themed icon name, or an absolute path the launcher loads the image from.
    Name(String),
}

pub(super) async fn serve(provider: Provider) -> Result<()> {
    let mut rows: Vec<Row> = Vec::new();
    // Nothing else runs in this process between requests, so waiting for the
    // next line on the runtime's one thread costs nothing.
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        let request = match serde_json::from_str::<Request>(&line) {
            Ok(request) => request,
            Err(e) => {
                eprintln!("forskap: ignoring launcher request {line:?}: {e}");
                continue;
            }
        };
        match request {
            Request::Search(text) => {
                rows = match provider.search(&text).await {
                    Ok(hits) => hits.rows,
                    // The launcher shows nothing for an error either; say why in its log.
                    Err(e) => {
                        eprintln!("forskap: search failed: {e:#}");
                        Vec::new()
                    }
                };
                for (id, row) in rows.iter().enumerate() {
                    send(&Response::Append(result(id, row)))?;
                }
                send(&Response::Finished)?;
            }
            Request::Activate(id) => {
                let Some(row) = rows.get(id as usize) else {
                    continue;
                };
                match provider.activate(&row.id).await {
                    Ok(()) => send(&Response::Close)?,
                    // Leave the launcher open so the failure is noticed.
                    Err(e) => eprintln!("forskap: opening {} failed: {e:#}", row.id),
                }
            }
            Request::Exit | Request::Close => return Ok(()),
            Request::ActivateContext { .. }
            | Request::Complete(_)
            | Request::Context(_)
            | Request::Interrupt
            | Request::Quit(_) => {}
        }
    }
    Ok(())
}

fn result(id: usize, row: &Row) -> SearchResult {
    SearchResult {
        id: u32::try_from(id).unwrap_or(u32::MAX),
        name: row.title.clone(),
        description: row.subtitle.clone(),
        keywords: None,
        icon: Some(Icon::Name(row.icon().to_string())),
        exec: None,
        window: None,
    }
}

/// One response per line; a closed stdout ends the process quietly like it
/// ends any other command.
fn send(response: &Response) -> Result<()> {
    outln!("{}", serde_json::to_string(response)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::SearchKind;

    fn parse(line: &str) -> Request {
        serde_json::from_str(line).unwrap_or_else(|e| panic!("{line}: {e}"))
    }

    #[test]
    fn requests_read_as_pop_launcher_writes_them() {
        assert_eq!(
            parse(r#"{"Search":"gl oauth"}"#),
            Request::Search("gl oauth".into())
        );
        assert_eq!(parse(r#"{"Activate":2}"#), Request::Activate(2));
        assert_eq!(
            parse(r#"{"ActivateContext":{"id":1,"context":0}}"#),
            Request::ActivateContext { id: 1, context: 0 }
        );
        assert_eq!(parse(r#""Exit""#), Request::Exit);
        assert_eq!(parse(r#""Interrupt""#), Request::Interrupt);
        assert!(serde_json::from_str::<Request>(r#"{"Launch":1}"#).is_err());
    }

    #[test]
    fn responses_write_as_pop_launcher_reads_them() {
        let row = Row {
            id: "issues:7:42".into(),
            title: "#42 Fix login".into(),
            subtitle: "team/api · opened".into(),
            kind: SearchKind::Issues,
            score: 3,
            url: "https://gl.example.com/team/api/-/issues/42".into(),
            avatar: None,
        };
        assert_eq!(
            serde_json::to_string(&Response::Append(result(0, &row))).unwrap(),
            r##"{"Append":{"id":0,"name":"#42 Fix login","description":"team/api · opened","keywords":null,"icon":{"Name":"emblem-important-symbolic"},"exec":null,"window":null}}"##
        );
        assert_eq!(
            serde_json::to_string(&Response::Finished).unwrap(),
            r#""Finished""#
        );
        assert_eq!(
            serde_json::to_string(&Response::Close).unwrap(),
            r#""Close""#
        );
    }
}
