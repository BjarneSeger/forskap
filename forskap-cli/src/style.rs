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

/// A state word: of an issue, merge request or epic, of a sync job, the
/// origin of a time entry, or the level of a `forskap status` check. Words
/// outside the palette stay plain.
pub fn state(word: &str) -> Painted<'_> {
    let style = match word {
        "opened" | "running" | "ok" => fg(AnsiColor::Green),
        "closed" | "backing off" | "error" => fg(AnsiColor::Red),
        "merged" | "demanded" => fg(AnsiColor::Magenta),
        "locked" | "due" | "queued" | "warning" => fg(AnsiColor::Yellow),
        "waiting" | "skipped" => Style::new().dimmed(),
        _ => Style::new(),
    };
    paint(style, word)
}

/// An item as GitLab writes it: `#42`, `!7`, `&5`.
pub fn reference(sigil: impl fmt::Display, iid: i64) -> Painted<'static> {
    paint(fg(AnsiColor::Cyan), format!("{sigil}{iid}"))
}

/// Why a sync job or a queued write failed.
pub fn error(text: &str) -> Painted<'_> {
    paint(fg(AnsiColor::Red), text)
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
        assert_eq!(state("opened").to_string(), "opened");
        assert_eq!(reference('#', 42).to_string(), "#42");
        assert_eq!(error("403").to_string(), "403");
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
        assert_eq!(reference('!', 7).to_string(), "\x1b[36m!7\x1b[0m");
        assert_eq!(error("403").to_string(), "\x1b[31m403\x1b[0m");
        // A word outside the palette gets no escapes at all.
        assert_eq!(state("In review").to_string(), "In review");
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
    }
}
