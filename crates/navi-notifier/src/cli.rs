//! Command-line surface.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

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
        /// Send a sample message to this destination, e.g. `slack`.
        #[arg(long)]
        destination: Option<String>,
    },

    /// Report what each enabled provider can see (identity, visible orgs, creds).
    Doctor,

    /// Read or write config values without hand-editing config.toml.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
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
}
