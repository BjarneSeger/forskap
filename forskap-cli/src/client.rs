//! Thin wrapper around the generated varlink client.
//!
//! Exists so each subcommand doesn't have to repeat the socket-resolution and
//! `AsyncConnection::with_address` dance.

use std::sync::Arc;

use anyhow::{Context, Result};
use forskap_api::VarlinkClient;
use varlink::{AsyncConnection, AsyncMethodCall, ServiceInfo};

use crate::config::{self, Config};

/// Resolve the daemon's varlink socket address.
///
/// Precedence: `FORSKAPD_SOCKET` env var (or its pre-rename spelling
/// `GITLAB_TRACKRD_SOCKET`) -> the config file's `socket` -> the daemon's
/// default socket ([`forskap_api::default_socket`], which the daemon binds).
pub fn socket(cfg: &Config) -> Result<String> {
    let named = std::env::var("FORSKAPD_SOCKET")
        .or_else(|_| std::env::var("GITLAB_TRACKRD_SOCKET"))
        .ok()
        .or_else(|| cfg.socket.clone());
    if let Some(address) = named {
        return Ok(address);
    }
    let path = forskap_api::default_socket().context(
        "no home directory to look for the daemon's socket in; set FORSKAPD_SOCKET to its address",
    )?;
    Ok(format!("unix:{}", path.display()))
}

/// Open an async varlink connection to the daemon.
///
/// The connection is single-use per command invocation; we don't pool it
/// because the CLI exits right after the call returns.
pub async fn connect(socket: &str) -> Result<VarlinkClient> {
    let conn = open(socket)
        .await
        .with_context(|| format!("connecting to varlink socket {socket}"))?;
    Ok(VarlinkClient::new(conn))
}

/// [`connect`] to the socket the config and environment name.
pub async fn connect_default() -> Result<VarlinkClient> {
    connect(&socket(&config::load()?)?).await
}

/// The bare connection, for the calls outside the forskapd interface; hand
/// it to [`VarlinkClient::new`] for the others.
pub async fn open(socket: &str) -> varlink::Result<Arc<AsyncConnection>> {
    AsyncConnection::with_address(socket).await
}

/// `org.varlink.service.GetInfo`, which every varlink service answers: the
/// daemon's product name and version.
pub async fn service_info(conn: &Arc<AsyncConnection>) -> varlink::Result<ServiceInfo> {
    // An empty object: the crate's own `GetInfoArgs` is a unit struct and
    // would go out as `null`.
    let none = serde_json::Map::new();
    AsyncMethodCall::<_, ServiceInfo, varlink::Error>::new(
        Arc::clone(conn),
        "org.varlink.service.GetInfo",
        none,
    )
    .call()
    .await
}
