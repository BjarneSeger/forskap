//! `forskap time` — time tracking. [`tick`] and [`prompt`] are also reachable as
//! the hidden top-level `forskap tick` / `forskap prompt` that installed hooks call.

mod history;
mod hook;
mod log;
pub mod prompt;
pub mod tick;

use anyhow::Result;

use crate::cli::TimeCommand;

pub async fn run(command: TimeCommand) -> Result<()> {
    match command {
        TimeCommand::Log {
            reference,
            duration,
            mr,
            project,
            summary,
        } => log::run(&reference, duration, mr, project.project, summary).await,
        TimeCommand::Prompt => prompt::run().await,
        TimeCommand::History { window, output } => history::run(window.days, output.output).await,
        TimeCommand::Hook { shell } => hook::run(shell),
    }
}

/// Seconds spent as the text views show them: `1h 30m`, `45s` under a
/// minute, `0m` for none. Unlike GitLab's spelling it has no days or weeks,
/// whose length depends on the instance.
pub fn spent(secs: i64) -> String {
    let secs = secs.max(0);
    if secs == 0 {
        return "0m".to_string();
    }
    let (hours, mins, rem) = (secs / 3600, secs % 3600 / 60, secs % 60);
    let mut parts = Vec::new();
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if mins > 0 {
        parts.push(format!("{mins}m"));
    }
    if hours == 0 && mins == 0 {
        parts.push(format!("{rem}s"));
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Anchors the grammar; the shape over all inputs is covered by
    /// `spent_renders_whole_minutes_of_any_input`.
    #[test]
    fn spent_pins_the_grammar() {
        assert_eq!(spent(5400), "1h 30m");
        assert_eq!(spent(9 * 3600), "9h");
        assert_eq!(spent(45), "45s");
        assert_eq!(spent(0), "0m");
        assert_eq!(spent(-5), "0m");
    }

    /// Split a `"2h 5m"`-style rendering back into (hours, minutes).
    fn parse_h_m(s: &str) -> (i64, i64) {
        let (mut hours, mut mins) = (0, 0);
        for part in s.split(' ') {
            if let Some(h) = part.strip_suffix('h') {
                hours = h.parse().unwrap();
            } else if let Some(m) = part.strip_suffix('m') {
                mins = m.parse().unwrap();
            } else {
                panic!("unexpected part {part:?} in {s:?}");
            }
        }
        (hours, mins)
    }

    proptest! {
        #[test]
        fn spent_renders_whole_minutes_of_any_input(secs in 0..i64::MAX) {
            let out = spent(secs);
            if secs == 0 {
                prop_assert_eq!(out, "0m");
            } else if secs < 60 {
                prop_assert_eq!(out, format!("{secs}s"));
            } else {
                // Past a minute the seconds remainder is dropped, never shown.
                let (hours, mins) = parse_h_m(&out);
                prop_assert!(mins < 60);
                prop_assert_eq!(hours * 3600 + mins * 60, secs - secs % 60);
            }
        }
    }
}
