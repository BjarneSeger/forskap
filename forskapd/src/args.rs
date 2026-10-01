//! The daemon's command line. `main` parses it before anything else happens,
//! so `--version`, `--help` and a mistyped flag exit without starting a
//! daemon, reading a keychain or touching a file.

use std::path::Path;

use clap::error::ErrorKind;
use clap::{CommandFactory, Parser};

use crate::config::ServerConfig;

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
    /// Serve a built-in demo account, to try clients against.
    ///
    /// Everything it keeps lives in a new private temporary directory that
    /// is removed on exit (SIGINT and SIGTERM included): its database, its
    /// avatars, its socket. It never touches the keychain, GitLab or the
    /// network, nor the config file, database, cache or socket of the real
    /// daemon. Writes (time logged, items closed, assigned or created)
    /// change the demo only; logging in or out is turned down. Once the demo
    /// is synced, the first line on stdout is the socket's address as
    /// `FORSKAPD_SOCKET` takes it (`unix:/tmp/forskapd-dry-run.…/forskapd.socket`);
    /// the log goes to stderr.
    #[arg(long)]
    pub dry_run: bool,

    /// Listen on this Unix socket.
    ///
    /// Takes precedence over `[server] socket` of the config. Under systemd
    /// socket activation the socket systemd passes is used, as always. With
    /// `--dry-run` it replaces the socket in the temporary directory; it must
    /// not exist yet and can't be the daemon's default socket.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<String>,
}

impl Args {
    /// [`Parser::parse`], plus the checks clap can't make alone. Exits on
    /// `--help`, `--version` and a usage error (with code 2).
    pub fn parse_checked() -> Self {
        let args = Self::parse();
        if let Err(e) = args.check() {
            e.exit();
        }
        args
    }

    /// A dry run must not take the socket the real daemon listens on by
    /// default: clients would talk to the demo believing it is the real
    /// one, and the real daemon couldn't start. The config may name another
    /// socket, but a dry run doesn't read the config; that one exists while
    /// the daemon runs, and binding an existing path fails.
    fn check(&self) -> Result<(), clap::Error> {
        if self.dry_run
            && let Some(socket) = &self.socket
            && is_default_socket(socket)
        {
            let msg = format!(
                "--dry-run can't take the daemon's default socket {socket}; \
                 name another path or leave --socket out"
            );
            return Err(Self::command().error(ErrorKind::ArgumentConflict, msg));
        }
        Ok(())
    }
}

fn is_default_socket(socket: &str) -> bool {
    let default = ServerConfig { socket: None }.resolved_socket();
    let absolute = |p: &str| std::path::absolute(p).unwrap_or_else(|_| Path::new(p).into());
    absolute(socket) == absolute(&default)
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
    use super::*;

    fn parse(args: &[&str]) -> Result<Args, clap::Error> {
        Args::try_parse_from(std::iter::once("forskapd").chain(args.iter().copied()))
    }

    #[test]
    fn no_arguments_run_the_daemon_as_it_always_ran() {
        let args = parse(&[]).unwrap();
        assert!(!args.dry_run);
        assert_eq!(args.socket, None);
        assert!(args.check().is_ok());
    }

    #[test]
    fn dry_run_is_a_flag() {
        let args = parse(&["--dry-run"]).unwrap();
        assert!(args.dry_run);
        assert_eq!(args.socket, None);
        assert!(args.check().is_ok());
        let e = parse(&["--dry-run=yes"]).unwrap_err();
        assert_eq!(e.exit_code(), 2, "it takes no value");
    }

    #[test]
    fn a_dry_run_takes_any_socket_but_the_default_one() {
        let args = parse(&["--dry-run", "--socket", "/tmp/demo.socket"]).unwrap();
        assert_eq!(args.socket.as_deref(), Some("/tmp/demo.socket"));
        assert!(args.check().is_ok());

        let default = ServerConfig { socket: None }.resolved_socket();
        let args = parse(&["--dry-run", "--socket", &default]).unwrap();
        let e = args.check().unwrap_err();
        assert_eq!(e.kind(), ErrorKind::ArgumentConflict);
        assert_eq!(e.exit_code(), 2);
        // The real daemon may be pointed at it, of course.
        let args = parse(&["--socket", &default]).unwrap();
        assert!(args.check().is_ok());
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
            "--dry-run",
            "FORSKAPD_SOCKET",
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
