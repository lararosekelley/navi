//! `navi logs`: tail the background service's logs, wherever they live per-OS.
//! journald on Linux, the launchd log file on macOS, the Task Scheduler log file
//! on Windows. Targets the *service*; a foreground `navi run` logs to its own
//! terminal and has no persistent file.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

/// Show (and optionally follow) the last `lines` of the service's logs.
pub fn show(follow: bool, lines: usize) -> Result<()> {
    match std::env::consts::OS {
        "linux" => {
            let mut cmd = Command::new("journalctl");
            cmd.args(["--user", "-u", "navi", "-n", &lines.to_string()]);
            if follow {
                cmd.arg("-f");
            }
            run_pager(cmd)
        }
        "macos" => tail_file(&macos_log()?, follow, lines),
        "windows" => tail_windows(&windows_log()?, follow, lines),
        other => bail!("`navi logs` isn't supported on {other}"),
    }
}

fn tail_file(path: &Path, follow: bool, lines: usize) -> Result<()> {
    if !path.exists() {
        bail!(
            "no log at {} yet; is the service running? (check `navi service status`)",
            path.display()
        );
    }
    let mut cmd = Command::new("tail");
    cmd.args(["-n", &lines.to_string()]);
    if follow {
        cmd.arg("-f");
    }
    cmd.arg(path);
    run_pager(cmd)
}

fn tail_windows(path: &Path, follow: bool, lines: usize) -> Result<()> {
    if !path.exists() {
        bail!(
            "no log at {} yet; is the task running? (check `navi service status`)",
            path.display()
        );
    }
    // Escape single quotes (PowerShell doubles them inside a single-quoted string)
    // so a username like O'Brien can't break out of the literal; `-LiteralPath`
    // also stops the path from being glob-expanded.
    let literal = path.display().to_string().replace('\'', "''");
    let script = format!(
        "Get-Content -LiteralPath '{literal}' -Tail {lines}{}",
        if follow { " -Wait" } else { "" }
    );
    let mut cmd = Command::new("powershell");
    cmd.args(["-NoProfile", "-Command", &script]);
    run_pager(cmd)
}

/// Run a log viewer in the foreground. Its exit code is ignored (following logs
/// exits non-zero on Ctrl-C, which isn't an error); only a failure to launch is.
fn run_pager(mut cmd: Command) -> Result<()> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    cmd.status()
        .with_context(|| format!("failed to run {program}; is it installed?"))?;
    Ok(())
}

fn macos_log() -> Result<PathBuf> {
    Ok(home()?.join("Library/Logs/navi.log"))
}

fn windows_log() -> Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .context("LOCALAPPDATA is not set")?;
    Ok(base.join("navi").join("navi.log"))
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}
