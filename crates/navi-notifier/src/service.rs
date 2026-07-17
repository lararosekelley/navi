//! `navi service`: install, remove, or check the background service that runs
//! `navi run` on login. A systemd user unit on Linux, a launchd agent on macOS,
//! and a Task Scheduler logon task on Windows.
//!
//! The service does not inherit your interactive shell's environment, so tokens
//! have to reach it another way: on Linux/macOS the generated unit sources a
//! `navi.env` file kept next to the config (created here, chmod 600); on Windows
//! the logon task inherits user-scope variables you set with `setx`.

use std::env;
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::prompt::confirm;

/// launchd label and Task Scheduler task name; also used to locate them again.
const LAUNCHD_LABEL: &str = "dev.navi.navi";
const WINDOWS_TASK_NAME: &str = "Navi";

/// Install and start the background service for the current OS.
pub fn install(config_path: &Path, yes: bool) -> Result<()> {
    let exe = current_exe_string()?;
    match env::consts::OS {
        "linux" => install_systemd(&exe, config_path, yes),
        "macos" => install_launchd(&exe, config_path, yes),
        "windows" => install_task(&exe, config_path, yes),
        other => bail!(
            "no background-service integration for {other}; run `navi run` under your own supervisor"
        ),
    }
}

/// Stop and remove the background service for the current OS.
pub fn uninstall(yes: bool) -> Result<()> {
    match env::consts::OS {
        "linux" => uninstall_systemd(yes),
        "macos" => uninstall_launchd(yes),
        "windows" => uninstall_task(yes),
        other => bail!("no background-service integration for {other}"),
    }
}

/// Report whether the background service is installed and running.
pub fn status() -> Result<()> {
    match env::consts::OS {
        "linux" => status_systemd(),
        "macos" => status_launchd(),
        "windows" => status_task(),
        other => bail!("no background-service integration for {other}"),
    }
}

