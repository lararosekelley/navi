//! Command-line surface.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "navi",
    version,
    about = "Focused, configurable PR-review alerts from GitHub to Slack",
    long_about = "navi watches your GitHub review activity and sends you a tight, \
                  high-signal Slack DM stream — review requests, replies to your \
                  comments, re-review requests, dismissals, merges and closes — \
                  without the noise of GitHub's native Slack app."
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
}
