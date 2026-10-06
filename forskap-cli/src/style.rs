//! Colour: the palette, and the one place `--color` is interpreted.
//!
//! The text views call the named helpers, which return their text unstyled
//! while colour is off. Structured output is serialized from the daemon's
//! reply and never passes through here.

use std::borrow::Cow;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};

use anstyle::{AnsiColor, Style};

use crate::cli::ColorChoice;

static ON: AtomicBool = AtomicBool::new(false);

// A test switches only its own thread: the others pin plain text meanwhile.
#[cfg(test)]
thread_local! {
    static ON_HERE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Settle `--color` for this run, against stdout and the environment.
pub fn init(choice: ColorChoice) {
    use std::io::IsTerminal;

    let var = std::env::var_os;
    let env = Env {
        terminal: std::io::stdout().is_terminal() && var("TERM").is_none_or(|v| v != "dumb"),
        no_color: var("NO_COLOR").is_some_and(|v| !v.is_empty()),
        force: var("CLICOLOR_FORCE").is_some_and(|v| !v.is_empty() && v != "0"),
    };
    ON.store(decide(choice, env), Ordering::Relaxed);
}

#[cfg(test)]
pub fn force(on: bool) {
    ON_HERE.set(on);
}

fn on() -> bool {
    #[cfg(test)]
    if ON_HERE.get() {
        return true;
    }
    ON.load(Ordering::Relaxed)
}

/// What `auto` decides on.
#[derive(Clone, Copy)]
struct Env {
    /// Stdout is a terminal that knows colour.
    terminal: bool,
    no_color: bool,
    force: bool,
}

fn decide(choice: ColorChoice, env: Env) -> bool {
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => !env.no_color && (env.force || env.terminal),
    }
}

/// Styled text that still pads like the bare text: `{:<8}` counts only what
/// is visible.
pub struct Painted<'a> {
    style: Style,
    text: Cow<'a, str>,
}

impl fmt::Display for Painted<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Nothing to colour: an absent suffix or value writes no escapes.
        if self.text.is_empty() {
            return f.pad("");
        }
        write!(f, "{}", self.style)?;
        f.pad(&self.text)?;
        write!(f, "{:#}", self.style)
    }
}

fn paint<'a>(style: Style, text: impl Into<Cow<'a, str>>) -> Painted<'a> {
    Painted {
        style: if on() { style } else { Style::new() },
        text: text.into(),
    }
}

fn fg(color: AnsiColor) -> Style {
    Style::new().fg_color(Some(color.into()))
}

/// A section or day heading, or a table's header row.
pub fn heading(text: &str) -> Painted<'_> {
    paint(Style::new().bold(), text)
}

/// What a line is about: an item's title in a view, the host and the user
/// `forskap auth status` names.
pub fn strong(text: &str) -> Painted<'_> {
    paint(Style::new().bold(), text)
}

/// A state word: of an issue, merge request or epic, of a sync job, the
/// origin of a time entry, the level of a `forskap status` check or its
/// verdict. Words outside the palette stay plain.
pub fn state(word: &str) -> Painted<'_> {
    let style = match word {
        "opened" | "running" | "ok" | "healthy" => fg(AnsiColor::Green),
        "closed" | "backing off" | "error" | "unhealthy" => fg(AnsiColor::Red),
        "merged" | "demanded" => fg(AnsiColor::Magenta),
        "locked" | "due" | "queued" | "warning" => fg(AnsiColor::Yellow),
        "waiting" | "skipped" | "unavailable" => Style::new().dimmed(),
        _ => Style::new(),
    };
    paint(style, word)
}

/// The verb of an activity event, as GitLab names it: what the user did.
/// Verbs outside the palette stay plain.
pub fn action(verb: &str) -> Painted<'_> {
    let style = match verb {
        "opened" | "reopened" | "created" | "joined" => fg(AnsiColor::Green),
        "closed" | "deleted" | "destroyed" | "left" | "expired" => fg(AnsiColor::Red),
        "merged" | "accepted" | "approved" => fg(AnsiColor::Magenta),
        "pushed to" | "pushed new" => fg(AnsiColor::Blue),
        _ => Style::new(),
    };
    paint(style, verb)
}

/// An item as GitLab writes it: `#42`, `!7`, `&5`.
pub fn reference(sigil: impl fmt::Display, iid: i64) -> Painted<'static> {
    paint(fg(AnsiColor::Cyan), format!("{sigil}{iid}"))
}

/// Where an item lives: a project or group path, or `project 7` where only
/// the id is known.
pub fn path(text: &str) -> Painted<'_> {
    paint(fg(AnsiColor::Blue), text)
}

/// The verb of a confirmation: what a write or a command just did.
pub fn success(text: &str) -> Painted<'_> {
    paint(fg(AnsiColor::Green), text)
}

/// Something to act on before long: a rate-limit pause, a token about to
/// expire.
pub fn warning(text: &str) -> Painted<'_> {
    paint(fg(AnsiColor::Yellow), text)
}

/// Why a sync job or a queued write failed.
pub fn error(text: &str) -> Painted<'_> {
    paint(fg(AnsiColor::Red), text)
}

