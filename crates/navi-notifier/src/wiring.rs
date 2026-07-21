//! Config-driven construction of the engine's sources and destinations, the "plugin
//! registry" seam. Adding a provider means adding a branch here plus its crate.
//!
//! A new provider touches five spots that must stay in sync: the id list in
//! `build_engine`, its `*_enabled` arm, and its `build_source`/`build_destination`
//! arm. The catch-all arms return `false`/`bail!`, so a half-done addition
//! misbehaves quietly rather than failing to compile - keep them together.

use std::sync::Arc;

use anyhow::bail;
use anyhow::{Context, Result};
use navi_notifier_core::traits::{Destination, Source, StateStore};
use navi_notifier_core::{Engine, RuleEngine};
use navi_notifier_discord::{DiscordDestination, DiscordDestinationConfig};
use navi_notifier_email::{EmailDestination, EmailDestinationConfig, EmailTls};
use navi_notifier_gitea::{GiteaSource, GiteaSourceConfig};
use navi_notifier_github::{GitHubSource, GitHubSourceConfig};
use navi_notifier_gitlab::{GitLabSource, GitLabSourceConfig};
use navi_notifier_slack::{SlackDestination, SlackDestinationConfig};

use crate::config::{Config, SlackConfig};

/// Build the fully-wired engine from config and a state store.
pub fn build_engine(config: &Config, state: Arc<dyn StateStore>) -> Result<Engine> {
    let mut sources: Vec<Arc<dyn Source>> = Vec::new();
    for id in ["github", "gitlab", "gitea"] {
        if source_enabled(config, id) {
            sources.push(build_source(config, id)?);
        }
    }

    let mut destinations: Vec<Arc<dyn Destination>> = Vec::new();
    for id in ["slack", "discord", "email"] {
        if destination_enabled(config, id) {
            destinations.push(build_destination(config, id)?);
        }
    }

    anyhow::ensure!(!sources.is_empty(), "no sources enabled in config");
    anyhow::ensure!(
        !destinations.is_empty(),
        "no destinations enabled in config"
    );

    let rules = RuleEngine::new(config.rules.clone()).context("compiling mute patterns")?;
    let digest_kinds = if config.digest.enabled {
        config.digest.kinds.iter().cloned().collect()
    } else {
        std::collections::HashSet::new()
    };
    Ok(
        Engine::new(sources, destinations, config.engine_routes(), rules, state)
            .with_digest_kinds(digest_kinds),
    )
}

fn source_enabled(config: &Config, id: &str) -> bool {
    match id {
        "github" => config.github.enabled,
        "gitlab" => config.gitlab.enabled,
        "gitea" => config.gitea.enabled,
        _ => false,
    }
}

fn destination_enabled(config: &Config, id: &str) -> bool {
    match id {
        "slack" => config.slack.enabled,
        "discord" => config.discord.enabled,
        "email" => config.email.enabled,
        _ => false,
    }
}

/// Build a single source by id, regardless of its `enabled` flag (so `navi test`
/// can exercise one before you turn it on).
pub fn build_source(config: &Config, id: &str) -> Result<Arc<dyn Source>> {
    match id {
        "github" => Ok(Arc::new(
            GitHubSource::new(GitHubSourceConfig {
                token: config.github.resolve_token()?,
                api_base: config.github.api_base.clone(),
                track_prs: config.github.track_prs,
                mark_read: config.github.mark_read,
                comment_min_age_secs: config.general.comment_min_age_secs,
            })
            .context("initializing GitHub source")?,
        )),
        "gitlab" => Ok(Arc::new(
            GitLabSource::new(GitLabSourceConfig {
                token: config.gitlab.resolve_token()?,
                api_base: config.gitlab.api_base.clone(),
            })
            .context("initializing GitLab source")?,
        )),
        "gitea" => Ok(Arc::new(
            GiteaSource::new(GiteaSourceConfig {
                token: config.gitea.resolve_token()?,
                api_base: config.gitea.api_base.clone(),
                comment_min_age_secs: config.general.comment_min_age_secs,
            })
            .context("initializing Gitea source")?,
        )),
        other => bail!("unknown source `{other}` (github | gitlab | gitea)"),
    }
}

/// Build a single destination by id, regardless of its `enabled` flag.
pub fn build_destination(config: &Config, id: &str) -> Result<Arc<dyn Destination>> {
    match id {
        "slack" => Ok(Arc::new(build_slack(&config.slack)?)),
        "discord" => Ok(Arc::new(
            DiscordDestination::new(DiscordDestinationConfig {
                token: config.discord.resolve_token(),
                dm_to: config.discord.dm_to.clone(),
                api_base: None,
            })
            .context("initializing Discord destination")?,
        )),
        "email" => {
            let tls = match config.email.tls.as_str() {
                "none" => EmailTls::None,
                "starttls" => EmailTls::StartTls,
                "implicit" => EmailTls::Implicit,
                other => bail!("unknown email tls mode `{other}` (use none|starttls|implicit)"),
            };
            Ok(Arc::new(
                EmailDestination::new(EmailDestinationConfig {
                    smtp_host: config.email.smtp_host.clone(),
                    smtp_port: config.email.smtp_port,
                    tls,
                    username: config.email.username.clone(),
                    password: config.email.resolve_password(),
                    from: config.email.from.clone(),
                    to: config.email.to.clone(),
                })
                .context("initializing email destination")?,
            ))
        }
        other => bail!("unknown destination `{other}` (slack | discord | email)"),
    }
}

/// Build the Slack destination, shared by the engine and `navi test`.
pub fn build_slack(config: &SlackConfig) -> Result<SlackDestination> {
    let token = config.resolve_token()?;
    SlackDestination::new(SlackDestinationConfig {
        token,
        dm_to: config.dm_to.clone(),
        api_base: None,
    })
    .context("initializing Slack destination")
}
