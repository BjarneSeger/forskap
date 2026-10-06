//! `forskap auth login` — interactive GitLab authentication.
//!
//! Opens the GitLab PAT creation page with sensible scope hints, prompts for
//! the resulting token, and hands it to the daemon which validates it,
//! persists it to the OS keychain, and connects. The token never touches a
//! file on disk on the CLI side.

use anyhow::{Context, Result};
use forskap_api::VarlinkClientInterface;
use forskap_api::admin::VarlinkClientInterface as _;
use inquire::Password;

use crate::client;
use crate::friendly::friendly;

pub async fn run(host: String) -> Result<()> {
    let url = format!(
        "https://{host}/-/user_settings/personal_access_tokens?name=forskapd&scopes=api,read_user,self_rotate"
    );

    outln!("Opening {url}")?;
    outln!(
        "Generate a token with the `api`, `read_user` and `self_rotate` scopes, then paste it below."
    )?;
    outln!("With `self_rotate` the daemon renews the token before it expires.")?;
    if let Err(e) = open::that(&url) {
        eprintln!("(couldn't open browser automatically: {e})");
        eprintln!("Open the URL above manually.");
    }

    // inquire's prompt is synchronous, blocking terminal I/O; run it off the
    // single-threaded async executor so the runtime isn't blocked while the
    // user pastes their token.
    let token = tokio::task::spawn_blocking(|| {
        Password::new("Paste the personal access token:")
            .without_confirmation()
            .with_display_mode(inquire::PasswordDisplayMode::Masked)
            .prompt()
            .context("reading token from stdin")
    })
    .await??;
    let token = token.trim().to_string();
    if token.is_empty() {
        anyhow::bail!("no token entered; aborting");
    }

    let (client, admin) = client::connect_both().await?;

    admin
        .login(host.clone(), token)
        .call()
        .await
        .map_err(|e| friendly("Login", e))?;

    let me = client
        .who_am_i()
        .call()
        .await
        .map_err(|e| friendly("WhoAmI", e))?;
    outln!(
        "{}",
        super::status::logged_in(&me.host, &me.username, me.user_id)
    )?;
    Ok(())
}
