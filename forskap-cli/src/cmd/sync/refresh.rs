//! `forskap sync refresh` — drop the daemon's caches and re-fetch.
//!
//! Use it after editing something in the GitLab UI when you don't want to wait
//! out the daemon's sync interval. The daemon replies once what it cleared of
//! the assigned lists and the history is re-synced; on a terminal a line on
//! stderr follows that sync until then.

use std::convert::Infallible;
use std::io::{self, IsTerminal, Write};
use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use clap::ValueEnum;
use forskap_api::{CacheScope, VarlinkClientInterface};

use super::jobs;
use crate::cli::RefreshScope;
use crate::client;
use crate::friendly::friendly;
use crate::watch::{self, ERASE_LINE};

/// How often the sync is asked how far it is. The first look comes after
/// one such wait, so a quick refresh shows nothing.
const FOLLOW_EVERY: Duration = Duration::from_millis(500);

/// The daemon's `ClearCache` scopes behind each CLI scope.
fn wire(scope: RefreshScope) -> &'static [CacheScope] {
    match scope {
        RefreshScope::Assigned => &[CacheScope::assigned],
        RefreshScope::Search => &[CacheScope::search],
        // The daemon syncs the history in three age bands.
        RefreshScope::History => &[CacheScope::quick, CacheScope::slow, CacheScope::stale],
        RefreshScope::Usage => &[CacheScope::usage],
    }
}

pub async fn run(mut scopes: Vec<RefreshScope>) -> Result<()> {
    scopes.dedup();
    // No scope ⇒ `None`, which the daemon reads as "everything synced".
    let scope =
        (!scopes.is_empty()).then(|| scopes.iter().flat_map(|s| wire(*s)).cloned().collect());

    let client = client::connect_default().await?;
    let mut clear = client.clear_cache(scope);
    // The follow ends with the call, and takes its line with it.
    let cleared = tokio::select! {
        cleared = clear.call() => cleared,
        never = follow() => match never {},
    };
    cleared.map_err(|e| friendly("ClearCache", e))?;

    if scopes.is_empty() {
        outln!("cache cleared")?;
    } else {
        let names: Vec<_> = scopes
            .iter()
            .filter_map(|s| s.to_possible_value())
            .map(|v| v.get_name().to_string())
            .collect();
        outln!("cleared: {}", names.join(", "))?;
    }
    Ok(())
}

/// Show what the sync is at while the daemon holds its reply back. On its
/// own connection: the daemon answers one call at a time on each. Whatever
/// fails here is not the refresh failing, and stays silent.
async fn follow() -> Infallible {
    if !io::stderr().is_terminal() {
        std::future::pending::<()>().await;
    }
    let mut line = Line::default();
    loop {
        tokio::time::sleep(FOLLOW_EVERY).await;
        if let Ok(reply) = jobs::fetch().await {
            let now = Utc::now().timestamp();
            line.paint(&jobs::summary(&reply.jobs, reply.paused_until, now));
        }
    }
}

/// A status line on stderr, redrawn in place and gone when dropped.
#[derive(Default)]
struct Line {
    shown: bool,
}

impl Line {
    fn paint(&mut self, text: &str) {
        let cols = terminal_size::terminal_size_of(io::stderr()).map(|(w, _)| w.0.into());
        write(&frame(text, cols));
        self.shown = true;
    }
}

impl Drop for Line {
    fn drop(&mut self) {
        if self.shown {
            write(&frame("", None));
        }
    }
}

/// `text` drawn over the line the cursor is on, which it stays on.
fn frame(text: &str, cols: Option<usize>) -> String {
    let mut frame = format!("\r{ERASE_LINE}");
    // One column short: a line reaching the last one would wrap.
    let room = cols.map_or(usize::MAX, |cols| cols.saturating_sub(1));
    watch::cut(&mut frame, text, room);
    frame
}

// Not `eprint!`, which panics on a closed stderr.
fn write(frame: &str) {
    let mut stderr = io::stderr();
    let _ = stderr.write_all(frame.as_bytes());
    let _ = stderr.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_replaces_the_line_and_stays_on_it() {
        assert_eq!(frame("syncing events", None), "\r\x1b[Ksyncing events");
        assert_eq!(frame("syncing events", Some(8)), "\r\x1b[Ksyncing");
        assert_eq!(frame("", Some(80)), "\r\x1b[K");
    }
}
