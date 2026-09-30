//! `forskap config` — inspect/scaffold the user config file.
//!
//! The TOML template is generated from the [`crate::config::Config`] derive,
//! so the field list, defaults, and `///` doc comments come straight out of
//! the struct definition — there is no separate template to keep in sync.

use anyhow::Result;

use crate::cli::ConfigCommand;
use crate::config;

pub fn run(command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::Template => {
            out!(
                "{}",
                confique::toml::template::<config::Config>(confique::toml::FormatOptions::default())
            )
        }
        ConfigCommand::Path => outln!("{}", config::config_path().display()),
    }
}
