//! User configuration loaded from `$XDG_CONFIG_HOME/gitlab-trackr-cli/config.toml`.
//!
//! Schema and defaults are owned by the [`Config`] derive — run
//! `tt config template` to print an annotated TOML file with the current
//! defaults and doc comments inline.

use std::path::PathBuf;

use anyhow::Result;
use confique::Config as ConfiqueConfig;

/// `tt` user configuration.
///
/// Every field is optional in the TOML file; missing keys fall back to the
/// `#[config(default = ...)]` value declared on the field.
#[derive(Debug, ConfiqueConfig)]
pub struct Config {
    /// Minimum minutes between two interactive prompts triggered by `tt tick`.
    #[config(default = 30)]
    pub interval_minutes: u64,

    /// Fallback duration string shown in the interactive prompt when the
    /// elapsed time can't be measured (e.g. on the very first tick after
    /// install). Must be a GitLab time-tracking string like `"30m"` or
    /// `"1h15m"`.
    #[config(default = "30m")]
    pub default_duration: String,

    /// Override the daemon's varlink socket address; the
    /// `GITLAB_TRACKRD_SOCKET` env var wins over it. If neither is set, the
    /// same XDG-runtime-dir fallback chain as the daemon is used.
    pub socket: Option<String>,

    // Read only by the Linux-only `tt integration search-provider`.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    #[config(nested)]
    pub search_provider: SearchProvider,
}

/// Settings for `tt integration search-provider`, the GNOME Shell / KRunner integration.
#[derive(Debug, ConfiqueConfig)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct SearchProvider {
    /// Only answer desktop searches that start with this word, e.g. `gl` makes
    /// `gl oauth` search for "oauth" and `gl` alone list frequently opened
    /// items, while any other search returns nothing. Unset, every search is
    /// answered (queries shorter than two characters are ignored). KRunner
    /// reads the word once at startup, so restart it after changing this.
    pub trigger_word: Option<String>,
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("gitlab-trackr-cli/config.toml")
}

/// Load the config: file values fill in, then `#[config(default)]` plugs the rest.
///
/// A missing config file is fine — confique treats it as an empty layer. Parse
/// errors and missing required (non-default) fields propagate.
pub fn load() -> Result<Config> {
    Ok(Config::builder().file(config_path()).load()?)
}
