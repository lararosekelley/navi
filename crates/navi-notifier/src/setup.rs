//! `navi setup` / `navi uninstall`: install the man page, wire shell completions
//! into the user's rc file, and reverse both.

use std::env;
use std::fs;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use clap::CommandFactory;

use crate::cli::Cli;
use crate::prompt::confirm;

/// Marker written above the completion line so re-runs and uninstall can find it
/// (`#` is also a comment in PowerShell).
const COMPLETION_MARKER: &str = "# added by navi setup";

const POWERSHELL_LINE: &str = "if (Get-Command navi -ErrorAction SilentlyContinue) { navi completions powershell | Out-String | Invoke-Expression }";

pub fn setup(yes: bool, refresh: bool) -> Result<()> {
    install_man_page()?;
    if refresh {
        // Non-interactive; run by `upgrade` via the new binary. The rc line
        // re-sources from the binary each shell start, so wiring needs no update.
        return print_completion_hint();
    }
    wire_completions(yes)
}

/// Render the man page into the XDG data dir so `man navi` resolves it.
fn install_man_page() -> Result<()> {
    if cfg!(windows) {
        return Ok(());
    }
    let dir = man_dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let mut buffer = Vec::new();
    clap_mangen::Man::new(Cli::command())
        .render(&mut buffer)
        .context("failed to render man page")?;
    let path = dir.join("navi.1");
    fs::write(&path, buffer).with_context(|| format!("failed to write {}", path.display()))?;
    println!("installed man page to {}", path.display());
    Ok(())
}

fn man_dir() -> Result<PathBuf> {
    let data_home = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share")))
        .or_else(|| env::var_os("LOCALAPPDATA").map(PathBuf::from))
        .context("cannot locate a data directory; set HOME, XDG_DATA_HOME, or LOCALAPPDATA")?;
    Ok(data_home.join("man").join("man1"))
}

/// Append a completion-sourcing line to the detected shell's rc file, once.
fn wire_completions(yes: bool) -> Result<()> {
    let Some((shell, rc_path, line)) = completion_target()? else {
        println!("could not detect a supported shell; see the README for manual setup");
        return Ok(());
    };

    let existing = match fs::read_to_string(&rc_path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("failed to read {}", rc_path.display())),
    };
    if existing.contains(COMPLETION_MARKER) || existing.contains("navi completions") {
        println!(
            "{shell} completions already configured in {}",
            rc_path.display()
        );
        return Ok(());
    }

    // Only prompt at a real terminal. Piped in (the installer running us), there
    // is no one to answer, so skip cleanly instead of reading EOF as "no".
    let interactive = std::io::stdin().is_terminal();
    let proceed = if yes {
        true
    } else if interactive {
        confirm(&format!(
            "append completion setup to {}? [y/N] ",
            rc_path.display()
        ))?
    } else {
        false
    };
    if !proceed {
        println!(
            "{}",
            if interactive {
                "skipped completion setup"
            } else {
                "non-interactive shell; skipped completion setup"
            }
        );
        println!("to configure manually, add this to {}:", rc_path.display());
        println!("  {line}");
        return Ok(());
    }

    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&format!("\n{COMPLETION_MARKER}\n{line}\n"));
    if let Some(parent) = rc_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(&rc_path, updated)
        .with_context(|| format!("failed to write {}", rc_path.display()))?;
    println!("added {shell} completion setup to {}", rc_path.display());
    Ok(())
}

fn print_completion_hint() -> Result<()> {
    let Some((shell, rc_path, line)) = completion_target()? else {
        return Ok(());
    };
    let configured = fs::read_to_string(&rc_path)
        .map(|rc| rc.contains(COMPLETION_MARKER) || rc.contains("navi completions"))
        .unwrap_or(false);
    if configured {
        return Ok(());
    }
    println!(
        "{shell} completions are not configured; run `navi setup`, or add to {}:",
        rc_path.display()
    );
    println!("  {line}");
    Ok(())
}

/// Resolve (shell name, rc file, completion line). A POSIX shell from `$SHELL`
/// wins (covers Git Bash / WSL); otherwise fall back to PowerShell.
fn completion_target() -> Result<Option<(&'static str, PathBuf, &'static str)>> {
    if let Some(target) = posix_shell_target() {
        return Ok(Some(target));
    }
    Ok(powershell_target())
}

fn posix_shell_target() -> Option<(&'static str, PathBuf, &'static str)> {
    let shell = env::var("SHELL").unwrap_or_default();
    let shell = shell.rsplit('/').next().unwrap_or_default();
    let home = env::var_os("HOME").map(PathBuf::from)?;
    match shell {
        "bash" => Some((
            "bash",
            home.join(".bashrc"),
            "command -v navi >/dev/null && source <(navi completions bash)",
        )),
        "zsh" => Some((
            "zsh",
            home.join(".zshrc"),
            "command -v navi >/dev/null && source <(navi completions zsh)",
        )),
        "fish" => Some((
            "fish",
            home.join(".config/fish/config.fish"),
            "command -q navi; and navi completions fish | source",
        )),
        _ => None,
    }
}

