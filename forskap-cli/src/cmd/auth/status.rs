//! `forskap auth status` — show who the daemon is currently authenticated as,
//! and what its token rotation is at; without a session, why there is none
//! and how the daemon stands.

use anyhow::Result;
use chrono::{DateTime, Utc};
use forskap_api::{TokenRotation, VarlinkClientInterface};

use crate::cli::OutputFormat;
use crate::cmd::status::stands;
use crate::cmd::sync::jobs::span;
use crate::friendly::{self, DaemonError as _, friendly};
use crate::{client, output, style};

/// A token this close to its expiry, and not rotated, needs replacing soon.
/// A week: GitLab's own expiry mail gives as much notice.
pub const EXPIRY_WARN_SECS: i64 = 7 * 86_400;

pub async fn run(format: OutputFormat) -> Result<()> {
    let client = client::connect_default().await?;
    let me = match client.who_am_i().call().await {
        Ok(me) => me,
        Err(e) => {
            let dormant = e.not_authenticated();
            let locked = dormant
                .as_ref()
                .map(|why| friendly::locked(why.reason.as_ref(), why.retrying));
            let said = dormant.and_then(|why| why.detail.map(str::to_string));
            let failed = friendly("WhoAmI", e);
            // No session: since when, and what the daemon does about it,
            // where it says.
            let stands = match locked {
                Some(locked) => {
                    let status = client.get_status().call().await.ok();
                    let standing = status.and_then(|s| s.dormancy);
                    let now = Utc::now().timestamp();
                    standing.and_then(|s| stands(&s, locked, said.as_deref(), now))
                }
                None => None,
            };
            return Err(match stands {
                Some(stands) => anyhow::anyhow!("{failed}\n{stands}"),
                None => failed,
            });
        }
    };

    output::emit(
        format,
        &serde_json::json!({
            "host": me.host,
            "user_id": me.user_id,
            "username": me.username,
            "token_expires_at": me.token_expires_at,
            "token_rotates": me.token_rotates,
            "rotation": me.rotation,
        }),
        |_| {
            let (rotation, now) = (me.rotation.as_ref(), Utc::now());
            outln!("{}", logged_in(&me.host, &me.username, me.user_id))?;
            outln!(
                "{}",
                token_line_styled(me.token_expires_at, me.token_rotates, rotation, now)
            )?;
            for note in rotation_notes(rotation, now) {
                outln!("{}", style::warning(&note))?;
            }
            Ok(())
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
pub fn token_line_styled(
    expires_at: Option<i64>,
    rotates: bool,
    rotation: Option<&TokenRotation>,
    now: DateTime<Utc>,
) -> String {
    let line = token_line(expires_at, rotates, rotation, now);
    let left = expires_at.map(|secs| secs - now.timestamp());
    match left {
        Some(left) if left <= 0 => style::error(&line).to_string(),
        Some(left) if !rotates && left < EXPIRY_WARN_SECS => style::warning(&line).to_string(),
        _ => line,
    }
}

/// The token's expiry and rotation in one sentence: when the daemon rotates
/// it, or why it doesn't, where the daemon says (`rotation`, since 1.3).
pub fn token_line(
    expires_at: Option<i64>,
    rotates: bool,
    rotation: Option<&TokenRotation>,
    now: DateTime<Utc>,
) -> String {
    let Some(expires) = expires_at.and_then(|secs| DateTime::from_timestamp(secs, 0)) else {
        return "The token has no known expiry date.".to_string();
    };
    let on = rotation
        .and_then(|r| r.at)
        .and_then(|secs| DateTime::from_timestamp(secs, 0));
    let skipped = rotation.and_then(|r| r.skipped.as_deref());
    let rotation = match (rotates, on, skipped) {
        (true, Some(on), _) => format!("the daemon rotates it on {}", on.format("%Y-%m-%d")),
        (true, None, _) => "the daemon rotates it before that".to_string(),
        (false, _, Some(why)) => format!("it is not rotated: {why}"),
        (false, _, None) => "automatic rotation is off".to_string(),
    };
    format!("The token {}; {rotation}.", expiry(expires, now))
}

/// What the rotation has to report beyond that sentence: an attempt that
/// failed, and a rotated token that hasn't reached the keychain.
pub fn rotation_notes(rotation: Option<&TokenRotation>, now: DateTime<Utc>) -> Vec<String> {
    let Some(rotation) = rotation else {
        return Vec::new();
    };
    let mut notes = Vec::new();
    if let Some(error) = &rotation.last_error {
        let next = rotation.retry_at.map(|at| at - now.timestamp());
        notes.push(match next {
            Some(secs) if secs > 0 => format!("{error}; the next attempt comes in {}.", span(secs)),
            _ => format!("{error}."),
        });
    }
    if rotation.unsaved == Some(true) {
        notes.push(
            "The rotated token is not in the keychain yet, which still holds the one GitLab \
             revoked: should the daemon restart before it is, run `forskap auth login` with a \
             new token. The daemon keeps trying to store it."
                .to_string(),
        );
    }
    notes
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
            assert_eq!(token_line(expires_at, rotates, None, now), line);
        }
    }

    fn rotation() -> TokenRotation {
        TokenRotation {
            at: None,
            skipped: None,
            last_error: None,
            retry_at: None,
            unsaved: None,
        }
    }

    /// A daemon that says what its rotation is at gets the date, or the
    /// reason, instead of "before that" and "off".
    #[test]
    fn token_line_says_when_it_rotates_or_why_not() {
        let now = at("2026-12-01T09:00:00Z");
        let expires = Some(at("2026-12-31T00:00:00Z").timestamp());
        let due = TokenRotation {
            at: Some(at("2026-12-21T08:14:00Z").timestamp()),
            ..rotation()
        };
        assert_eq!(
            token_line(expires, true, Some(&due), now),
            "The token expires on 2026-12-31 (in 29 days); the daemon rotates it on 2026-12-21."
        );
        let unscoped = TokenRotation {
            skipped: Some("it lacks the self_rotate scope".to_string()),
            ..rotation()
        };
        assert_eq!(
            token_line(expires, false, Some(&unscoped), now),
            "The token expires on 2026-12-31 (in 29 days); it is not rotated: it lacks the \
             self_rotate scope."
        );
        // Nothing read yet: as from a daemon too old to say.
        assert_eq!(
            token_line(expires, true, Some(&rotation()), now),
            token_line(expires, true, None, now)
        );
        assert_eq!(
            token_line(None, false, Some(&rotation()), now),
            "The token has no known expiry date."
        );
    }

    /// A failed attempt and an unsaved token are worth a line each; a
    /// rotation that goes its way is worth none.
    #[test]
    fn rotation_notes_say_what_went_wrong() {
        let now = at("2026-12-21T09:00:00Z");
        assert!(rotation_notes(None, now).is_empty());
        assert!(rotation_notes(Some(&rotation()), now).is_empty());

        let failed = TokenRotation {
            last_error: Some("rotating the GitLab token failed: network error: reset".to_string()),
            retry_at: Some(now.timestamp() + 240),
            ..rotation()
        };
        assert_eq!(
            rotation_notes(Some(&failed), now),
            [
                "rotating the GitLab token failed: network error: reset; the next attempt comes in 4m."
            ]
        );
        let refused = TokenRotation {
            last_error: Some("rotating the GitLab token was refused: 403".to_string()),
            unsaved: Some(true),
            ..rotation()
        };
        let notes = rotation_notes(Some(&refused), now);
        assert_eq!(notes[0], "rotating the GitLab token was refused: 403.");
        assert!(notes[1].starts_with("The rotated token is not in the keychain yet"));
    }

    #[test]
    fn token_line_colours_by_urgency() {
        style::force(true);
        let now = at("2026-12-24T09:00:00Z");
        let expires = |s: &str| Some(at(s).timestamp());
        let styled = |expires_at, rotates| token_line_styled(expires_at, rotates, None, now);
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
