//! Turn the daemon's varlink errors into messages a user reads.
//!
//! Every error of the interfaces carries a `message`, which is printed after
//! the method that failed. `NotAuthenticated` is the exception: the daemon
//! ships a stable machine `reason` code (plus an optional `detail`) on it, and
//! the CLI owns the phrasing here. That split means wording can change without
//! a protocol bump, and an older daemon — which sends no reason — still gets a
//! sensible generic line.

use forskap_api::{ErrorKind, NotAuthReason, admin};

/// A failed call, of either interface's generated client.
pub trait DaemonError: std::fmt::Display {
    /// `NotAuthenticated`'s reason and detail; `None` for any other error.
    fn not_authenticated(&self) -> Option<(Option<NotAuthReason>, Option<&str>)>;

    /// The message of an error the daemon replied, with the argument it
    /// refused or GitLab's HTTP status where it has one.
    fn message(&self) -> Option<String>;

    /// The varlink error under it: a broken connection, a reply outside the
    /// interface (`MethodNotFound`, `InvalidParameter`).
    fn varlink_kind(&self) -> Option<&varlink::ErrorKind>;
}

impl DaemonError for forskap_api::Error {
    fn not_authenticated(&self) -> Option<(Option<NotAuthReason>, Option<&str>)> {
        let ErrorKind::NotAuthenticated(args) = self.kind() else {
            return None;
        };
        let args = args.as_ref();
        let reason = args.and_then(|a| a.reason.clone());
        Some((reason, args.and_then(|a| a.detail.as_deref())))
    }

    fn message(&self) -> Option<String> {
        Some(match self.kind() {
            ErrorKind::InvalidArgument(Some(args)) => {
                format!("{} ({})", args.message, args.argument)
            }
            ErrorKind::GitlabError(Some(args)) => refused(&args.message, args.status),
            ErrorKind::NotFound(Some(args)) => args.message.clone(),
            ErrorKind::GitlabUnavailable(Some(args)) => args.message.clone(),
            ErrorKind::Internal(Some(args)) => args.message.clone(),
            _ => return None,
        })
    }

    fn varlink_kind(&self) -> Option<&varlink::ErrorKind> {
        self.source_varlink_kind()
    }
}

impl DaemonError for admin::Error {
    fn not_authenticated(&self) -> Option<(Option<NotAuthReason>, Option<&str>)> {
        None
    }

    fn message(&self) -> Option<String> {
        Some(match self.kind() {
            admin::ErrorKind::GitlabError(Some(args)) => refused(&args.message, args.status),
            admin::ErrorKind::GitlabUnavailable(Some(args)) => args.message.clone(),
            admin::ErrorKind::Internal(Some(args)) => args.message.clone(),
            _ => return None,
        })
    }

    fn varlink_kind(&self) -> Option<&varlink::ErrorKind> {
        self.source_varlink_kind()
    }
}

/// GitLab's refusal, with the HTTP status it answered where the daemon has
/// one: the message may not say it.
fn refused(message: &str, status: Option<i64>) -> String {
    match status {
        Some(status) => format!("{message} (HTTP {status})"),
        None => message.to_string(),
    }
}

/// Whether the daemon doesn't have the method: it is older than this forskap,
/// and a view that can do without the answer falls back to what it showed
/// before.
pub fn is_method_not_found(e: &impl DaemonError) -> bool {
    matches!(
        e.varlink_kind(),
        Some(varlink::ErrorKind::MethodNotFound(_))
    )
}

/// Map a failed varlink call to an `anyhow::Error` with a user-facing message:
/// `"<op> failed: <message>"`, `op` being the method's name, or for
/// `NotAuthenticated` what it means and what to do. An error the daemon
/// didn't reply (a broken connection) reads as the client describes it.
pub fn friendly(op: &str, e: impl DaemonError) -> anyhow::Error {
    if let Some((reason, detail)) = e.not_authenticated() {
        return anyhow::anyhow!("{}", message_for(reason, detail));
    }
    match (e.message(), e.varlink_kind()) {
        (Some(message), _) => anyhow::anyhow!("{op} failed: {message}"),
        // What a daemon older than this forskap answers for what it lacks.
        (
            None,
            Some(
                varlink::ErrorKind::InvalidParameter(name)
                | varlink::ErrorKind::MethodNotFound(name),
            ),
        ) => anyhow::anyhow!(
            "{op} failed: forskapd doesn't know {name}, so it is older than this forskap; \
             restart it (`forskap status` says how)"
        ),
        (None, _) => anyhow::anyhow!("{op} failed: {e}"),
    }
}

