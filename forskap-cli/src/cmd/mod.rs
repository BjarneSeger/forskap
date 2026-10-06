//! Mirrors the command tree: one module per group, one per subcommand inside
//! it. Each group exposes a `run(...)` that [`crate::main`] dispatches to.

pub mod activity;
pub mod auth;
pub mod config;
pub mod epic;
#[cfg(target_os = "linux")]
pub mod integration;
pub mod item;
pub mod project;
pub mod queue;
pub mod search;
pub mod status;
pub mod sync;
pub mod time;

use anyhow::Result;

/// A labelled line of a view, left out while its value is empty.
pub fn field(name: &str, value: &str) -> Result<()> {
    if !value.is_empty() {
        outln!("  {:<11} {value}", crate::style::muted(&format!("{name}:")))?;
    }
    Ok(())
}
