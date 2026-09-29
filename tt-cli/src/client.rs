//! Thin wrapper around the generated varlink client.
//!
//! Exists so each subcommand doesn't have to repeat the socket-resolution and
//! `AsyncConnection::with_address` dance.

use anyhow::{Context, Result};
use gitlab_trackr_api::VarlinkClient;
use varlink::AsyncConnection;

use crate::config::{self, Config};

/// Resolve the daemon's varlink socket address.
///
/// Precedence: `GITLAB_TRACKRD_SOCKET` env var -> the config file's `socket`
/// -> `unix:$XDG_RUNTIME_DIR/gitlab-trackrd.socket` ->
/// `unix:/tmp/gitlab-trackrd.socket`. **The defaults must stay in sync with the
/// daemon's own resolution in `gitlab-trackrd/src/main.rs`**, or `tt` will
/// silently miss the running daemon.
pub fn socket(cfg: &Config) -> String {
    std::env::var("GITLAB_TRACKRD_SOCKET")
        .ok()
        .or_else(|| cfg.socket.clone())
        .unwrap_or_else(|| {
            std::env::var("XDG_RUNTIME_DIR")
                .map(|d| format!("unix:{d}/gitlab-trackrd.socket"))
                .unwrap_or_else(|_| "unix:/tmp/gitlab-trackrd.socket".to_string())
        })
}

/// Open an async varlink connection to the daemon.
///
/// The connection is single-use per command invocation; we don't pool it
/// because the CLI exits right after the call returns.
pub async fn connect(socket: &str) -> Result<VarlinkClient> {
    let conn = AsyncConnection::with_address(socket)
        .await
        .with_context(|| format!("connecting to varlink socket {socket}"))?;
    Ok(VarlinkClient::new(conn))
}

/// [`connect`] to the socket the config and environment name.
pub async fn connect_default() -> Result<VarlinkClient> {
    connect(&socket(&config::load()?)).await
}