/// The message for a dormancy `reason` code, with `detail` appended in
/// parentheses when present. Unknown codes and a missing reason (older daemon)
/// fall back to the generic "run `forskap auth login`" line. Also for a view
/// that says beside its own content why there is no session.
pub fn message_for(reason: Option<NotAuthReason>, detail: Option<&str>) -> String {
    let base = format!("{} {}", problem(reason.as_ref()), remedy(reason.as_ref()));
    match detail {
        Some(d) if !d.is_empty() => format!("{base} ({d})"),
        _ => base,
    }
}

/// What a dormancy `reason` means for the user.
fn problem(reason: Option<&NotAuthReason>) -> &'static str {
    match reason {
        Some(NotAuthReason::no_credentials) | None => "Not connected to GitLab.",
        Some(NotAuthReason::token_rejected) => "GitLab rejected the stored token.",
        Some(NotAuthReason::unreachable) => "Can't reach GitLab — the daemon is not connected.",
        Some(NotAuthReason::keychain_error) => {
            "Couldn't read your saved credentials from the keychain."
        }
        Some(NotAuthReason::logged_out) => "Logged out.",
    }
}

/// What to do about a dormancy `reason`.
pub fn remedy(reason: Option<&NotAuthReason>) -> &'static str {
    match reason {
        Some(NotAuthReason::no_credentials | NotAuthReason::logged_out) | None => {
            "Run `forskap auth login` to authenticate."
        }
        Some(NotAuthReason::token_rejected) => "Run `forskap auth login` to re-authenticate.",
        Some(NotAuthReason::unreachable) => {
            "It retries automatically unless auto-reconnect is disabled; if so, \
             restart it once GitLab is reachable."
        }
        // The daemon waits for a locked keychain and says so in the detail;
        // any other failure to read it takes a new login.
        Some(NotAuthReason::keychain_error) => {
            "If it is locked, unlock it and the daemon connects by itself; otherwise run \
             `forskap auth login` to store them again."
        }
    }
}

#[cfg(test)]
mod tests {
    use forskap_api::{
        GitlabError_Args, GitlabUnavailable_Args, Internal_Args, InvalidArgument_Args,
        NotFound_Args,
    };

    use super::*;

    /// Each error reads as its message after the method, not as the
    /// generated client's dump; the argument or the status says more.
    #[test]
    fn a_daemon_error_reads_as_its_message() {
        let message = || "it went wrong".to_string();
        for (kind, shown) in [
            (
                ErrorKind::InvalidArgument(Some(InvalidArgument_Args {
                    argument: "options.limit".into(),
                    message: "invalid limit: 0".into(),
                })),
                "Search failed: invalid limit: 0 (options.limit)",
            ),
            (
                ErrorKind::GitlabError(Some(GitlabError_Args {
                    message: message(),
                    status: Some(403),
                })),
                "Search failed: it went wrong (HTTP 403)",
            ),
            (
                ErrorKind::GitlabError(Some(GitlabError_Args {
                    message: message(),
                    status: None,
                })),
                "Search failed: it went wrong",
            ),
            (
                ErrorKind::GitlabUnavailable(Some(GitlabUnavailable_Args { message: message() })),
                "Search failed: it went wrong",
            ),
            (
                ErrorKind::Internal(Some(Internal_Args { message: message() })),
                "Search failed: it went wrong",
            ),
            (
                ErrorKind::NotFound(Some(NotFound_Args { message: message() })),
                "Search failed: it went wrong",
            ),
        ] {
            let error = forskap_api::Error::from(kind);
            assert_eq!(friendly("Search", error).to_string(), shown);
        }
    }

