//! `navi upgrade` / `navi downgrade`, plus a once-a-day "newer release" hint.
//!
//! Upgrades re-run the cargo-dist installer for the target release (the same
//! mechanism `curl .../sh/navi | bash` uses), so there's no bundled updater or
//! extra TLS stack. `cargo install` copies should upgrade through cargo instead.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{env, fs, sync::mpsc, thread};

use anyhow::{bail, Context, Result};

use crate::prompt::confirm;

/// Source repo for release discovery and the installer artifacts.
const REPO: &str = "lararosekelley/navi";
const REPO_URL: &str = "https://github.com/lararosekelley/navi";
/// The first release that shipped `downgrade`, and the floor it can reach: going
/// below would strand the user on a binary with no `downgrade`. v0.1.4 shipped
/// without these commands, so the first release with them is 0.1.5. Bump this if
/// they first land in a different version.
const MIN_DOWNGRADE_VERSION: &str = "0.1.5";

/// Stamp next to the receipt; one release check per day. Opt out with
/// `NAVI_NO_UPDATE_CHECK=1`.
const UPDATE_CHECK_FILE: &str = "update-check";
const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// navi's config directory (where the update-check stamp lives).
pub(crate) fn config_dir() -> Option<PathBuf> {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("LOCALAPPDATA").map(PathBuf::from))
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("navi"))
}

pub fn upgrade(head: bool, _force: bool, no_restart: bool) -> Result<()> {
    if head {
        // --head cargo-installs to ~/.cargo/bin, a different path than the service
        // runs, so a restart wouldn't pick it up; leave the service alone.
        return upgrade_to_head();
    }
    println!("upgrading navi to the latest release");
    run_installer(None)?;
    println!("upgraded (re-open your shell if the version looks stale)");
    crate::service::restart_after_upgrade(!no_restart)?;
    Ok(())
}

fn upgrade_to_head() -> Result<()> {
    println!("--head builds and installs the latest unreleased commit from {REPO_URL}");
    println!("HEAD is a pre-release snapshot: it may be broken or untested");
    if !confirm("continue? [y/N] ")? {
        println!("upgrade cancelled");
        return Ok(());
    }
    let status = Command::new("cargo")
        .args(["install", "--git", REPO_URL, "--locked", "navi-notifier"])
        .status()
        .context("failed to run cargo; --head requires a Rust toolchain")?;
    if !status.success() {
        bail!("cargo install exited with status {status}");
    }
    println!("installed navi from HEAD; to return to a release, run: navi upgrade");
    Ok(())
}

/// Step back to an earlier release. Riskier than upgrading (an older binary may
/// not understand state a newer one wrote), so it confirms first and never goes
/// below [`MIN_DOWNGRADE_VERSION`].
pub fn downgrade(to: Option<String>, yes: bool, no_restart: bool) -> Result<()> {
    let installed = env!("CARGO_PKG_VERSION");
    let installed_version = parse_version(installed)
        .with_context(|| format!("could not parse the installed version {installed}"))?;
    let floor = parse_version(MIN_DOWNGRADE_VERSION).expect("floor is a valid version");

    if installed_version <= floor {
        println!("navi {installed} is the earliest release `downgrade` can reach; nothing older");
        return Ok(());
    }

    let requested = match &to {
        Some(to) => Some(parse_version(to).with_context(|| format!("not a version: {to}"))?),
        None => None,
    };
    let available = match requested {
        Some(_) => Vec::new(),
        None => remote_release_versions()?,
    };
    let target = version_string(resolve_target(
        installed_version,
        floor,
        requested,
        &available,
    )?);

    println!("downgrade navi {installed} -> {target}");
    println!("a release older than {installed} may not understand state a newer one wrote");
    if !yes && !confirm("continue? [y/N] ")? {
        println!("downgrade cancelled");
        return Ok(());
    }
    run_installer(Some(&target))?;
    println!("downgraded to {target}; to move forward again, run: navi upgrade");
    crate::service::restart_after_upgrade(!no_restart)?;
    Ok(())
}

/// Re-run the cargo-dist installer for `version` (or the latest release when
/// `None`). Uses the shell installer on Unix and the PowerShell one on Windows.
fn run_installer(version: Option<&str>) -> Result<()> {
    let base = match version {
        Some(v) => format!("{REPO_URL}/releases/download/v{v}/navi-notifier-installer"),
        None => format!("{REPO_URL}/releases/latest/download/navi-notifier-installer"),
    };
    let status = if cfg!(windows) {
        Command::new("powershell")
            .args([
                "-ExecutionPolicy",
                "Bypass",
                "-c",
                &format!("irm {base}.ps1 | iex"),
            ])
            .status()
            .context("failed to run the PowerShell installer")?
    } else {
        Command::new("sh")
            .arg("-c")
            .arg(format!(
                "curl --proto '=https' --tlsv1.2 -LsSf {base}.sh | sh"
            ))
            .status()
            .context("failed to run the installer; is curl available?")?
    };
    if !status.success() {
        bail!("installer exited with status {status}");
    }
    Ok(())
}

