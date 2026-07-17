//! Command-line surface.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "navi",
    version,
    about = "Focused, configurable PR-review alerts",
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

    /// Send a sample Block Kit message to verify Slack credentials and DM target.
    TestSlack,

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
    },

    /// Step back to an earlier release.
    Downgrade {
        /// Downgrade to a specific version instead of the previous release.
        #[arg(long, value_name = "VERSION")]
        to: Option<String>,
        /// Skip the confirmation prompt.
        #[arg(long, short = 'y')]
        yes: bool,
    },
}
