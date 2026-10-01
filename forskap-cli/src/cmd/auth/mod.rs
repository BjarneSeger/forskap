//! `forskap auth` — the daemon's GitLab login.

mod login;
mod logout;
pub mod status;

use anyhow::Result;

use crate::cli::AuthCommand;

pub async fn run(command: AuthCommand) -> Result<()> {
    match command {
        AuthCommand::Login { host } => login::run(host).await,
        AuthCommand::Logout => logout::run().await,
        AuthCommand::Status { output } => status::run(output.output).await,
    }
}
