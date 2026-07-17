//! Config-driven construction of the engine's sources and destinations, the "plugin
//! registry" seam. Adding a provider means adding a branch here plus its crate.

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
    if config.github.enabled {
        let source = GitHubSource::new(GitHubSourceConfig {
            token: config.github.resolve_token()?,
            api_base: config.github.api_base.clone(),
        })
        .context("initializing GitHub source")?;
        sources.push(Arc::new(source));
    }
    if config.gitlab.enabled {
        let source = GitLabSource::new(GitLabSourceConfig {
            token: config.gitlab.resolve_token()?,
            api_base: config.gitlab.api_base.clone(),
        })
        .context("initializing GitLab source")?;
        sources.push(Arc::new(source));
    }
    if config.gitea.enabled {
        let source = GiteaSource::new(GiteaSourceConfig {
            token: config.gitea.resolve_token()?,
            api_base: config.gitea.api_base.clone(),
        })
        .context("initializing Gitea source")?;
        sources.push(Arc::new(source));
    }

    let mut destinations: Vec<Arc<dyn Destination>> = Vec::new();
    if config.slack.enabled {
        destinations.push(Arc::new(build_slack(&config.slack)?));
    }
    if config.discord.enabled {
        let destination = DiscordDestination::new(DiscordDestinationConfig {
            token: config.discord.resolve_token(),
            dm_to: config.discord.dm_to.clone(),
            api_base: None,
        })
        .context("initializing Discord destination")?;
        destinations.push(Arc::new(destination));
    }
    if config.email.enabled {
        let tls = match config.email.tls.as_str() {
            "none" => EmailTls::None,
            "starttls" => EmailTls::StartTls,
            "implicit" => EmailTls::Implicit,
            other => bail!("unknown email tls mode `{other}` (use none|starttls|implicit)"),
        };
        let destination = EmailDestination::new(EmailDestinationConfig {
            smtp_host: config.email.smtp_host.clone(),
            smtp_port: config.email.smtp_port,
            tls,
            username: config.email.username.clone(),
            password: config.email.resolve_password(),
            from: config.email.from.clone(),
            to: config.email.to.clone(),
        })
        .context("initializing email destination")?;
        destinations.push(Arc::new(destination));
    }

    anyhow::ensure!(!sources.is_empty(), "no sources enabled in config");
    anyhow::ensure!(
        !destinations.is_empty(),
        "no destinations enabled in config"
    );

    let rules = RuleEngine::new(config.rules.clone());
    Ok(Engine::new(
        sources,
        destinations,
        config.engine_routes(),
        rules,
        state,
    ))
}

/// Build the Slack destination, shared by the engine and `test-slack`.
pub fn build_slack(config: &SlackConfig) -> Result<SlackDestination> {
    let token = config.resolve_token()?;
    SlackDestination::new(SlackDestinationConfig {
        token,
        dm_to: config.dm_to.clone(),
        api_base: None,
    })
    .context("initializing Slack destination")
}
