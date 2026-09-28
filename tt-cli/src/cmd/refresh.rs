//! `tt refresh` — drop the daemon's caches and re-fetch.
//!
//! With no flags this clears everything synced — use it after editing an issue
//! in the GitLab UI when you don't want to wait out the daemon's sync interval.
//! The per-slice flags clear only the named caches. The daemon replies once
//! what it cleared of the assigned lists and the history is re-synced.
//! `--usage` is the exception: open statistics are user data, so only that
//! explicit flag drops them.

use anyhow::Result;
use gitlab_trackr_api::VarlinkClientInterface;

use crate::{client, config};

pub async fn run(
    quick: bool,
    slow: bool,
    stale: bool,
    issues: bool,
    search: bool,
    usage: bool,
) -> Result<()> {
    // Collect the requested scopes. No flags ⇒ `None`, which the daemon reads
    // as "clear everything".
    let mut scope: Vec<String> = Vec::new();
    if quick {
        scope.push("quick".to_string());
    }
    if slow {
        scope.push("slow".to_string());
    }
    if stale {
        scope.push("stale".to_string());
    }
    if issues {
        scope.push("issues".to_string());
    }
    if search {
        scope.push("search".to_string());
    }
    if usage {
        scope.push("usage".to_string());
    }
    let scope = if scope.is_empty() { None } else { Some(scope) };

    let cfg = config::load()?;
    let socket = cfg.socket.unwrap_or_else(client::default_socket);
    let client = client::connect(&socket).await?;
    client
        .clear_cache(scope.clone())
        .call()
        .await
        .map_err(|e| anyhow::anyhow!("ClearCache failed: {e}"))?;
    match scope {
        None => println!("cache cleared"),
        Some(s) => println!("cleared: {}", s.join(", ")),
    }
    Ok(())
}