fn powershell_target() -> Option<(&'static str, PathBuf, &'static str)> {
    for exe in ["pwsh", "powershell"] {
        let Ok(output) = Command::new(exe)
            .args(["-NoProfile", "-Command", "$PROFILE"])
            .output()
        else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !path.is_empty() {
            return Some(("PowerShell", PathBuf::from(path), POWERSHELL_LINE));
        }
    }
    None
}

/// Reverse `setup` and the installer: strip the completion line we added, delete
/// the man page, and remove the config/receipt directory. The binary is reported
/// (with its removal command) rather than deleted.
pub fn uninstall(dry_run: bool, yes: bool) -> Result<()> {
    let completion = match completion_target()? {
        Some((shell, rc_path, _)) => match fs::read_to_string(&rc_path) {
            Ok(contents) if contents.contains(COMPLETION_MARKER) => {
                Some((shell, rc_path, contents))
            }
            _ => None,
        },
        None => None,
    };
    let man_page = man_dir()
        .ok()
        .map(|dir| dir.join("navi.1"))
        .filter(|p| p.exists());
    let config_dir = crate::upgrade::config_dir().filter(|p| p.exists());

    println!("navi uninstall removes what setup and the installer added:");
    let mut anything = false;
    if let Some((shell, rc_path, _)) = &completion {
        println!("  - {shell} completion line in {}", rc_path.display());
        anything = true;
    }
    if let Some(path) = &man_page {
        println!("  - man page {}", path.display());
        anything = true;
    }
    if let Some(dir) = &config_dir {
        println!("  - config and install receipt in {}", dir.display());
        anything = true;
    }
    if !anything {
        println!("  (nothing found; already removed, or installed another way)");
    }

    if dry_run {
        println!("dry run: nothing was removed");
        print_binary_note();
        return Ok(());
    }
    if anything && !yes && !confirm("remove these? [y/N] ")? {
        println!("uninstall cancelled");
        print_binary_note();
        return Ok(());
    }

    if let Some((shell, rc_path, contents)) = completion {
        if let Some(stripped) = strip_completion_block(&contents) {
            fs::write(&rc_path, stripped)
                .with_context(|| format!("failed to update {}", rc_path.display()))?;
            println!("removed {shell} completion line from {}", rc_path.display());
        }
    }
    if let Some(path) = man_page {
        fs::remove_file(&path).with_context(|| format!("failed to remove {}", path.display()))?;
        println!("removed man page {}", path.display());
    }
    if let Some(dir) = config_dir {
        fs::remove_dir_all(&dir).with_context(|| format!("failed to remove {}", dir.display()))?;
        println!("removed {}", dir.display());
    }

    print_binary_note();
    Ok(())
}

/// Tell the user how to remove the binary itself; a running process can't
/// reliably delete its own executable.
fn print_binary_note() {
    println!();
    match env::current_exe() {
        Ok(path) => {
            println!("the navi binary is left in place; remove it with:");
            if cfg!(windows) {
                println!("  Remove-Item \"{}\"", path.display());
            } else {
                println!("  rm {}", path.display());
            }
        }
        Err(_) => println!("remove the navi binary from your PATH to finish."),
    }
    println!("(or `cargo uninstall navi-notifier` / `brew uninstall navi-notifier` if you installed it that way)");
    println!("your config and state (~/.config/navi is removed above; state db is left) are otherwise untouched.");
}

/// Drop the block `setup` appended: the marker, the completion line after it,
/// and the single blank line before it. `None` when the marker is absent.
fn strip_completion_block(contents: &str) -> Option<String> {
    let lines: Vec<&str> = contents.lines().collect();
    let marker = lines
        .iter()
        .position(|line| line.trim() == COMPLETION_MARKER)?;
    let removes_line = lines
        .get(marker + 1)
        .is_some_and(|line| line.contains("navi completions"));
    let end = (marker + 1 + usize::from(removes_line)).min(lines.len());
    let start = marker.saturating_sub(usize::from(
        marker > 0 && lines[marker - 1].trim().is_empty(),
    ));
    let mut kept = lines[..start].to_vec();
    kept.extend_from_slice(&lines[end..]);
    let mut result = kept.join("\n");
    if !result.is_empty() && contents.ends_with('\n') {
        result.push('\n');
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_removes_the_marked_block() {
        let rc = "export PATH=/x\n\n# added by navi setup\ncommand -v navi >/dev/null && source <(navi completions bash)\n";
        assert_eq!(strip_completion_block(rc).unwrap(), "export PATH=/x\n");
    }

    #[test]
    fn strip_leaves_content_after_the_block() {
        let rc = "# added by navi setup\ncommand -v navi >/dev/null && source <(navi completions zsh)\nalias g=git\n";
        assert_eq!(strip_completion_block(rc).unwrap(), "alias g=git\n");
    }

    #[test]
    fn strip_keeps_hand_edited_line_after_orphaned_marker() {
        let rc = "# added by navi setup\nalias g=git\n";
        assert_eq!(strip_completion_block(rc).unwrap(), "alias g=git\n");
    }

    #[test]
    fn strip_without_marker_is_none() {
        assert_eq!(strip_completion_block("export PATH=/x\n"), None);
    }
}
