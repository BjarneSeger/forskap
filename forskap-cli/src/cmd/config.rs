//! `forskap config` — inspect/scaffold the user config file.
//!
//! The TOML template is generated from the [`crate::config::Config`] derive,
//! so the field list, defaults, and `///` doc comments come straight out of
//! the struct definition — there is no separate template to keep in sync.

use crate::cli::ConfigCommand;
use crate::config;

pub fn run(command: ConfigCommand) {
    match command {
        ConfigCommand::Template => {
            print!(
                "{}",
                confique::toml::template::<config::Config>(confique::toml::FormatOptions::default())
            );
        }
        ConfigCommand::Path => {
            println!("{}", config::config_path().display());
        }
    }
}
