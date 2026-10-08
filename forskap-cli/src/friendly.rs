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
    /// `NotAuthenticated`'s reason, detail and whether the daemon gets a
    /// session by itself; `None` for any other error.
    fn not_authenticated(&self) -> Option<NotAuth<'_>>;

    /// The message of an error the daemon replied, with the argument it
    /// refused or GitLab's HTTP status where it has one.
    fn message(&self) -> Option<String>;

    /// The varlink error under it: a broken connection, a reply outside the
    /// interface (`MethodNotFound`, `InvalidParameter`).
    fn varlink_kind(&self) -> Option<&varlink::ErrorKind>;
}

impl DaemonError for forskap_api::Error {
    fn not_authenticated(&self) -> Option<NotAuth<'_>> {
        let ErrorKind::NotAuthenticated(args) = self.kind() else {
            return None;
        };
        let args = args.as_ref();
        Some(NotAuth {
            reason: args.and_then(|a| a.reason.clone()),
            detail: args.and_then(|a| a.detail.as_deref()),
            retrying: args.and_then(|a| a.retrying),
        })
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
    fn not_authenticated(&self) -> Option<NotAuth<'_>> {
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
    if let Some(why) = e.not_authenticated() {
        return anyhow::anyhow!("{}", message_for(why.reason, why.detail, why.retrying));
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

/// Why the daemon has no session, as a `NotAuthenticated` reply carries it.
pub struct NotAuth<'a> {
    pub reason: Option<NotAuthReason>,
    pub detail: Option<&'a str>,
    /// Whether the daemon gets a session by itself; `None` from a daemon
    /// before 1.3, which doesn't say.
    pub retrying: Option<bool>,
}

/// The message for a dormancy `reason` code, with `detail` appended in
/// parentheses when present. Unknown codes and a missing reason (older daemon)
/// fall back to the generic "run `forskap auth login`" line. `retrying` is the
/// daemon's word on whether it gets a session by itself: wait, or act. Also
/// for a view that says beside its own content why there is no session.
pub fn message_for(
    reason: Option<NotAuthReason>,
    detail: Option<&str>,
    retrying: Option<bool>,
) -> String {
    let reason = reason.as_ref();
    let base = format!("{} {}", problem(reason, retrying), remedy(reason, retrying));
    // A keychain the daemon waits for: its detail says just that again.
    let detail = detail.filter(|_| !locked(reason, retrying));
    match detail {
        Some(d) if !d.is_empty() => format!("{base} ({d})"),
        _ => base,
    }
}

/// Whether the dormancy is a locked keychain: a keychain error the daemon
/// gets over by itself is that and nothing else, an unreadable keychain
/// takes a login.
pub fn locked(reason: Option<&NotAuthReason>, retrying: Option<bool>) -> bool {
    reason == Some(&NotAuthReason::keychain_error) && retrying == Some(true)
}

/// What a dormancy `reason` means for the user.
fn problem(reason: Option<&NotAuthReason>, retrying: Option<bool>) -> &'static str {
    match reason {
        Some(NotAuthReason::no_credentials) | None => "Not connected to GitLab.",
        Some(NotAuthReason::token_rejected) => "GitLab rejected the stored token.",
        Some(NotAuthReason::unreachable) => "Can't reach GitLab — the daemon is not connected.",
        Some(NotAuthReason::keychain_error) if locked(reason, retrying) => {
            "The keychain holding your credentials is locked."
        }
        Some(NotAuthReason::keychain_error) => {
            "Couldn't read your saved credentials from the keychain."
        }
        Some(NotAuthReason::logged_out) => "Logged out.",
    }
}

