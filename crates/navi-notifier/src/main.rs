//! `navi`: focused, configurable PR-review alerts. Sources (GitHub, GitLab,
//! Gitea) and destinations (Slack, Discord, email) are provider crates wired
//! together here through the registry in `wiring`.

mod cli;
mod completions;
mod config;
mod config_cmd;
mod doctor;
mod editor;
mod envfile;
mod guided;
mod logs;
mod prompt;
mod providers;
mod service;
mod setup;
mod state;
mod test_cmd;
mod upgrade;
mod wiring;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::{CommandFactory, Parser};
use navi_notifier_core::{Engine, EventOutcome, FilterContext, RunReport};
use time::OffsetDateTime;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, Command, ConfigAction, ProvidersAction, ServiceAction};
use crate::config::{resolve_config_path, resolve_state_path, Config};
use crate::state::SqliteStore;

fn main() -> Result<()> {
    // Dynamic completion: when a shell's completer invokes us with COMPLETE=<shell>,
    // print candidates and exit instead of running a command.
    clap_complete::env::CompleteEnv::with_factory(Cli::command)
        .var(completions::COMPLETE_VAR)
        .complete();

    let cli = Cli::parse();
    let config_path = resolve_config_path(cli.config.clone())?;

    // Load navi.env before any async runtime starts, so populating the process
    // environment happens while we're still single-threaded (set_var is not safe
    // to call once the runtime's worker threads are up). navi.env is authoritative.
    envfile::load_beside_config(&config_path);

    dispatch(cli.command, config_path)
}

/// Route a command. Only the handful that actually `.await` spin up a Tokio runtime
/// (via `on_runtime`); the rest (config edits, service management, upgrades, `logs`)
/// run synchronously and never pay for one.
fn dispatch(command: Command, config_path: PathBuf) -> Result<()> {
    match command {
        Command::Init { force } => cmd_init(&config_path, force),
        Command::Once { dry_run } => on_runtime(cmd_once(&config_path, dry_run)),
        Command::Run => on_runtime(cmd_run(&config_path)),
        Command::Test {
            source,
            destination,
        } => on_runtime(cmd_test(&config_path, source, destination)),
        Command::Doctor { offline } => on_runtime(cmd_doctor(&config_path, offline)),
        Command::Config { action } => match action {
            ConfigAction::Get { key } => config_cmd::get(&config_path, &key),
            ConfigAction::Set { key, value } => config_cmd::set(&config_path, &key, &value),
            ConfigAction::Edit => config_cmd::edit(&config_path),
        },
        Command::Env => envfile::edit(&config_path),
        Command::Providers { action } => match action.unwrap_or(ProvidersAction::List) {
            ProvidersAction::List => {
                providers::list(&load_and_init_logging(&config_path)?);
                Ok(())
            }
            ProvidersAction::Setup { name } => providers::setup(&name),
        },
        Command::Logs { follow, lines } => logs::show(follow, lines),
        Command::Completions { shell } => completions::print(shell),
        Command::Setup { yes, refresh } => setup::setup(yes, refresh),
        Command::Service { action } => match action {
            ServiceAction::Install { yes } => service::install(&config_path, yes),
            ServiceAction::Uninstall { yes } => service::uninstall(yes),
            ServiceAction::Restart => service::restart(),
            ServiceAction::Status => service::status(),
        },
        Command::Uninstall { dry_run, yes } => setup::uninstall(dry_run, yes),
        Command::Upgrade {
            force,
            head,
            no_restart,
        } => upgrade::upgrade(head, force, no_restart),
        Command::Downgrade {
            to,
            yes,
            no_restart,
        } => upgrade::downgrade(to, yes, no_restart),
    }
}

/// Run an async command body on a fresh multi-thread Tokio runtime.
fn on_runtime<F: std::future::Future<Output = Result<()>>>(fut: F) -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the async runtime")?
        .block_on(fut)
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

async fn open_engine(config: &Config) -> Result<Engine> {
    let state_path = resolve_state_path()?;
    let store = Arc::new(SqliteStore::open(&state_path).context("opening state store")?);
    wiring::build_engine(config, store)
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
    let engine = open_engine(&config).await?;
    let ctx = filter_context(&config);
    let report = engine.run_once(ctx, dry_run).await;
    print_report(&report, dry_run);
    // A dry run advances nothing, so it must not release the buffer either.
    if !dry_run {
        let released = engine.flush_deferred(&ctx).await;
        if released > 0 {
            println!("  released {released} event(s) held by quiet hours");
        }
    }
    Ok(())
}