/// Offer to install the service right after `navi init`, but only at a real
/// terminal on a supported OS. Declining is a no-op with a hint.
pub fn offer_after_init(config_path: &Path) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        return Ok(());
    }
    let kind = match env::consts::OS {
        "linux" => "a systemd user service",
        "macos" => "a launchd agent",
        "windows" => "a Task Scheduler logon task",
        _ => return Ok(()),
    };
    if confirm(&format!(
        "Run navi in the background on login? Installs {kind}. [y/N] "
    ))? {
        install(config_path, true)?;
    } else {
        println!("Skipped; enable it later with `navi service install`.");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Linux: systemd user service
// ---------------------------------------------------------------------------

fn install_systemd(exe: &str, config: &Path, yes: bool) -> Result<()> {
    let home = home_dir()?;
    let unit_dir = home.join(".config/systemd/user");
    let unit_path = unit_dir.join("navi.service");
    let env_file = env_file_path(config);

    if !approve(
        yes,
        &format!(
            "install systemd user service at {} and enable it? [y/N] ",
            unit_path.display()
        ),
    )? {
        return Ok(());
    }

    fs::create_dir_all(&unit_dir)
        .with_context(|| format!("failed to create {}", unit_dir.display()))?;
    fs::write(&unit_path, systemd_unit(exe, config, &env_file))
        .with_context(|| format!("failed to write {}", unit_path.display()))?;
    ensure_env_file(&env_file)?;

    run(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
    run(Command::new("systemctl").args(["--user", "enable", "--now", "navi.service"]))?;

    println!("installed and started navi.service");
    print_env_hint(&env_file, "systemctl --user restart navi.service");
    println!("logs:  journalctl --user -u navi -f");
    println!("to keep it running while logged out:  loginctl enable-linger \"$USER\"");
    Ok(())
}

fn uninstall_systemd(yes: bool) -> Result<()> {
    let unit_path = home_dir()?.join(".config/systemd/user/navi.service");
    if !unit_path.exists() {
        println!("no navi.service found at {}", unit_path.display());
        return Ok(());
    }
    if !approve(yes, "stop and remove navi.service? [y/N] ")? {
        return Ok(());
    }
    // Best effort: the unit may already be stopped.
    let _ = Command::new("systemctl")
        .args(["--user", "disable", "--now", "navi.service"])
        .status();
    fs::remove_file(&unit_path)
        .with_context(|| format!("failed to remove {}", unit_path.display()))?;
    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    println!("removed {}", unit_path.display());
    println!("(the navi.env token file, if any, is left in place)");
    Ok(())
}

fn status_systemd() -> Result<()> {
    let enabled = capture(Command::new("systemctl").args(["--user", "is-enabled", "navi.service"]));
    let active = capture(Command::new("systemctl").args(["--user", "is-active", "navi.service"]));
    println!(
        "navi.service: {} ({})",
        active.as_deref().unwrap_or("unknown"),
        enabled.as_deref().unwrap_or("unknown")
    );
    Ok(())
}

/// The `[Unit]`/`[Service]` file. Paths are double-quoted so spaces survive.
fn systemd_unit(exe: &str, config: &Path, env_file: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=navi: focused PR-review alerts\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart=\"{exe}\" run --config \"{config}\"\n\
         EnvironmentFile=-{env_file}\n\
         Restart=on-failure\n\
         RestartSec=10\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exe = exe,
        config = config.display(),
        env_file = env_file.display(),
    )
}

// ---------------------------------------------------------------------------
// macOS: launchd agent
// ---------------------------------------------------------------------------

fn install_launchd(exe: &str, config: &Path, yes: bool) -> Result<()> {
    let home = home_dir()?;
    let plist_path = home
        .join("Library/LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist"));
    let log = home.join("Library/Logs/navi.log");
    let env_file = env_file_path(config);

    if !approve(
        yes,
        &format!(
            "install launchd agent at {} and load it? [y/N] ",
            plist_path.display()
        ),
    )? {
        return Ok(());
    }

    if let Some(parent) = plist_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(&plist_path, launchd_plist(exe, config, &env_file, &log))
        .with_context(|| format!("failed to write {}", plist_path.display()))?;
    ensure_env_file(&env_file)?;

    // Reload cleanly if it was already loaded.
    let _ = Command::new("launchctl")
        .args(["unload", "-w"])
        .arg(&plist_path)
        .status();
    run(Command::new("launchctl")
        .args(["load", "-w"])
        .arg(&plist_path))?;

    println!("installed and loaded {LAUNCHD_LABEL}");
    print_env_hint(
        &env_file,
        &format!(
            "launchctl unload -w {p} && launchctl load -w {p}",
            p = plist_path.display()
        ),
    );
    println!("logs:  tail -f {}", log.display());
    Ok(())
}

fn uninstall_launchd(yes: bool) -> Result<()> {
    let plist_path = home_dir()?
        .join("Library/LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist"));
    if !plist_path.exists() {
        println!("no launchd agent found at {}", plist_path.display());
        return Ok(());
    }
    if !approve(yes, "unload and remove the navi launchd agent? [y/N] ")? {
        return Ok(());
    }
    let _ = Command::new("launchctl")
        .args(["unload", "-w"])
        .arg(&plist_path)
        .status();
    fs::remove_file(&plist_path)
        .with_context(|| format!("failed to remove {}", plist_path.display()))?;
    println!("removed {}", plist_path.display());
    println!("(the navi.env token file, if any, is left in place)");
    Ok(())
}

fn status_launchd() -> Result<()> {
    let loaded = Command::new("launchctl")
        .args(["list", LAUNCHD_LABEL])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    println!(
        "{LAUNCHD_LABEL}: {}",
        if loaded { "loaded" } else { "not loaded" }
    );
    Ok(())
}

