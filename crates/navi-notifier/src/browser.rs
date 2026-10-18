//! Open a URL with the platform's default handler.

use std::process::Command;

use anyhow::{bail, Context, Result};

/// Open `url` in the default browser.
pub fn open(url: &str) -> Result<()> {
    let mut command = if cfg!(target_os = "macos") {
        Command::new("open")
    } else if cfg!(windows) {
        // Not `cmd /C start`: cmd.exe would interpret `&`, `^`, or `%` in the URL.
        let mut command = Command::new("rundll32");
        command.arg("url.dll,FileProtocolHandler");
        command
    } else {
        Command::new("xdg-open")
    };
    let status = command.arg(url).status().context("launching the browser")?;
    if !status.success() {
        bail!("the browser opener exited with {status}");
    }
    Ok(())
}
