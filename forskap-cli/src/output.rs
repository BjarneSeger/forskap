//! Everything written to stdout, and the one place `--output` is interpreted.

use std::fmt;
use std::io::{self, Write};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::cli::OutputFormat;

/// The reader of our stdout is gone (`forskap issue list | head`); `main`
/// exits quietly on it.
#[derive(Debug, thiserror::Error)]
#[error("stdout closed")]
pub struct StdoutClosed;

/// `print!` that returns the error instead of panicking on a closed pipe.
macro_rules! out {
    ($($arg:tt)*) => {
        $crate::output::write(format_args!($($arg)*))
    };
}

/// `println!` counterpart of [`out!`].
macro_rules! outln {
    ($($arg:tt)*) => {
        $crate::output::write(format_args!("{}\n", format_args!($($arg)*)))
    };
}

pub fn write(args: fmt::Arguments) -> Result<()> {
    write_to(&mut io::stdout(), args)
}

fn write_to(out: &mut impl Write, args: fmt::Arguments) -> Result<()> {
    match out.write_fmt(args) {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Err(StdoutClosed.into()),
        other => other.context("writing to stdout"),
    }
}

/// Print `value` as pretty JSON, or hand it to `text` to print.
pub fn emit<T: Serialize + ?Sized>(
    format: OutputFormat,
    value: &T,
    text: impl FnOnce(&T) -> Result<()>,
) -> Result<()> {
    match format {
        OutputFormat::Json => outln!("{}", serde_json::to_string_pretty(value)?),
        OutputFormat::Text => text(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Failing(io::ErrorKind);

    impl Write for Failing {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(self.0.into())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn closed_pipe_is_its_own_error() {
        let err = write_to(&mut Failing(io::ErrorKind::BrokenPipe), format_args!("x")).unwrap_err();
        assert!(err.is::<StdoutClosed>());
    }

    #[test]
    fn other_write_errors_are_reported() {
        let err =
            write_to(&mut Failing(io::ErrorKind::StorageFull), format_args!("x")).unwrap_err();
        assert!(!err.is::<StdoutClosed>());
    }
}
