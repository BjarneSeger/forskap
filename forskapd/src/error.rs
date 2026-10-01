//! Daemon-wide error type.
//!
//! Each `#[from]`-annotated variant lets `thiserror` derive the matching
//! `From` impl, so call sites use `?` instead of `.map_err(...)` chains.

use forskap_api::NotAuthReason;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// GitLab failed the request without an HTTP status to tell why (an
    /// unreadable answer, a GraphQL error, …), or the daemon refused it
    /// itself.
    #[error("GitLab error: {0}")]
    Gitlab(String),

    /// GitLab answered with a status that is no 401, 429 or 5xx: it won't do
    /// this as asked (403, 404, 400, 422, …). Permanent like [`Self::Gitlab`];
    /// the status tells a refused listing (403/404) from a bad request.
    #[error("GitLab error: {detail}")]
    Rejected { status: u16, detail: String },

    /// GitLab answered 401: the token is dead, and only `forskap auth login` helps.
    #[error("GitLab rejected the token: {0}")]
    Unauthorized(String),

    /// A rotation GitLab may have carried out, whose answer was unusable:
    /// the token may be revoked with its successor unknown.
    #[error("token rotation answer unusable: {0}")]
    RotationLost(String),

    /// Transient network error — safe to retry.
    #[error("network error: {0}")]
    Transient(String),

    /// GitLab answered 429 or 5xx: the request may succeed later, and the
    /// session itself is fine.
    #[error("GitLab unavailable ({status}): {detail}")]
    Throttled {
        status: u16,
        /// The server's `Retry-After`, when it sent one.
        retry_after: Option<std::time::Duration>,
        detail: String,
    },

    #[error("secret store: {0}")]
    Secrets(String),

    /// The daemon runs without a keychain (a dry run).
    #[error("this forskapd is a dry run and has no keychain")]
    NoKeychain,

    #[error(transparent)]
    Fjall(#[from] fjall::Error),

    #[error("db: {0}")]
    Db(&'static str),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Varlink error: {0}")]
    Varlink(#[from] varlink::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// Whether the same request may succeed if repeated later. A 5xx counts
    /// only for `idempotent` requests: the server may have applied a
    /// non-idempotent write before failing.
    pub fn is_retryable(&self, idempotent: bool) -> bool {
        match self {
            Self::Transient(_) => true,
            Self::Throttled { status, .. } => *status == 429 || idempotent,
            _ => false,
        }
    }

    /// The server-requested pause before retrying, if any.
    pub fn retry_after(&self) -> Option<std::time::Duration> {
        match self {
            Self::Throttled { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

/// Why the daemon has no live GitLab session. Attached to
/// [`Error::NotAuthenticated`] so the reason the daemon already logs at startup
/// can reach the CLI. The daemon owns the *cause* (a stable machine `code` plus
/// optional free-text `detail`); the CLI owns the *phrasing*.
#[derive(Debug, Clone)]
pub enum DormancyReason {
    /// No credentials are stored in the keychain (never logged in / logged out).
    NoCredentials,
    /// Reading the OS keychain failed.
    KeychainError(String),
    /// Credentials exist but GitLab was unreachable (network / transient error).
    Unreachable { host: String, detail: String },
    /// Credentials exist but GitLab rejected the token (auth failure).
    TokenRejected { host: String, detail: String },
    /// The user explicitly logged out.
    LoggedOut,
}

impl DormancyReason {
    /// The wire enum sent to the CLI. The CLI maps it to a human message, so
    /// wording can change without a protocol change; adding a variant is a
    /// protocol change the CLI is forced to handle (exhaustive match).
    pub fn reason(&self) -> NotAuthReason {
        match self {
            Self::NoCredentials => NotAuthReason::no_credentials,
            Self::KeychainError(_) => NotAuthReason::keychain_error,
            Self::Unreachable { .. } => NotAuthReason::unreachable,
            Self::TokenRejected { .. } => NotAuthReason::token_rejected,
            Self::LoggedOut => NotAuthReason::logged_out,
        }
    }

    /// Free-text detail (host + underlying error) for the codes that carry one.
    pub fn detail(&self) -> Option<String> {
        match self {
            Self::NoCredentials | Self::LoggedOut => None,
            Self::KeychainError(d) => Some(d.clone()),
            Self::Unreachable { host, detail } | Self::TokenRejected { host, detail } => {
                Some(format!("{host}: {detail}"))
            }
        }
    }

    /// Classify a failed `GitlabClient::connect`: a network error or a
    /// 429/5xx means GitLab was unreachable; anything else means the token was
    /// rejected.
    pub fn from_connect_error(host: &str, e: &Error) -> Self {
        match e {
            Error::Transient(detail) => Self::Unreachable {
                host: host.to_string(),
                detail: detail.clone(),
            },
            e @ Error::Throttled { .. } => Self::Unreachable {
                host: host.to_string(),
                detail: e.to_string(),
            },
            other => Self::TokenRejected {
                host: host.to_string(),
                detail: other.to_string(),
            },
        }
    }

    /// Whether the daemon should keep retrying the connection on its own.
    ///
    /// Only a transient network failure (`Unreachable`) is worth auto-retrying:
    /// the credentials are known-good and the outage is expected to clear. Every
    /// other reason needs the user to act — `forskap auth login` after a `TokenRejected` /
    /// `NoCredentials` / `LoggedOut`, or fixing the keychain — so retrying would
    /// just spin. Consumed by the background reconnect task (see `reconnect`).
    pub fn is_auto_retryable(&self) -> bool {
        matches!(self, Self::Unreachable { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_connect_error_classifies_transient_as_unreachable() {
        let r = DormancyReason::from_connect_error(
            "gitlab.example.com",
            &Error::Transient("connection refused".into()),
        );
        assert_eq!(r.reason(), NotAuthReason::unreachable);
        assert_eq!(
            r.detail().as_deref(),
            Some("gitlab.example.com: connection refused")
        );
    }

    #[test]
    fn from_connect_error_classifies_gitlab_as_token_rejected() {
        let r = DormancyReason::from_connect_error(
            "gitlab.example.com",
            &Error::Gitlab("401 Unauthorized".into()),
        );
        assert_eq!(r.reason(), NotAuthReason::token_rejected);
        assert_eq!(
            r.detail().as_deref(),
            Some("gitlab.example.com: GitLab error: 401 Unauthorized")
        );
    }

    /// A status GitLab answered the connection check with, other than a
    /// 429/5xx, is the token's fault, as when it carried no status.
    #[test]
    fn from_connect_error_classifies_a_rejection_as_token_rejected() {
        for status in [400, 403, 404] {
            let r = DormancyReason::from_connect_error(
                "gitlab.example.com",
                &Error::Rejected {
                    status,
                    detail: format!("HTTP {status}"),
                },
            );
            assert_eq!(r.reason(), NotAuthReason::token_rejected, "{status}");
            assert!(!r.is_auto_retryable(), "{status}");
        }
    }

    #[test]
    fn from_connect_error_classifies_throttled_as_unreachable() {
        for status in [429, 502] {
            let r = DormancyReason::from_connect_error(
                "gitlab.example.com",
                &Error::Throttled {
                    status,
                    retry_after: None,
                    detail: "busy".into(),
                },
            );
            assert!(r.is_auto_retryable(), "{status} must not park the daemon");
        }
    }

    #[test]
    fn retryability_by_error_and_idempotency() {
        let throttled = |status| Error::Throttled {
            status,
            retry_after: None,
            detail: String::new(),
        };
        for idempotent in [true, false] {
            assert!(Error::Transient("x".into()).is_retryable(idempotent));
            assert!(throttled(429).is_retryable(idempotent));
            assert!(!Error::Gitlab("404".into()).is_retryable(idempotent));
            for status in [400, 403, 404, 422] {
                let rejected = Error::Rejected {
                    status,
                    detail: String::new(),
                };
                assert!(!rejected.is_retryable(idempotent), "{status}");
                assert_eq!(rejected.retry_after(), None);
            }
        }
        assert!(throttled(503).is_retryable(true));
        assert!(!throttled(503).is_retryable(false));
    }

    #[test]
    fn codes_and_details_for_reasons_without_a_host() {
        assert_eq!(
            DormancyReason::NoCredentials.reason(),
            NotAuthReason::no_credentials
        );
        assert_eq!(DormancyReason::NoCredentials.detail(), None);
        assert_eq!(
            DormancyReason::LoggedOut.reason(),
            NotAuthReason::logged_out
        );
        assert_eq!(DormancyReason::LoggedOut.detail(), None);
        let k = DormancyReason::KeychainError("boom".into());
        assert_eq!(k.reason(), NotAuthReason::keychain_error);
        assert_eq!(k.detail().as_deref(), Some("boom"));
    }

    #[test]
    fn only_unreachable_is_auto_retryable() {
        let host = "gitlab.example.com".to_string();
        assert!(
            DormancyReason::Unreachable {
                host: host.clone(),
                detail: "connection refused".into(),
            }
            .is_auto_retryable()
        );
        assert!(
            !DormancyReason::TokenRejected {
                host,
                detail: "401".into(),
            }
            .is_auto_retryable()
        );
        assert!(!DormancyReason::NoCredentials.is_auto_retryable());
        assert!(!DormancyReason::KeychainError("boom".into()).is_auto_retryable());
        assert!(!DormancyReason::LoggedOut.is_auto_retryable());
    }
}
