//! `navi`: focused, configurable PR-review alerts from GitHub to Slack.

mod cli;
mod config;
mod state;
mod wiring;

use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::Parser;
use navi_notifier_core::model::{
    Actor, Event, EventKind, PullRequest, Repo, ReviewState, ViewerRelationship,
};
use navi_notifier_core::{Engine, EventOutcome, FilterContext, RunReport};
use time::OffsetDateTime;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, Command};
use crate::config::{resolve_config_path, resolve_state_path, Config};
use crate::state::SqliteStore;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_path = resolve_config_path(cli.config.clone())?;

    match cli.command {
        Command::Init { force } => cmd_init(&config_path, force),
        Command::Once { dry_run } => cmd_once(&config_path, dry_run).await,
        Command::Run => cmd_run(&config_path).await,
        Command::TestSlack => cmd_test_slack(&config_path).await,
    }
}

/// Load config and initialize logging from it. Shared by the runtime commands.
fn load_and_init_logging(config_path: &Path) -> Result<Config> {
    if !config_path.exists() {
        bail!(
            "no config at {}; run `navi init` first",
            config_path.display()
        );
    }
    let config = Config::load(config_path)?;
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(&config.general.log_level))
        .unwrap_or_else(|_| EnvFilter::new("info"));
    // `try_init` so repeated calls in tests don't panic.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
    Ok(config)
}

async fn open_engine(config: &Config) -> Result<(Engine, Arc<SqliteStore>)> {
    let state_path = resolve_state_path()?;
    let store = Arc::new(SqliteStore::open(&state_path).context("opening state store")?);
    let engine = wiring::build_engine(config, store.clone())?;
    Ok((engine, store))
}

/// Compute the current local time-of-day (minutes since midnight) for quiet hours.
fn filter_context(config: &Config) -> FilterContext {
    let now = OffsetDateTime::now_utc();
    let utc_minutes = now.hour() as i32 * 60 + now.minute() as i32;
    let local = (utc_minutes + config.general.utc_offset_minutes).rem_euclid(1440);
    FilterContext {
        local_minutes: Some(local as u16),
    }
}

async fn cmd_once(config_path: &Path, dry_run: bool) -> Result<()> {
    let config = load_and_init_logging(config_path)?;
    let (engine, _store) = open_engine(&config).await?;
    let report = engine.run_once(filter_context(&config), dry_run).await;
    print_report(&report, dry_run);
    Ok(())
}

async fn cmd_run(config_path: &Path) -> Result<()> {
    let config = load_and_init_logging(config_path)?;
    let (engine, _store) = open_engine(&config).await?;
    let interval = std::time::Duration::from_secs(config.general.poll_interval_secs.max(1));
    info!(
        interval_secs = interval.as_secs(),
        "navi daemon started; polling for review activity"
    );

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        let report = engine.run_once(filter_context(&config), false).await;
        if report.delivered_count() > 0 || !report.source_errors.is_empty() {
            info!(
                delivered = report.delivered_count(),
                errors = report.source_errors.len(),
                "poll pass complete"
            );
        }

        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = &mut shutdown => {
                info!("received shutdown signal; exiting");
                break;
            }
        }
    }
    Ok(())
}

async fn cmd_test_slack(config_path: &Path) -> Result<()> {
    let config = load_and_init_logging(config_path)?;
    let notifier = wiring::build_slack(&config.slack)?;
    let who = notifier
        .verify()
        .await
        .context("verifying Slack credentials (auth.test)")?;
    println!("Authenticated with Slack as {who}");

    use navi_notifier_core::traits::Notifier;
    notifier
        .send(&sample_event())
        .await
        .context("sending sample message")?;
    println!(
        "Sent a sample message to your configured Slack target ({}).",
        config.slack.dm_to
    );
    Ok(())
}

/// Print a human-readable summary of a run (used by `once`).
fn print_report(report: &RunReport, dry_run: bool) {
    if report.records.is_empty() {
        println!("No new events.");
    }
    for record in &report.records {
        let e = &record.event;
        let head = format!(
            "{} {}#{}",
            e.kind.tag(),
            e.pull_request.repo.full_name(),
            e.pull_request.number
        );
        let outcome = match &record.outcome {
            EventOutcome::Delivered { to } => format!("delivered → {}", to.join(", ")),
            EventOutcome::WouldDeliver { to } => format!("WOULD deliver → {}", to.join(", ")),
            EventOutcome::Suppressed(reason) => format!("suppressed ({reason:?})"),
            EventOutcome::AlreadyDelivered => "already delivered".to_string(),
            EventOutcome::DeliveryFailed { errors } => format!("FAILED: {}", errors.join("; ")),
        };
        println!("  {head:<40} {outcome}");
    }
    for (source, err) in &report.source_errors {
        warn!(source, %err, "source error during run");
    }
    if dry_run {
        println!("(dry run; nothing was sent and no state advanced)");
    }
}

