//! `forskap auth status` — show who the daemon is currently authenticated as.

use anyhow::Result;
use chrono::{DateTime, Utc};
use forskap_api::VarlinkClientInterface;

use crate::cli::OutputFormat;
use crate::friendly::friendly;
use crate::{client, output};

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
            outln!(
                "Logged in to {} as @{} (#{}).",
                me.host,
                me.username,
                me.user_id
            )?;
            outln!(
                "{}",
                token_line(me.token_expires_at, me.token_rotates, Utc::now())
            )
        },
    )
}

/// The token's expiry and rotation in one sentence.
fn token_line(expires_at: Option<i64>, rotates: bool, now: DateTime<Utc>) -> String {
    let Some(expires) = expires_at.and_then(|secs| DateTime::from_timestamp(secs, 0)) else {
        return "The token has no known expiry date.".to_string();
    };
    let date = expires.format("%Y-%m-%d");
    let left = expires - now;
    let when = match (left.num_days(), left.num_hours()) {
        _ if left <= chrono::TimeDelta::zero() => format!("expired on {date}"),
        (0, 0) => format!("expires on {date} (in less than an hour)"),
        (0, 1) => format!("expires on {date} (in 1 hour)"),
        (0, hours) => format!("expires on {date} (in {hours} hours)"),
        (1, _) => format!("expires on {date} (in 1 day)"),
        (days, _) => format!("expires on {date} (in {days} days)"),
    };
    let rotation = if rotates {
        "the daemon rotates it before that"
    } else {
        "automatic rotation is off"
    };
    format!("The token {when}; {rotation}.")
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
}
