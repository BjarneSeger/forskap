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