/// Resolve when to stop the daemon: Ctrl-C, or SIGTERM on Unix.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

/// A representative event used by `test-slack` to exercise rendering + delivery.
fn sample_event() -> Event {
    Event {
        source_id: "github".into(),
        kind: EventKind::ReviewSubmitted {
            state: ReviewState::ChangesRequested,
        },
        pull_request: PullRequest {
            repo: Repo::new("acme", "widgets"),
            number: 42,
            title: "navi test message".into(),
            url: "https://github.com/acme/widgets/pull/42".into(),
            author: Actor::new("you"),
            draft: false,
        },
        viewer: ViewerRelationship {
            is_author: true,
            is_reviewer: false,
        },
        actor: Actor::new("navi"),
        occurred_at: OffsetDateTime::now_utc(),
        target_url: Some("https://github.com/acme/widgets/pull/42".into()),
        excerpt: Some("If you can read this, navi can DM you. 🎉".into()),
        dedup_key: "navi:test-slack".into(),
    }
}

/// Write a starter config file, creating the parent directory as needed.
fn cmd_init(path: &Path, force: bool) -> Result<()> {
    if path.exists() && !force {
        bail!(
            "config already exists at {} (use --force to overwrite)",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config directory {}", parent.display()))?;
    }
    std::fs::write(path, starter_config())
        .with_context(|| format!("writing config to {}", path.display()))?;
    println!("Wrote starter config to {}", path.display());
    println!("Next: set NAVI_GITHUB_TOKEN and NAVI_SLACK_TOKEN, then run `navi test-slack`.");
    Ok(())
}

/// A commented starter config. Hand-written (rather than serialized defaults) so it
/// can carry explanatory comments the user will actually read.
fn starter_config() -> String {
    debug_assert!(toml::from_str::<Config>(STARTER_CONFIG).is_ok());
    STARTER_CONFIG.to_string()
}

const STARTER_CONFIG: &str = r#"# navi configuration

[general]
# Seconds between poll passes when running `navi run`.
poll_interval_secs = 60
# Log filter: "info", or e.g. "navi=debug,octocrab=warn".
log_level = "info"
# Offset from UTC in minutes, used only for quiet-hours evaluation.
# e.g. -420 = US Pacific (PDT), 60 = Central Europe.
utc_offset_minutes = 0

[github]
enabled = true
# Env var holding a GitHub PAT with `notifications` + `repo` (read) scope.
token_env = "NAVI_GITHUB_TOKEN"
# For GitHub Enterprise Server, set api_base = "https://ghe.example.com/api/v3"

[gitlab]
# Off by default. Enable to get review-request and mention alerts from GitLab.
enabled = false
# Env var holding a GitLab PAT with `read_api` scope.
token_env = "NAVI_GITLAB_TOKEN"
# For self-hosted, set api_base = "https://gitlab.example.com/api/v4"

[slack]
enabled = true
# Env var holding a Slack bot token (xoxb-...). Needs chat:write + im:write.
token_env = "NAVI_SLACK_TOKEN"
# "self" DMs whoever the token authenticates as; or set a Slack user id like "U0123".
dm_to = "self"

[discord]
# Off by default. dm_to is either a webhook URL (simplest, no token) or a user id.
enabled = false
# Env var holding a bot token (needed only for user-DM mode, not webhooks).
token_env = "NAVI_DISCORD_TOKEN"
# dm_to = "https://discord.com/api/webhooks/..."   # webhook, or a user id like "123456789012345678"
dm_to = ""

[rules.events]
# Toggle individual alert kinds. Everything below defaults on except ready_for_review.
review_requested = true
re_review_requested = true
review_submitted = true
review_dismissed = true
comment_reply = true
mentioned = true
merged = true
closed = true
ready_for_review = false

[rules.repos]
# Empty allow = all repos. Patterns: "owner/name" or "owner/*". deny wins over allow.
allow = []
deny = []

[rules.quiet_hours]
enabled = false
start = "22:00"
end = "08:00"

[rules.merge_close]
# Whether to alert on merge/close for PRs you authored and/or reviewed.
author = true
reviewer = true

# mute_authors is a list of logins whose actions never notify (e.g. bots):
# [rules]
# mute_authors = ["dependabot[bot]"]

# Routes wire sources to notifiers. Omit this section entirely to send every
# source to every enabled notifier. List routes to be explicit, e.g. github+gitlab
# to slack, or github to discord:
[[routes]]
source = "github"
notifier = "slack"
"#;