/// The launchd plist. `ProgramArguments` runs a shell that sources the env file
/// (launchd has no `EnvironmentFile`) and then execs navi.
fn launchd_plist(exe: &str, config: &Path, env_file: &Path, log: &Path) -> String {
    let run = format!(
        "set -a; [ -f \"{env}\" ] && . \"{env}\"; exec \"{exe}\" run --config \"{config}\"",
        env = env_file.display(),
        exe = exe,
        config = config.display(),
    );
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \x20   <key>Label</key><string>{label}</string>\n\
         \x20   <key>ProgramArguments</key>\n\
         \x20   <array>\n\
         \x20       <string>/bin/sh</string>\n\
         \x20       <string>-c</string>\n\
         \x20       <string>{run}</string>\n\
         \x20   </array>\n\
         \x20   <key>RunAtLoad</key><true/>\n\
         \x20   <key>KeepAlive</key><true/>\n\
         \x20   <key>StandardOutPath</key><string>{log}</string>\n\
         \x20   <key>StandardErrorPath</key><string>{log}</string>\n\
         </dict>\n\
         </plist>\n",
        label = LAUNCHD_LABEL,
        run = xml_escape(&run),
        log = xml_escape(&log.display().to_string()),
    )
}

// ---------------------------------------------------------------------------
// Windows: Task Scheduler logon task
// ---------------------------------------------------------------------------

fn install_task(exe: &str, config: &Path, yes: bool) -> Result<()> {
    if !approve(
        yes,
        &format!("register the '{WINDOWS_TASK_NAME}' logon task to run navi at sign-in? [y/N] "),
    )? {
        return Ok(());
    }
    run(Command::new("schtasks").args(schtasks_create_args(exe, config)))?;

    println!("registered the '{WINDOWS_TASK_NAME}' task; navi will start at your next sign-in");
    println!("navi runs hidden (no console window). start it now with:");
    println!("  schtasks /Run /TN {WINDOWS_TASK_NAME}");
    println!("the task inherits your user environment, so set tokens persistently:");
    println!("  setx NAVI_GITHUB_TOKEN ghp_...");
    println!("  setx NAVI_SLACK_TOKEN xoxb-...");
    println!("(then sign out and back in so the task picks them up)");
    Ok(())
}

fn uninstall_task(yes: bool) -> Result<()> {
    if !approve(
        yes,
        &format!("delete the '{WINDOWS_TASK_NAME}' task? [y/N] "),
    )? {
        return Ok(());
    }
    run(Command::new("schtasks").args(["/Delete", "/TN", WINDOWS_TASK_NAME, "/F"]))?;
    println!("deleted the '{WINDOWS_TASK_NAME}' task");
    Ok(())
}

fn status_task() -> Result<()> {
    let found = Command::new("schtasks")
        .args(["/Query", "/TN", WINDOWS_TASK_NAME])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    println!(
        "{WINDOWS_TASK_NAME} task: {}",
        if found {
            "registered"
        } else {
            "not registered"
        }
    );
    Ok(())
}

/// Arguments to `schtasks` that register the logon task. The action runs navi
/// through a hidden-window PowerShell so no console appears at sign-in.
fn schtasks_create_args(exe: &str, config: &Path) -> Vec<String> {
    let action = format!(
        "powershell -WindowStyle Hidden -NoProfile -Command \"& '{exe}' run --config '{config}'\"",
        exe = exe,
        config = config.display(),
    );
    vec![
        "/Create".into(),
        "/TN".into(),
        WINDOWS_TASK_NAME.into(),
        "/SC".into(),
        "ONLOGON".into(),
        "/RL".into(),
        "LIMITED".into(),
        "/F".into(),
        "/TR".into(),
        action,
    ]
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn current_exe_string() -> Result<String> {
    let path = env::current_exe().context("cannot determine the navi binary path")?;
    Ok(path.to_string_lossy().into_owned())
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .context("cannot locate your home directory; set HOME")
}

/// The token env file lives next to the config so the two travel together.
fn env_file_path(config: &Path) -> PathBuf {
    config
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("navi.env")
}

/// Create a commented, 0600 env-file template if one is not already there.
fn ensure_env_file(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(path, ENV_TEMPLATE).with_context(|| format!("failed to write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to chmod 600 {}", path.display()))?;
    }
    Ok(())
}