/// Secondary text: a timestamp, a URL, a field's label, a hint, an empty
/// result, a remark that is no failure.
pub fn muted(text: &str) -> Painted<'_> {
    paint(Style::new().dimmed(), text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_colours_only_a_terminal_and_the_flag_overrules() {
        use ColorChoice::{Always, Auto, Never};
        for terminal in [false, true] {
            for no_color in [false, true] {
                for force in [false, true] {
                    let env = Env {
                        terminal,
                        no_color,
                        force,
                    };
                    assert!(decide(Always, env));
                    assert!(!decide(Never, env));
                    assert_eq!(
                        decide(Auto, env),
                        !no_color && (terminal || force),
                        "terminal {terminal}, NO_COLOR {no_color}, CLICOLOR_FORCE {force}"
                    );
                }
            }
        }
        // What a launcher or a script reading the text output gets.
        let piped = Env {
            terminal: false,
            no_color: false,
            force: false,
        };
        assert!(!decide(Auto, piped));
    }

    #[test]
    fn off_leaves_the_text_as_it_is() {
        assert_eq!(heading("Issues:").to_string(), "Issues:");
        assert_eq!(strong("Fix login").to_string(), "Fix login");
        assert_eq!(state("opened").to_string(), "opened");
        assert_eq!(action("pushed to").to_string(), "pushed to");
        assert_eq!(reference('#', 42).to_string(), "#42");
        assert_eq!(path("team/api").to_string(), "team/api");
        assert_eq!(success("logged").to_string(), "logged");
        assert_eq!(warning("paused").to_string(), "paused");
        assert_eq!(error("403").to_string(), "403");
        assert_eq!(muted("refused").to_string(), "refused");
    }

    #[test]
    fn on_wraps_the_text_in_its_style() {
        force(true);
        assert_eq!(heading("Issues:").to_string(), "\x1b[1mIssues:\x1b[0m");
        assert_eq!(state("opened").to_string(), "\x1b[32mopened\x1b[0m");
        assert_eq!(state("waiting").to_string(), "\x1b[2mwaiting\x1b[0m");
        // The levels of `forskap status`.
        assert_eq!(state("ok").to_string(), "\x1b[32mok\x1b[0m");
        assert_eq!(state("warning").to_string(), "\x1b[33mwarning\x1b[0m");
        assert_eq!(state("error").to_string(), "\x1b[31merror\x1b[0m");
        assert_eq!(state("skipped").to_string(), "\x1b[2mskipped\x1b[0m");
        // A sync job GitLab refuses for good is no failure.
        assert_eq!(
            state("unavailable").to_string(),
            "\x1b[2munavailable\x1b[0m"
        );
        // The verdict of `forskap status`.
        assert_eq!(state("healthy").to_string(), "\x1b[32mhealthy\x1b[0m");
        assert_eq!(state("unhealthy").to_string(), "\x1b[31munhealthy\x1b[0m");
        assert_eq!(muted("refused").to_string(), "\x1b[2mrefused\x1b[0m");
        assert_eq!(reference('!', 7).to_string(), "\x1b[36m!7\x1b[0m");
        assert_eq!(error("403").to_string(), "\x1b[31m403\x1b[0m");
        assert_eq!(strong("Fix login").to_string(), "\x1b[1mFix login\x1b[0m");
        assert_eq!(path("team/api").to_string(), "\x1b[34mteam/api\x1b[0m");
        assert_eq!(success("logged").to_string(), "\x1b[32mlogged\x1b[0m");
        assert_eq!(warning("paused").to_string(), "\x1b[33mpaused\x1b[0m");
        // A word outside the palette gets no escapes at all.
        assert_eq!(state("In review").to_string(), "In review");
        assert_eq!(state("gitlab").to_string(), "gitlab");
    }

    #[test]
    fn actions_take_the_colour_of_what_they_did() {
        force(true);
        for (verb, code) in [
            ("opened", "32"),
            ("reopened", "32"),
            ("created", "32"),
            ("joined", "32"),
            ("closed", "31"),
            ("deleted", "31"),
            ("destroyed", "31"),
            ("left", "31"),
            ("expired", "31"),
            ("merged", "35"),
            ("accepted", "35"),
            ("approved", "35"),
            ("pushed to", "34"),
            ("pushed new", "34"),
        ] {
            assert_eq!(
                action(verb).to_string(),
                format!("\x1b[{code}m{verb}\x1b[0m")
            );
        }
        assert_eq!(action("commented on").to_string(), "commented on");
        assert_eq!(action("updated").to_string(), "updated");
    }

    #[test]
    fn empty_text_gets_no_escapes() {
        force(true);
        assert_eq!(muted("").to_string(), "");
        assert_eq!(format!("{:<4}|", path("")), "    |");
    }

    #[test]
    fn padding_counts_only_the_visible_text() {
        force(true);
        assert_eq!(
            format!("{:<8}|", state("closed")),
            "\x1b[31mclosed  \x1b[0m|"
        );
        assert_eq!(
            format!("{:<6}|", reference('#', 42)),
            "\x1b[36m#42   \x1b[0m|"
        );
        assert_eq!(
            format!("{:<12}|", action("pushed to")),
            "\x1b[34mpushed to   \x1b[0m|"
        );
        assert_eq!(format!("{:<7}|", muted("url:")), "\x1b[2murl:   \x1b[0m|");
    }
}
