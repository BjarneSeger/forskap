//! `forskap auth status` — show who the daemon is currently authenticated as.

use anyhow::Result;
use chrono::{DateTime, Utc};
use forskap_api::VarlinkClientInterface;

use crate::cli::OutputFormat;
use crate::friendly::friendly;
use crate::{client, output, style};

/// A token this close to its expiry, and not rotated, needs replacing soon.
/// A week: GitLab's own expiry mail gives as much notice.
pub const EXPIRY_WARN_SECS: i64 = 7 * 86_400;

pub async fn run(format: OutputFormat) -> Result<()> {
    let client = client::connect_default().await?;
    let me = client
        .who_am_i()
        .call()
        .await
        .map_err(|e| friendly("WhoAmI", e))?;

    output::emit(
        format,
        &serde_json::json!({
            "host": me.host,
            "user_id": me.user_id,
            "username": me.username,
            "token_expires_at": me.token_expires_at,
            "token_rotates": me.token_rotates,
        }),
        |_| {
            outln!("{}", logged_in(&me.host, &me.username, me.user_id))?;
            outln!(
                "{}",
                token_line_styled(me.token_expires_at, me.token_rotates, Utc::now())
            )
        },
    )
}

/// `Logged in to gitlab.example.com as @ada (#7).`
pub fn logged_in(host: &str, username: &str, user_id: i64) -> String {
    format!(
        "Logged in to {} as {} {}.",
        style::strong(host),
        style::strong(&format!("@{username}")),
        style::muted(&format!("(#{user_id})"))
    )
}

/// [`token_line`] coloured by its urgency: red once the token expired, yellow
/// while it expires inside the week `forskap status` warns in and nothing
/// rotates it, plain otherwise.
pub fn token_line_styled(expires_at: Option<i64>, rotates: bool, now: DateTime<Utc>) -> String {
    let line = token_line(expires_at, rotates, now);
    let left = expires_at.map(|secs| secs - now.timestamp());
    match left {
        Some(left) if left <= 0 => style::error(&line).to_string(),
        Some(left) if !rotates && left < EXPIRY_WARN_SECS => style::warning(&line).to_string(),
        _ => line,
    }
}

/// The token's expiry and rotation in one sentence.
pub fn token_line(expires_at: Option<i64>, rotates: bool, now: DateTime<Utc>) -> String {
    let Some(expires) = expires_at.and_then(|secs| DateTime::from_timestamp(secs, 0)) else {
        return "The token has no known expiry date.".to_string();
    };
    let rotation = if rotates {
        "the daemon rotates it before that"
    } else {
        "automatic rotation is off"
    };
    format!("The token {}; {rotation}.", expiry(expires, now))
}

/// When the token expires, or that it did: `expires on 2026-12-31 (in 6
/// days)`, `expired on 2026-12-24`.
pub fn expiry(expires: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let date = expires.format("%Y-%m-%d");
    let left = expires - now;
    match (left.num_days(), left.num_hours()) {
        _ if left <= chrono::TimeDelta::zero() => format!("expired on {date}"),
        (0, 0) => format!("expires on {date} (in less than an hour)"),
        (0, 1) => format!("expires on {date} (in 1 hour)"),
        (0, hours) => format!("expires on {date} (in {hours} hours)"),
        (1, _) => format!("expires on {date} (in 1 day)"),
        (days, _) => format!("expires on {date} (in {days} days)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().to_utc()
    }

    #[test]
    fn token_line_tells_expiry_and_rotation() {
        let now = at("2026-12-24T09:00:00Z");
        let expires = |s: &str| Some(at(s).timestamp());
        for (expires_at, rotates, line) in [
            (
                expires("2026-12-31T00:00:00Z"),
                true,
                "The token expires on 2026-12-31 (in 6 days); the daemon rotates it before that.",
            ),
            (
                expires("2026-12-25T10:00:00Z"),
                false,
                "The token expires on 2026-12-25 (in 1 day); automatic rotation is off.",
            ),
            (
                expires("2026-12-25T00:00:00Z"),
                false,
                "The token expires on 2026-12-25 (in 15 hours); automatic rotation is off.",
            ),
            (
                expires("2026-12-24T09:30:00Z"),
                true,
                "The token expires on 2026-12-24 (in less than an hour); the daemon rotates it before that.",
            ),
            (
                expires("2026-12-24T00:00:00Z"),
                false,
                "The token expired on 2026-12-24; automatic rotation is off.",
            ),
            (None, false, "The token has no known expiry date."),
        ] {
            assert_eq!(token_line(expires_at, rotates, now), line);
        }
    }

    #[test]
    fn token_line_colours_by_urgency() {
        style::force(true);
        let now = at("2026-12-24T09:00:00Z");
        let expires = |s: &str| Some(at(s).timestamp());
        let styled = |expires_at, rotates| token_line_styled(expires_at, rotates, now);
        assert!(
            styled(expires("2026-12-24T00:00:00Z"), true).starts_with("\x1b[31mThe token expired")
        );
        assert!(
            styled(expires("2026-12-30T00:00:00Z"), false).starts_with("\x1b[33mThe token expires")
        );
        // Rotated before then, or far off: nothing to act on.
        assert_eq!(
            styled(expires("2026-12-30T00:00:00Z"), true),
            "The token expires on 2026-12-30 (in 5 days); the daemon rotates it before that."
        );
        assert!(styled(expires("2027-03-01T00:00:00Z"), false).starts_with("The token expires"));
        assert_eq!(styled(None, false), "The token has no known expiry date.");
        assert_eq!(
            logged_in("gitlab.example.com", "ada", 7),
            "Logged in to \x1b[1mgitlab.example.com\x1b[0m as \x1b[1m@ada\x1b[0m \x1b[2m(#7)\x1b[0m."
        );
    }
}