/// What to do about a dormancy `reason`: nothing but wait where the daemon
/// says it is `retrying`.
pub fn remedy(reason: Option<&NotAuthReason>, retrying: Option<bool>) -> &'static str {
    match (reason, retrying) {
        (Some(NotAuthReason::no_credentials | NotAuthReason::logged_out) | None, _) => {
            "Run `forskap auth login` to authenticate."
        }
        (Some(NotAuthReason::token_rejected), _) => "Run `forskap auth login` to re-authenticate.",
        (Some(NotAuthReason::unreachable), Some(true)) => "It reconnects by itself.",
        (Some(NotAuthReason::unreachable), Some(false)) => {
            "Auto-reconnect is off: restart the daemon once GitLab is reachable."
        }
        // A daemon too old to say whether it retries.
        (Some(NotAuthReason::unreachable), None) => {
            "It retries automatically unless auto-reconnect is disabled; if so, \
             restart it once GitLab is reachable."
        }
        (Some(NotAuthReason::keychain_error), Some(true)) => {
            "The daemon connects by itself once it is unlocked."
        }
        (Some(NotAuthReason::keychain_error), Some(false)) => {
            "Run `forskap auth login` to store them again."
        }
        // A daemon before 1.3 waits for a locked keychain without saying so
        // here (its detail does); one before that never did.
        (Some(NotAuthReason::keychain_error), None) => {
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
            message_for(Some(NotAuthReason::no_credentials), None, None)
                .contains("forskap auth login")
        );
        assert!(message_for(Some(NotAuthReason::token_rejected), None, None).contains("rejected"));
        assert!(message_for(Some(NotAuthReason::unreachable), None, None).contains("reach GitLab"));
        assert!(message_for(Some(NotAuthReason::keychain_error), None, None).contains("keychain"));
        assert!(message_for(Some(NotAuthReason::logged_out), None, None).contains("Logged out"));
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
            assert_eq!(message_for(Some(reason), None, None), message);
        }
    }

    /// A daemon of 1.3 says whether it gets a session by itself, and the
    /// message says wait or act instead of hedging.
    #[test]
    fn a_daemon_that_says_whether_it_retries_gets_a_plain_answer() {
        let locked = "it is locked; the daemon connects by itself once it is unlocked";
        for (reason, detail, retrying, message) in [
            (
                NotAuthReason::unreachable,
                Some("gitlab.example.com: timed out"),
                true,
                "Can't reach GitLab — the daemon is not connected. It reconnects by itself. \
                 (gitlab.example.com: timed out)",
            ),
            (
                NotAuthReason::unreachable,
                None,
                false,
                "Can't reach GitLab — the daemon is not connected. Auto-reconnect is off: \
                 restart the daemon once GitLab is reachable.",
            ),
            // The detail of a locked keychain says what the message does.
            (
                NotAuthReason::keychain_error,
                Some(locked),
                true,
                "The keychain holding your credentials is locked. The daemon connects by \
                 itself once it is unlocked.",
            ),
            (
                NotAuthReason::keychain_error,
                Some("secret store: no such service"),
                false,
                "Couldn't read your saved credentials from the keychain. Run \
                 `forskap auth login` to store them again. (secret store: no such service)",
            ),
            // What takes a login reads the same either way.
            (
                NotAuthReason::logged_out,
                None,
                false,
                "Logged out. Run `forskap auth login` to authenticate.",
            ),
        ] {
            assert_eq!(message_for(Some(reason), detail, Some(retrying)), message);
        }
    }

    #[test]
    fn missing_reason_falls_back() {
        // A daemon predating the `reason` field sends it absent (`None`); the
        // enum type makes an *unknown* code unrepresentable.
        let fallback = "Not connected to GitLab. Run `forskap auth login` to authenticate.";
        assert_eq!(message_for(None, None, None), fallback);
    }

    #[test]
    fn detail_is_appended_when_present() {
        let m = message_for(
            Some(NotAuthReason::unreachable),
            Some("gitlab.example.com: connection refused"),
            None,
        );
        assert!(m.ends_with("(gitlab.example.com: connection refused)"));
        // An empty detail is ignored rather than rendered as "()".
        assert!(!message_for(Some(NotAuthReason::unreachable), Some(""), None).ends_with("()"));
    }
}
