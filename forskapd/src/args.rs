//! The daemon's command line. `main` parses it before anything else happens,
//! so `--version`, `--help` and a mistyped flag exit without starting a
//! daemon, reading a keychain or touching a file.

use clap::Parser;

/// The caching GitLab daemon behind `forskap`.
///
/// forskapd syncs your GitLab issues, merge requests, epics, time logs and
/// activity into a local cache and serves them, and the writes to them, on a
/// varlink Unix socket. It is normally started without arguments by its
/// systemd user unit (`systemctl enable --now --user forskapd.socket`), or
/// on macOS by launchd (`brew services start forskap`).
#[derive(Debug, Parser)]
#[command(name = "forskapd", version, max_term_width = 100, after_long_help = AFTER_LONG_HELP)]
pub struct Args {
    /// Listen on this Unix socket.
    ///
    /// Takes precedence over `[server] socket` of the config. Under systemd
    /// socket activation the socket systemd passes is used, as always.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<String>,
}

/// The environment and files, below the options of `--help`.
const AFTER_LONG_HELP: &str = "\
Environment:
  FORSKAPD_LOG  What to log: trace, debug, info, warn or error, or a
                tracing filter such as `forskapd=debug` (default:
                forskapd=info).

Files (Linux; on macOS the config and the database are under
~/Library/Application Support, the avatars under ~/Library/Caches and the
socket is /tmp/forskapd.socket):
  $XDG_CONFIG_HOME/forskapd/config.toml  Your config, re-read when it changes.
  /usr/share/forskapd/config.toml        The package's defaults.
  $XDG_DATA_HOME/forskapd/db             The cache, the retry queue, the open
                                         counts.
  $XDG_CACHE_HOME/forskapd/avatars       The project avatars.
  $XDG_RUNTIME_DIR/forskapd.socket       The default socket.

The GitLab token lives in the OS keychain, never in a file: `forskap auth
login` stores it there.";

#[cfg(test)]
mod tests {
    use clap::error::ErrorKind;

    use super::*;

    fn parse(args: &[&str]) -> Result<Args, clap::Error> {
        Args::try_parse_from(std::iter::once("forskapd").chain(args.iter().copied()))
    }

    #[test]
    fn no_arguments_run_the_daemon_as_it_always_ran() {
        let args = parse(&[]).unwrap();
        assert_eq!(args.socket, None);
    }

    #[test]
    fn version_prints_the_name_and_version_and_starts_nothing() {
        for flag in ["--version", "-V"] {
            let e = parse(&[flag]).unwrap_err();
            assert_eq!(e.kind(), ErrorKind::DisplayVersion, "{flag}");
            assert_eq!(e.exit_code(), 0);
            let expected = format!("forskapd {}\n", env!("CARGO_PKG_VERSION"));
            assert_eq!(e.render().to_string(), expected);
        }
    }

    #[test]
    fn help_describes_the_daemon() {
        let e = parse(&["--help"]).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::DisplayHelp);
        assert_eq!(e.exit_code(), 0);
        let help = e.render().to_string();
        for needle in [
            "systemd user unit",
            "brew services",
            "FORSKAPD_LOG",
            "config.toml",
            "--socket <PATH>",
            "--version",
            "keychain",
        ] {
            assert!(help.contains(needle), "{needle:?} missing from:\n{help}");
        }
    }

    #[test]
    fn an_unknown_flag_is_a_usage_error() {
        let e = parse(&["--frobnicate"]).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::UnknownArgument);
        assert_eq!(e.exit_code(), 2);
        // A stray word too: the daemon takes no positional arguments.
        let e = parse(&["start"]).unwrap_err();
        assert_eq!(e.exit_code(), 2);
    }

    #[test]
    fn socket_takes_a_path() {
        let args = parse(&["--socket", "/run/user/1000/other.socket"]).unwrap();
        assert_eq!(args.socket.as_deref(), Some("/run/user/1000/other.socket"));
        let e = parse(&["--socket"]).unwrap_err();
        assert_eq!(e.exit_code(), 2, "a path is required");
    }
}
