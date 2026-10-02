//! `forskap auth logout` — clear stored credentials and drop the daemon's GitLab
//! connection.

use anyhow::Result;
use forskap_api::admin::VarlinkClientInterface;

use crate::client;
use crate::friendly::friendly;

pub async fn run() -> Result<()> {
    let client = client::connect_admin().await?;
    client
        .logout()
        .call()
        .await
        .map_err(|e| friendly("Logout", e))?;
    outln!("Logged out.")?;
    Ok(())
}