const ENV_TEMPLATE: &str = "# navi service environment (chmod 600). One KEY=value per line.\n\
    # Fill in tokens for the sources and destinations you enabled in config.toml.\n\
    NAVI_GITHUB_TOKEN=\n\
    NAVI_SLACK_TOKEN=\n\
    # NAVI_GITLAB_TOKEN=\n\
    # NAVI_GITEA_TOKEN=\n\
    # NAVI_DISCORD_TOKEN=\n\
    # NAVI_EMAIL_PASSWORD=\n";

fn print_env_hint(env_file: &Path, restart_cmd: &str) {
    println!(
        "put your tokens in {} (already chmod 600):",
        env_file.display()
    );
    println!("  NAVI_GITHUB_TOKEN=ghp_...");
    println!("  NAVI_SLACK_TOKEN=xoxb-...");
    println!("then restart the service:  {restart_cmd}");
}

/// Prompt for confirmation, mirroring `setup`: only ask at a real terminal;
/// non-interactive without `--yes` skips cleanly rather than reading EOF as "no".
fn approve(yes: bool, message: &str) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        println!("non-interactive; re-run with --yes to install without a prompt");
        return Ok(false);
    }
    let proceed = confirm(message)?;
    if !proceed {
        println!("cancelled");
    }
    Ok(proceed)
}

fn run(cmd: &mut Command) -> Result<()> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let status = cmd
        .status()
        .with_context(|| format!("failed to run {program}"))?;
    if !status.success() {
        bail!("{program} exited with {status}");
    }
    Ok(())
}

/// Run a command and return its trimmed stdout, or `None` if it could not run.
fn capture(cmd: &mut Command) -> Option<String> {
    let output = cmd.output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_unit_wires_exe_config_and_env_file() {
        let unit = systemd_unit(
            "/home/me/.cargo/bin/navi",
            Path::new("/home/me/.config/navi/config.toml"),
            Path::new("/home/me/.config/navi/navi.env"),
        );
        assert!(unit
            .contains("ExecStart=\"/home/me/.cargo/bin/navi\" run --config \"/home/me/.config/navi/config.toml\""));
        assert!(unit.contains("EnvironmentFile=-/home/me/.config/navi/navi.env"));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn launchd_plist_sources_env_then_execs_navi() {
        let plist = launchd_plist(
            "/usr/local/bin/navi",
            Path::new("/Users/me/.config/navi/config.toml"),
            Path::new("/Users/me/.config/navi/navi.env"),
            Path::new("/Users/me/Library/Logs/navi.log"),
        );
        assert!(plist.contains("<string>dev.navi.navi</string>"));
        assert!(plist.contains(". \"/Users/me/.config/navi/navi.env\""));
        assert!(plist.contains("exec \"/usr/local/bin/navi\" run"));
        assert!(plist.contains("/Users/me/Library/Logs/navi.log"));
    }

    #[test]
    fn schtasks_args_register_a_hidden_logon_task() {
        let args = schtasks_create_args(
            "C:\\navi.exe",
            Path::new("C:\\Users\\me\\navi\\config.toml"),
        );
        assert_eq!(args[0], "/Create");
        assert!(args.iter().any(|a| a == "ONLOGON"));
        assert!(args.iter().any(|a| a == WINDOWS_TASK_NAME));
        let action = args.last().unwrap();
        assert!(action.contains("WindowStyle Hidden"));
        assert!(action.contains("C:\\navi.exe"));
    }

    #[test]
    fn env_template_is_chmod_600_and_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("navi.env");
        ensure_env_file(&path).unwrap();
        assert!(path.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // A second call must not clobber user-entered tokens.
        fs::write(&path, "NAVI_GITHUB_TOKEN=secret\n").unwrap();
        ensure_env_file(&path).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "NAVI_GITHUB_TOKEN=secret\n"
        );
    }

    #[test]
    fn xml_escape_handles_ampersand_and_angles() {
        assert_eq!(xml_escape("a & b < c > d"), "a &amp; b &lt; c &gt; d");
    }
}
