//! Command-line surface.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "navi",
    version,
    about = "A friendly helper to guide you through the day-to-day noise of code review",
    long_about = "navi watches your review activity across GitHub, GitLab, and \
                  Gitea and sends you a tight, high-signal stream to Slack, Discord, \
                  or email (review requests, replies to your comments, re-review \
                  requests, dismissals, merges and closes) without the noise of a \
                  forge's native integrations."
)]
pub struct Cli {
    /// Path to the config file (defaults to the platform config dir).
    #[arg(long, short, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Write a starter config file with sensible defaults.
    Init {
        /// Overwrite an existing config file.
        #[arg(long)]
        force: bool,
    },

    /// Run continuously as a daemon, polling on the configured interval.
    Run,

    /// Run a single poll pass and exit.
    Once {
        /// Report what would be delivered without sending anything or advancing state.
        #[arg(long)]
        dry_run: bool,
    },

    /// Verify a provider: send a sample to a destination, or poll a source and
    /// print what it derives (no state is touched). Give at least one.
    Test {
        /// Poll this source once and print the derived events, e.g. `github`.
        #[arg(long)]
        source: Option<String>,
        /// Send a sample message to this destination, e.g. `slack` or `slack.work`.
        #[arg(long)]
        destination: Option<String>,
    },

    /// Validate the config and report what each enabled provider can see.
    Doctor {
        /// Only run the offline config checks; skip the live provider probes.
        #[arg(long)]
        offline: bool,
    },

    /// Read or write config values without hand-editing config.toml. `set` adds a
    /// known key your config doesn't have yet, and refuses one navi doesn't know.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },

    /// Open the `navi.env` secrets file (beside the config) in $VISUAL/$EDITOR.
    Env,

    /// List providers and their status, or print setup steps for one.
    Providers {
        #[command(subcommand)]
        action: Option<ProvidersAction>,
    },

    /// Tail the background service's logs (journald / launchd / Task Scheduler).
    Logs {
        /// Follow the log, streaming new lines as they arrive.
        #[arg(long, short = 'f')]
        follow: bool,
        /// Number of past lines to show.
        #[arg(long, short = 'n', default_value_t = 50)]
        lines: usize,
    },

    /// Print a shell completion script (bash, zsh, fish, powershell, elvish).
    Completions {
        /// The shell to generate completions for.
        shell: clap_complete::Shell,
    },

    /// Install the man page and wire up shell completions (idempotent).
    Setup {
        /// Skip the confirmation prompt.
        #[arg(long, short = 'y')]
        yes: bool,
        /// Re-render generated assets only (used after an upgrade).
        #[arg(long)]
        refresh: bool,
    },

    /// Hold notifications for a while, then get them as one batch. Snoozes one
    /// repo by default (the current directory's), or an org or everything.
    #[command(args_conflicts_with_subcommands = true)]
    Snooze {
        #[command(subcommand)]
        action: Option<SnoozeAction>,
        /// How long: a number and a unit (s, m, h, d, w, mo, y), e.g. 45s, 4h, 2mo.
        #[arg(value_name = "DURATION", required = true)]
        duration: Option<String>,
        #[command(flatten)]
        target: SnoozeTarget,
    },

    /// Install, remove, or check the background service (systemd/launchd/Task Scheduler).
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },

    /// Reverse `setup` and the installer: completions, man page, config/receipt.
    Uninstall {
        /// Show what would be removed without removing anything.
        #[arg(long)]
        dry_run: bool,
        /// Skip the confirmation prompt.
        #[arg(long, short = 'y')]
        yes: bool,
    },

    /// Upgrade to the latest release (installer-managed copies only).
    Upgrade {
        /// Reinstall the latest release even if already up to date.
        #[arg(long)]
        force: bool,
        /// Build and install the latest unreleased commit (needs a Rust toolchain).
        #[arg(long)]
        head: bool,
        /// Don't restart the background service afterwards. (--head never restarts.)
        #[arg(long, conflicts_with = "head")]
        no_restart: bool,
    },

    /// Step back to an earlier release.
    Downgrade {
        /// Downgrade to a specific version instead of the previous release.
        #[arg(long, value_name = "VERSION")]
        to: Option<String>,
        /// Skip the confirmation prompt.
        #[arg(long, short = 'y')]
        yes: bool,
        /// Don't restart the background service afterwards.
        #[arg(long)]
        no_restart: bool,
    },

    /// Support navi's development on Ko-fi (opens your browser).
    Donate,
}

