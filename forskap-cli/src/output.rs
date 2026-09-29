//! The one place `--output` is interpreted.

use anyhow::Result;
use serde::Serialize;

use crate::cli::OutputFormat;

/// Print `value` as pretty JSON, or hand it to `text` to print.
pub fn emit<T: Serialize + ?Sized>(
    format: OutputFormat,
    value: &T,
    text: impl FnOnce(&T),
) -> Result<()> {
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(value)?),
        OutputFormat::Text => text(value),
    }
    Ok(())
}
