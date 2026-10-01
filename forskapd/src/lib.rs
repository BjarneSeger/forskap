//! Library surface for `forskapd`.
//!
//! `main.rs` parses the arguments and runs [`daemon`]. This library target exists so the
//! crate's binaries (`forskapd`, `gen-config-template`) and the local
//! Criterion benches can link the daemon internals; it is **not** a public
//! API. Everything here is an implementation detail with no stability
//! guarantees — the crate is consumed as binaries only.

pub mod args;
pub mod config;
pub mod daemon;
pub mod db;
pub mod demo;
pub mod error;
pub mod gitlab;
pub mod handlers;
pub mod migrate;
pub mod query;
pub mod queue;
pub mod reconnect;
pub mod reload;
pub mod rotate;
pub mod secrets;
pub mod server;
pub mod service;
pub mod sync;
#[cfg(test)]
pub(crate) mod testing;
pub mod usage;
pub mod write;
