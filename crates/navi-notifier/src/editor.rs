//! Open a file in the user's editor, the way `git config --edit` does: honor
//! `$VISUAL` then `$EDITOR`, falling back to a platform default. The editor
//! string may carry flags (e.g. `code -w`), so it's split on whitespace.

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

/// Launch the resolved editor on `path` and wait for it to exit.
pub fn open(path: &Path) -> Result<()> {
    let editor = std::env::var("VISUAL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("EDITOR")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
        .unwrap_or_else(|| default_editor().to_string());

    let mut parts = editor.split_whitespace();
    // Non-empty by construction: the env vars are filtered for non-blank content and
    // `default_editor()` is a literal, so there is always at least one whitespace token.
    let program = parts.next().expect("editor string is never empty");

    let status = Command::new(program)
        .args(parts)
        .arg(path)
        .status()
        .with_context(|| format!("launching editor `{program}`"))?;
    if !status.success() {
        bail!("editor `{program}` exited with {status}");
    }
    Ok(())
}

#[cfg(windows)]
fn default_editor() -> &'static str {
    "notepad"
}

#[cfg(not(windows))]
fn default_editor() -> &'static str {
    "vi"
}
