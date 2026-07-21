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

/// Print `message` and return the trimmed line the user types.
pub fn input(message: &str) -> Result<String> {
    print!("{message}");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}