async fn cmd_run(config_path: &Path) -> Result<()> {
    let config = load_and_init_logging(config_path)?;
    let engine = open_engine(&config).await?;
    let interval = std::time::Duration::from_secs(config.general.poll_interval_secs.max(1));
    // Backstop against a provider that hangs instead of erroring: a pass that
    // overruns this is abandoned and retried next interval, rather than wedging
    // the daemon forever. Generous, so only a genuinely stuck pass trips it.
    let pass_timeout = (interval * 3).max(std::time::Duration::from_secs(300));
    info!(
        interval_secs = interval.as_secs(),
        "navi daemon started; polling for review activity"
    );
    upgrade::maybe_hint_update();

    let digest_interval = std::time::Duration::from_secs(config.digest.interval_secs.max(1));
    let mut last_digest = std::time::Instant::now();

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        // Abandoning a pass mid-flight can re-derive events next pass; the dedup
        // set stops anything already sent from sending twice.
        match tokio::time::timeout(
            pass_timeout,
            engine.run_once(filter_context(&config), false),
        )
        .await
        {
            Ok(report) => {
                if report.delivered_count() > 0 || !report.source_errors.is_empty() {
                    info!(
                        delivered = report.delivered_count(),
                        errors = report.source_errors.len(),
                        "poll pass complete"
                    );
                }
            }
            Err(_) => error!(
                timeout_secs = pass_timeout.as_secs(),
                "poll pass timed out; abandoning it and retrying next interval"
            ),
        }

        // Release anything quiet hours held once the window has passed. Checked
        // every pass rather than on a cadence of its own: the buffer read is cheap
        // when empty, and this bounds the delay after the window ends to one poll
        // interval.
        engine.flush_deferred(&filter_context(&config)).await;

        // Flush the digest on its own cadence, independent of the poll interval.
        if config.digest.enabled && last_digest.elapsed() >= digest_interval {
            engine.flush_digest().await;
            last_digest = std::time::Instant::now();
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

async fn cmd_test(
    config_path: &Path,
    source: Option<String>,
    destination: Option<String>,
) -> Result<()> {
    let config = load_and_init_logging(config_path)?;
    test_cmd::run(&config, source, destination).await
}

async fn cmd_doctor(config_path: &Path, offline: bool) -> Result<()> {
    let config = load_and_init_logging(config_path)?;
    doctor::doctor(&config, offline).await
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
            EventOutcome::Digested => "digested (batched for the next flush)".to_string(),
            EventOutcome::Deferred(reason) => {
                format!("deferred ({reason:?}); will send when the window ends")
            }
            EventOutcome::WouldDefer(reason) => format!("WOULD defer ({reason:?})"),
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
    guided::opt_in(path)?;
    service::offer_after_init(path)?;
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
# Log filter. At "info" each poll logs a one-line summary of what it examined
# (notifications, PRs swept, events derived) so `navi logs` shows whether navi saw
# your activity. For deeper tracing, name the crates, e.g.
# "info,navi_notifier_core=debug,navi_notifier_github=debug".
log_level = "info"
# Offset from UTC in minutes, used only for quiet-hours evaluation.
# e.g. -420 = US Pacific (PDT), 60 = Central Europe.
utc_offset_minutes = 0
# Hold a comment this many seconds before notifying (0 = off), so a bot that posts
# a placeholder and edits it in place (e.g. "Claude Code is working…" → the review)
# settles to its final text first. Costs up to this much delay on comment alerts.
comment_min_age_secs = 0
# What to surface on the very first poll, before navi has any stored state. Applies
# to every source (github, gitlab, gitea):
#   "review_requests" (default) - PRs currently awaiting your review
#   "none"                      - baseline silently, alert only on new activity
#   "all_open"                  - backfill every open PR you're involved in (noisy;
#                                 needs the involved-PR sweep: track_prs for github
#                                 and gitea; gitlab always sweeps)
backfill = "review_requests"

# Every provider starts disabled. `navi init` walks you through enabling the ones
# you want; or flip one yourself, e.g. `navi config set github.enabled true`.
[github]
enabled = false
# Env var holding a GitHub PAT with `notifications` + `repo` (read) scope.
token_env = "NAVI_GITHUB_TOKEN"
# For GitHub Enterprise Server, set api_base = "https://ghe.example.com/api/v3"
# Also poll your involved open PRs directly, not just the notifications inbox, so
# reviews on your PRs and activity in muted repos still reach you. Set false to
# rely on notifications only.
track_prs = true
# Mark a notification thread read once navi has delivered its event. Off by
# default so navi never touches your read/unread state unless you opt in.
mark_read = false

[gitlab]
# Off by default. Enable to get review-request and mention alerts from GitLab.
enabled = false
# Env var holding a GitLab PAT with `read_api` scope.
token_env = "NAVI_GITLAB_TOKEN"
# For self-hosted, set api_base = "https://gitlab.example.com/api/v4"

[gitea]
# Off by default. Works with Gitea and Forgejo.
enabled = false
token_env = "NAVI_GITEA_TOKEN"
# For your instance, set api_base = "https://gitea.example.com/api/v1"
# Also poll your involved PRs directly (search), not just the notifications inbox,
# so merges/closes you do yourself and activity on your own PRs still reach you.
track_prs = true

[slack]
enabled = false
# Env var holding a Slack bot token (xoxb-...). Needs chat:write + im:write.
token_env = "NAVI_SLACK_TOKEN"
# "self" DMs whoever the token authenticates as; or set a Slack user id like "U0123".
dm_to = "self"
# A PR's events are grouped into one Slack thread. These event kinds also surface at
# the top level (Slack reply_broadcast) so high-signal ones aren't buried in-thread.
# Set to [] for pure threading, or add kinds. Valid tags: review_requested,
# re_review_requested, review_submitted, review_dismissed, comment_reply, mentioned,
# merged, closed, ready_for_review, entered_merge_queue, removed_merge_queue,
# review_approved, review_changes_requested, review_commented.
# review_submitted covers every review; the review_approved / review_changes_requested /
# review_commented shorthands narrow it to a single state. Review *submissions* only
# broadcast for PRs you authored; review_dismissed still broadcasts (it's your review).
broadcast = ["merged", "closed", "review_dismissed", "review_approved", "review_changes_requested"]

[discord]
# Off by default. dm_to is either a webhook URL (simplest, no token) or a user id.
enabled = false
# Env var holding a bot token (needed only for user-DM mode, not webhooks).
token_env = "NAVI_DISCORD_TOKEN"
# dm_to = "https://discord.com/api/webhooks/..."   # webhook, or a user id like "123456789012345678"
dm_to = ""

[email]
# Off by default. Sends one message per event, threaded per PR.
enabled = false
smtp_host = "smtp.example.com"
smtp_port = 587
# "none" (local sink like Mailpit), "starttls" (587), or "implicit" (465).
tls = "starttls"
# username = "navi@example.com"
password_env = "NAVI_EMAIL_PASSWORD"
from = "navi <navi@example.com>"
to = "you <you@example.com>"

[rules.events]
# Toggle individual alert kinds; everything below is on by default.
review_requested = true
re_review_requested = true
review_submitted = true
review_dismissed = true
comment_reply = true
mentioned = true
merged = true
closed = true
ready_for_review = true
# Merge-queue events. navi auto-detects which repos use a merge queue and only alerts
# for those, so these are on by default; set either to false to mute it.
entered_merge_queue = true
removed_merge_queue = true

[rules.repos]
# Empty allow = all repos. Patterns: "owner/name", "owner/*" (whole owner),
# "owner/tmp-*" (name prefix), or "*/tmp-*" (any owner). deny wins over allow.
allow = []
# e.g. deny = ["me/git-stk-e2e-*"] to ignore ephemeral test repos.
deny = []

[rules.quiet_hours]
# Events that land inside this window are held, not dropped: they are delivered
# once the window ends. Nothing is lost to the night.
enabled = false
start = "22:00"
end = "08:00"

[rules.merge_close]
# Whether to alert on merge/close for PRs you authored and/or reviewed.
author = true
reviewer = true

# Per-repo overrides: change event toggles, quiet hours, or merge/close scope for
# matching repos. Unset fields inherit the global settings; first match wins. e.g.
# get ready-for-review alerts only in your work repos:
# [[rules.overrides]]
# repos = ["higharc/*"]
# events = { ready_for_review = true }
# merge_close = { reviewer = false }

# mute_authors is a list of logins whose actions never notify (e.g. bots):
# [rules]
# mute_authors = ["dependabot[bot]"]

# Pattern mutes filter by matching a field. `match` is author | title | excerpt;
# set regex = true for a regex, otherwise it's a case-insensitive substring.
# [[rules.mute]]
# match = "author"
# pattern = "[bot]"
#
# [[rules.mute]]
# match = "title"
# pattern = "^Bump "
# regex = true
#
# Or scope a mute with several fields at once - all must match. This mutes a bot's
# CI chatter without silencing its actual reviews:
# [[rules.mute]]
# author = "github-actions[bot]"
# excerpt = "CircleCI pipeline triggered"

# Routes wire sources to destinations. Omit this section entirely to send every
# source to every enabled destination. List routes to be explicit, e.g. github+gitlab
# to slack, or github to discord:
[[routes]]
source = "github"
destination = "slack"
# Add `repos` (same globs as rules.repos) to scope a route to certain repos; an
# event fans out to every matching route. e.g. work repos to Slack, personal to email:
# [[routes]]
# source = "github"
# destination = "email"
# repos = ["me/*"]
# Set `fallback = true` on a route to catch only events no other route claimed,
# "everything else". e.g. acme/* to Slack above, and the remainder to email:
# [[routes]]
# source = "github"
# destination = "email"
# fallback = true

# Digest: batch low-signal event kinds into one periodic summary instead of
# alerting on each. Off by default. Kinds not listed still alert in real time.
# [digest]
# enabled = true
# interval_secs = 3600
# kinds = ["ready_for_review", "merged", "closed"]
"#;