/// Once a day, after a common command, print one line when a newer release
/// exists. Best effort with a hard time cap; anything unusual prints nothing.
pub fn maybe_hint_update() {
    if !std::io::stderr().is_terminal() {
        return;
    }
    if env::var("NAVI_NO_UPDATE_CHECK").is_ok_and(|v| !v.is_empty() && v != "0") {
        return;
    }
    let Some(path) = config_dir().map(|d| d.join(UPDATE_CHECK_FILE)) else {
        return;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|e| e.as_secs())
        .unwrap_or(0);
    if !should_check(fs::read_to_string(&path).ok().as_deref(), now) {
        return;
    }

    let installed = parse_version(env!("CARGO_PKG_VERSION"));
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let behind = installed
            .zip(
                remote_release_versions()
                    .ok()
                    .and_then(|v| v.into_iter().max()),
            )
            .is_some_and(|(installed, latest)| latest > installed);
        let _ = sender.send(behind);
    });

    if let Ok(behind) = receiver.recv_timeout(Duration::from_secs(5)) {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(&path, format!("checked={now}\n"));
        if behind {
            eprintln!("a newer navi release is available; run `navi upgrade`");
        }
    }
}

fn should_check(cache: Option<&str>, now: u64) -> bool {
    let Some(cache) = cache else {
        return true;
    };
    cache
        .lines()
        .find_map(|line| line.strip_prefix("checked="))
        .and_then(|value| value.trim().parse::<u64>().ok())
        .is_none_or(|checked| now.saturating_sub(checked) >= CHECK_INTERVAL_SECS)
}

type Version3 = (u64, u64, u64);

fn resolve_target(
    installed: Version3,
    floor: Version3,
    requested: Option<Version3>,
    available: &[Version3],
) -> Result<Version3> {
    match requested {
        Some(target) => {
            if target >= installed {
                bail!(
                    "{} is not older than the installed {}; use `navi upgrade` to move forward",
                    version_string(target),
                    version_string(installed)
                );
            }
            if target < floor {
                bail!(
                    "{} is below {}, the earliest release `downgrade` can reach",
                    version_string(target),
                    version_string(floor)
                );
            }
            Ok(target)
        }
        None => available
            .iter()
            .copied()
            .filter(|v| *v < installed && *v >= floor)
            .max()
            .with_context(|| {
                format!(
                    "no release between {} and {} to downgrade to",
                    version_string(floor),
                    version_string(installed)
                )
            }),
    }
}

fn parse_version(text: &str) -> Option<Version3> {
    let mut parts = text.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

fn version_string((major, minor, patch): Version3) -> String {
    format!("{major}.{minor}.{patch}")
}

/// Released versions, from the repo's `vX.Y.Z` tags.
fn remote_release_versions() -> Result<Vec<Version3>> {
    let output = Command::new("git")
        .args(["ls-remote", "--tags", REPO_URL])
        .output()
        .context("failed to list releases; check your network connection")?;
    if !output.status.success() {
        bail!("failed to fetch the release list from {REPO}");
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text
        .lines()
        .filter_map(|line| line.split("refs/tags/").nth(1))
        .filter(|tag| !tag.ends_with("^{}"))
        .filter_map(|tag| parse_version(tag.strip_prefix('v').unwrap_or(tag)))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_accepts_plain_xyz_only() {
        assert_eq!(parse_version("0.1.4"), Some((0, 1, 4)));
        assert_eq!(parse_version("10.0.3"), Some((10, 0, 3)));
        assert_eq!(parse_version("0.1"), None);
        assert_eq!(parse_version("0.1.4.1"), None);
        assert_eq!(parse_version("v0.1.4"), None);
    }

    #[test]
    fn resolve_target_requires_explicit_to_be_older() {
        let (installed, floor) = ((0, 2, 0), (0, 1, 4));
        assert!(resolve_target(installed, floor, Some((0, 2, 0)), &[]).is_err());
        assert!(resolve_target(installed, floor, Some((0, 2, 1)), &[]).is_err());
    }

    #[test]
    fn resolve_target_refuses_below_the_floor() {
        let (installed, floor) = ((0, 2, 0), (0, 1, 4));
        assert!(resolve_target(installed, floor, Some((0, 1, 3)), &[]).is_err());
        assert_eq!(
            resolve_target(installed, floor, Some((0, 1, 4)), &[]).unwrap(),
            (0, 1, 4)
        );
    }

    #[test]
    fn resolve_target_default_picks_previous_release() {
        let (installed, floor) = ((0, 2, 0), (0, 1, 4));
        let available = [(0, 1, 4), (0, 1, 5), (0, 2, 0)];
        assert_eq!(
            resolve_target(installed, floor, None, &available).unwrap(),
            (0, 1, 5)
        );
    }

    #[test]
    fn should_check_missing_or_garbled() {
        assert!(should_check(None, 1_000_000));
        assert!(should_check(Some("checked=nope\n"), 1_000_000));
    }

    #[test]
    fn should_check_once_per_day() {
        let stamp = format!("checked={}\n", 1_000_000u64);
        assert!(!should_check(Some(&stamp), 1_000_000 + 60));
        assert!(should_check(Some(&stamp), 1_000_000 + CHECK_INTERVAL_SECS));
    }
}