    #[test]
    fn an_admin_error_reads_as_its_message() {
        for (kind, shown) in [
            (
                admin::ErrorKind::GitlabError(Some(admin::GitlabError_Args {
                    message: "GitLab rejected the token: invalid_token".into(),
                    status: Some(401),
                })),
                "Login failed: GitLab rejected the token: invalid_token (HTTP 401)",
            ),
            (
                admin::ErrorKind::GitlabUnavailable(Some(admin::GitlabUnavailable_Args {
                    message: "network error: connection refused".into(),
                })),
                "Login failed: network error: connection refused",
            ),
            (
                admin::ErrorKind::Internal(Some(admin::Internal_Args {
                    message: "logging in is disabled".into(),
                })),
                "Login failed: logging in is disabled",
            ),
        ] {
            let error = admin::Error::from(kind);
            assert_eq!(friendly("Login", error).to_string(), shown);
        }
    }

    #[test]
    fn an_older_daemon_reads_as_one() {
        let refused = varlink::ErrorKind::InvalidParameter("options.match_all".into());
        let error = forskap_api::Error::from(varlink::Error::from(refused));
        assert_eq!(
            friendly("Search", error).to_string(),
            "Search failed: forskapd doesn't know options.match_all, so it is older than this \
             forskap; restart it (`forskap status` says how)"
        );
    }

    /// One the daemon didn't reply keeps the client's words.
    #[test]
    fn a_failed_connection_reads_as_before() {
        let closed = varlink::Error::from(varlink::ErrorKind::ConnectionClosed);
        let error = forskap_api::Error::from(closed);
        let shown = friendly("Search", error).to_string();
        assert!(shown.starts_with("Search failed: "), "{shown}");
    }

    #[test]
    fn maps_each_known_reason() {
        assert!(
            message_for(Some(NotAuthReason::no_credentials), None).contains("forskap auth login")
        );
        assert!(message_for(Some(NotAuthReason::token_rejected), None).contains("rejected"));
        assert!(message_for(Some(NotAuthReason::unreachable), None).contains("reach GitLab"));
        assert!(message_for(Some(NotAuthReason::keychain_error), None).contains("keychain"));
        assert!(message_for(Some(NotAuthReason::logged_out), None).contains("Logged out"));
    }

    #[test]
    fn each_reason_reads_as_problem_then_remedy() {
        for (reason, message) in [
            (
                NotAuthReason::no_credentials,
                "Not connected to GitLab. Run `forskap auth login` to authenticate.",
            ),
            (
                NotAuthReason::token_rejected,
                "GitLab rejected the stored token. Run `forskap auth login` to re-authenticate.",
            ),
            (
                NotAuthReason::unreachable,
                "Can't reach GitLab — the daemon is not connected. It retries automatically \
                 unless auto-reconnect is disabled; if so, restart it once GitLab is reachable.",
            ),
            (
                NotAuthReason::keychain_error,
                "Couldn't read your saved credentials from the keychain. If it is locked, \
                 unlock it and the daemon connects by itself; otherwise run \
                 `forskap auth login` to store them again.",
            ),
            (
                NotAuthReason::logged_out,
                "Logged out. Run `forskap auth login` to authenticate.",
            ),
        ] {
            assert_eq!(message_for(Some(reason), None), message);
        }
    }

    #[test]
    fn missing_reason_falls_back() {
        // A daemon predating the `reason` field sends it absent (`None`); the
        // enum type makes an *unknown* code unrepresentable.
        let fallback = "Not connected to GitLab. Run `forskap auth login` to authenticate.";
        assert_eq!(message_for(None, None), fallback);
    }

    #[test]
    fn detail_is_appended_when_present() {
        let m = message_for(
            Some(NotAuthReason::unreachable),
            Some("gitlab.example.com: connection refused"),
        );
        assert!(m.ends_with("(gitlab.example.com: connection refused)"));
        // An empty detail is ignored rather than rendered as "()".
        assert!(!message_for(Some(NotAuthReason::unreachable), Some("")).ends_with("()"));
    }
}
