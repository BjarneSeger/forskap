//! Turn the daemon's `NotAuthenticated` varlink error into a friendly,
//! actionable message.
//!
//! The daemon ships a stable machine `reason` code (plus an optional `detail`)
//! on the error; the CLI owns the phrasing here. That split means wording can
//! change without a protocol bump, and an older daemon — which sends no reason
//! — still gets a sensible generic line.

use forskap_api::{ErrorKind, NotAuthReason, admin};

/// A failed call, of either interface's generated client.
pub trait DaemonError: std::fmt::Display {
    /// `NotAuthenticated`'s reason and detail; `None` for any other error.
    fn not_authenticated(&self) -> Option<(Option<NotAuthReason>, Option<&str>)>;

    /// The message of an error the daemon replied.
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
        let message = match self.kind() {
            ErrorKind::InvalidArgument(Some(args)) => &args.message,
            ErrorKind::NotFound(Some(args)) => &args.message,
            ErrorKind::GitlabError(Some(args)) => &args.message,
            ErrorKind::GitlabUnavailable(Some(args)) => &args.message,
            ErrorKind::Internal(Some(args)) => &args.message,
            _ => return None,
        };
        Some(message.clone())
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
        let message = match self.kind() {
            admin::ErrorKind::GitlabError(Some(args)) => &args.message,
            admin::ErrorKind::GitlabUnavailable(Some(args)) => &args.message,
            admin::ErrorKind::Internal(Some(args)) => &args.message,
            _ => return None,
        };
        Some(message.clone())
    }

    fn varlink_kind(&self) -> Option<&varlink::ErrorKind> {
        self.source_varlink_kind()
    }
}

/// Map a failed varlink call to an `anyhow::Error` with a user-facing message.
/// `op` is the method name used in the generic (non-auth) fallback, preserving
/// the previous `"<Method> failed: <error>"` output for every other error.
pub fn friendly(op: &str, e: impl DaemonError) -> anyhow::Error {
    if let Some((reason, detail)) = e.not_authenticated() {
        return anyhow::anyhow!("{}", message_for(reason, detail));
    }
    anyhow::anyhow!("{op} failed: {e}")
}

/// The message for a dormancy `reason` code, with `detail` appended in
/// parentheses when present. Unknown codes and a missing reason (older daemon)
/// fall back to the generic "run `forskap auth login`" line.
fn message_for(reason: Option<NotAuthReason>, detail: Option<&str>) -> String {
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
        Some(NotAuthReason::keychain_error) => "Run `forskap auth login` to store them again.",
    }
}

#[cfg(test)]
mod tests {
    use super::{NotAuthReason, message_for};

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
                "Couldn't read your saved credentials from the keychain. \
                 Run `forskap auth login` to store them again.",
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
