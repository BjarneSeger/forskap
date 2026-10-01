//! `--watch`: redraw a text view until Ctrl-C.
//!
//! The frame is painted over the previous one with plain cursor-home and
//! erase sequences. No alternate screen, no hidden cursor: nothing to restore
//! when the signal ends the process.

use std::io::{self, IsTerminal, Write};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::ValueEnum;

use crate::cli::{OutputFormat, WatchArgs};

const HOME: &str = "\x1b[H";
pub(crate) const ERASE_LINE: &str = "\x1b[K";
const ERASE_BELOW: &str = "\x1b[J";

/// How often to redraw, if at all. Only the text view can be redrawn.
pub fn interval(args: WatchArgs, format: OutputFormat) -> Result<Option<Duration>> {
    let Some(secs) = args.watch else {
        return Ok(None);
    };
    if !matches!(format, OutputFormat::Text) {
        let name = format.to_possible_value();
        let name = name.as_ref().map_or("that", |v| v.get_name());
        bail!("--watch redraws the text view; it can't be combined with --output {name}");
    }
    Ok(Some(Duration::from_secs(secs)))
}

/// Print what `view` returns every `every`, in place on a terminal and one
/// frame after the other elsewhere.
///
/// A first frame that fails is the command's error. Later ones are shown in
/// place of the view and retried: a watch outlives a daemon restart.
pub async fn run<F, Fut>(every: Duration, mut view: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<String>>,
{
    let terminal = io::stdout().is_terminal();
    let mut text = view().await?;
    loop {
        if terminal {
            out!("{}", paint(&text, size()))?;
            // The trailing erase has no newline to push it out.
            io::stdout().flush().context("writing to stdout")?;
        } else {
            outln!("{text}")?;
        }
        tokio::time::sleep(every).await;
        text = view()
            .await
            .unwrap_or_else(|e| format!("error: {e:#}\nretrying every {}s\n", every.as_secs()));
    }
}

/// Columns and rows of the terminal.
fn size() -> Option<(usize, usize)> {
    terminal_size::terminal_size().map(|(w, h)| (w.0.into(), h.0.into()))
}

/// Append the first `cols` visible characters of `line`. Its escape
/// sequences take no column and are all kept, also past the cut: a colour
/// opened before it is closed by its own reset.
pub(crate) fn cut(frame: &mut String, line: &str, cols: usize) {
    let mut visible = 0;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            frame.push(c);
            // A CSI sequence ends with its first byte from `@` to `~`.
            let mut csi = false;
            for c in chars.by_ref() {
                frame.push(c);
                if (csi && ('@'..='~').contains(&c)) || (!csi && c != '[') {
                    break;
                }
                csi = true;
            }
        } else if visible < cols {
            frame.push(c);
            visible += 1;
        }
    }
}

/// `text` as a frame drawn over the previous one, cut to the screen: a line
/// that wraps or a frame that scrolls would leave the cursor-home short.
fn paint(text: &str, size: Option<(usize, usize)>) -> String {
    let (cols, rows) = size.unwrap_or((usize::MAX, usize::MAX));
    // The cursor rests on the row below the frame.
    let room = rows.saturating_sub(1).max(1);
    let total = text.lines().count();
    let shown = if total > room { room - 1 } else { total };

    let mut frame = String::from(HOME);
    // Each line is erased before it is written: at the last column an erase
    // after the text would eat its last character.
    for line in text.lines().take(shown) {
        frame.push_str(ERASE_LINE);
        cut(&mut frame, line, cols);
        frame.push('\n');
    }
    if shown < total {
        let more = format!("… {} more lines", total - shown);
        frame.push_str(ERASE_LINE);
        frame.extend(more.chars().take(cols));
        frame.push('\n');
    }
    frame.push_str(ERASE_BELOW);
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_only_for_the_text_view() {
        let watch = |watch| WatchArgs { watch };
        assert_eq!(interval(watch(None), OutputFormat::Json).unwrap(), None);
        assert_eq!(
            interval(watch(Some(5)), OutputFormat::Text).unwrap(),
            Some(Duration::from_secs(5))
        );
        let err = interval(watch(Some(5)), OutputFormat::Json).unwrap_err();
        assert!(err.to_string().ends_with("--output json"), "{err}");
    }

    #[test]
    fn paint_overwrites_line_by_line() {
        assert_eq!(
            paint("a\nbc\n", Some((80, 24))),
            "\x1b[H\x1b[Ka\n\x1b[Kbc\n\x1b[J"
        );
        assert_eq!(paint("", None), "\x1b[H\x1b[J");
    }

    #[test]
    fn paint_cuts_the_frame_to_the_screen() {
        // Four rows: two lines, the note, and the row the cursor rests on.
        assert_eq!(
            paint("one\ntwo\nthree\nfour\n", Some((80, 4))),
            "\x1b[H\x1b[Kone\n\x1b[Ktwo\n\x1b[K… 2 more lines\n\x1b[J"
        );
        assert_eq!(
            paint("one\ntwo\nthree\n", Some((80, 4))),
            "\x1b[H\x1b[Kone\n\x1b[Ktwo\n\x1b[Kthree\n\x1b[J"
        );
        assert_eq!(
            paint("wide line\n", Some((4, 24))),
            "\x1b[H\x1b[Kwide\n\x1b[J"
        );
    }

    #[test]
    fn cut_counts_only_what_is_visible_and_closes_the_colours() {
        let cut = |line, cols| {
            let mut out = String::new();
            super::cut(&mut out, line, cols);
            out
        };
        let line = "\x1b[1mJOB\x1b[0m  \x1b[32mrunning\x1b[0m  now";
        // Wide enough: untouched, however many escapes it carries.
        assert_eq!(cut(line, 17), line);
        // Cut inside a coloured word: the colour is still reset.
        assert_eq!(cut(line, 8), "\x1b[1mJOB\x1b[0m  \x1b[32mrun\x1b[0m");
        // Cut before it: its escapes stay whole and show nothing.
        assert_eq!(cut(line, 3), "\x1b[1mJOB\x1b[0m\x1b[32m\x1b[0m");
        assert_eq!(cut("äöü", 2), "äö");
        assert_eq!(
            paint("\x1b[32mwide\x1b[0m line\n", Some((2, 24))),
            "\x1b[H\x1b[K\x1b[32mwi\x1b[0m\n\x1b[J"
        );
    }
}
