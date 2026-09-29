//! `forskap sync refresh` — drop the daemon's caches and re-fetch.
//!
//! Use it after editing something in the GitLab UI when you don't want to wait
//! out the daemon's sync interval. The daemon replies once what it cleared of
//! the assigned lists and the history is re-synced.

use anyhow::Result;
use clap::ValueEnum;
use forskap_api::VarlinkClientInterface;

use crate::cli::RefreshScope;
use crate::client;
use crate::friendly::friendly;

/// The daemon's `ClearCache` scopes behind each CLI scope.
fn wire(scope: RefreshScope) -> &'static [&'static str] {
    match scope {
        RefreshScope::Assigned => &["issues"],
        RefreshScope::Search => &["search"],
        // The daemon syncs the history in three age bands.
        RefreshScope::History => &["quick", "slow", "stale"],
        RefreshScope::Usage => &["usage"],
    }
}

pub async fn run(mut scopes: Vec<RefreshScope>) -> Result<()> {
    scopes.dedup();
    // No scope ⇒ `None`, which the daemon reads as "everything synced".
    let scope = (!scopes.is_empty()).then(|| {
        scopes
            .iter()
            .flat_map(|s| wire(*s))
            .map(|s| s.to_string())
            .collect()
    });

    let client = client::connect_default().await?;
    client
        .clear_cache(scope)
        .call()
        .await
        .map_err(|e| friendly("ClearCache", e))?;

    if scopes.is_empty() {
        println!("cache cleared");
    } else {
        let names: Vec<_> = scopes
            .iter()
            .filter_map(|s| s.to_possible_value())
            .map(|v| v.get_name().to_string())
            .collect();
        println!("cleared: {}", names.join(", "));
    }
    Ok(())
}
