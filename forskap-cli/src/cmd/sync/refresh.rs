//! `forskap sync refresh` — drop the daemon's caches and re-fetch.
//!
//! Use it after editing something in the GitLab UI when you don't want to wait
//! out the daemon's sync interval. The daemon replies once what it cleared of
//! the assigned lists and the history is re-synced.

use anyhow::Result;
use clap::ValueEnum;
use forskap_api::{CacheScope, VarlinkClientInterface};

use crate::cli::RefreshScope;
use crate::client;
use crate::friendly::friendly;

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
    client
        .clear_cache(scope)
        .call()
        .await
        .map_err(|e| friendly("ClearCache", e))?;

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
