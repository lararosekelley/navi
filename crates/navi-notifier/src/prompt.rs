//! Minimal yes/no prompt for the setup/upgrade commands.

use std::io::{self, Write};

use anyhow::Result;

/// Print `message` and read a line; true only for an explicit yes.
pub fn confirm(message: &str) -> Result<bool> {
    print!("{message}");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(matches!(input.trim(), "y" | "Y" | "yes" | "Yes"))
}