#[derive(Debug, Subcommand)]
pub enum ProvidersAction {
    /// Show each source/destination with its on-off state and whether creds resolve.
    List,
    /// Print setup steps for a provider (github|gitlab|gitea|slack|discord|email).
    Setup {
        /// The provider to explain.
        name: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum SnoozeAction {
    /// List active snoozes and the time left on each.
    Status,
    /// End a snooze early. Held notifications go out on the next poll.
    Off {
        #[command(flatten)]
        target: SnoozeTarget,
    },
}

/// What a snooze covers. With none of `--repo`, `--org` or `--global`, it covers
/// the repo of the current directory's git remote.
#[derive(Debug, Args)]
pub struct SnoozeTarget {
    /// A repo, as `owner/name`.
    #[arg(long, value_name = "OWNER/NAME", conflicts_with_all = ["org", "global"])]
    pub repo: Option<String>,
    /// Every repo in an org (on GitLab, a group and its subgroups).
    #[arg(long, value_name = "ORG", conflicts_with = "global")]
    pub org: Option<String>,
    /// Everything.
    #[arg(long)]
    pub global: bool,
    /// The provider the repo or org is on (github, gitlab, gitea). Needed when it
    /// can't be told from the git remote or the one enabled source.
    #[arg(long, value_name = "NAME", conflicts_with = "global")]
    pub provider: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum ServiceAction {
    /// Generate and enable a service that runs `navi run` on login.
    Install {
        /// Skip the confirmation prompt.
        #[arg(long, short = 'y')]
        yes: bool,
    },

    /// Stop and remove the background service.
    Uninstall {
        /// Skip the confirmation prompt.
        #[arg(long, short = 'y')]
        yes: bool,
    },

    /// Restart the background service so it re-reads config and navi.env.
    Restart,

    /// Show whether the background service is installed and running.
    Status,
}

#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// Print a config value by dotted key, e.g. `general.poll_interval_secs`.
    Get {
        /// Dotted path into config.toml, e.g. `github.enabled` or `slack.dm_to`.
        key: String,
    },
    /// Set a config value in place (comments and formatting preserved).
    Set {
        /// Dotted path, e.g. `github.enabled`.
        key: String,
        /// New value; parsed as bool/integer if it looks like one, else a string.
        value: String,
    },
    /// Open the config file in $VISUAL/$EDITOR.
    Edit,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Command, clap::Error> {
        Cli::try_parse_from(std::iter::once("navi").chain(args.iter().copied())).map(|c| c.command)
    }

    /// `main` treats a snooze with neither a subcommand nor a duration as
    /// unreachable, which holds only while clap enforces it.
    #[test]
    fn snooze_forms() {
        match parse(&["snooze", "4h"]).unwrap() {
            Command::Snooze {
                action: None,
                duration: Some(d),
                target,
            } => {
                assert_eq!(d, "4h");
                assert!(!target.global && target.repo.is_none() && target.org.is_none());
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            parse(&["snooze", "4h", "--global"]).unwrap(),
            Command::Snooze {
                action: None,
                duration: Some(_),
                target: SnoozeTarget { global: true, .. },
            }
        ));
        assert!(matches!(
            parse(&["snooze", "status"]).unwrap(),
            Command::Snooze {
                action: Some(SnoozeAction::Status),
                duration: None,
                ..
            }
        ));
        match parse(&["snooze", "off", "--repo", "a/b", "--provider", "github"]).unwrap() {
            Command::Snooze {
                action: Some(SnoozeAction::Off { target }),
                duration: None,
                ..
            } => {
                assert_eq!(target.repo.as_deref(), Some("a/b"));
                assert_eq!(target.provider.as_deref(), Some("github"));
            }
            other => panic!("{other:?}"),
        }

        for bad in [
            &["snooze"][..],
            &["snooze", "4h", "status"],
            &["snooze", "4h", "--repo", "a/b", "--global"],
            &["snooze", "4h", "--org", "acme", "--global"],
            &["snooze", "4h", "--global", "--provider", "github"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }
}
