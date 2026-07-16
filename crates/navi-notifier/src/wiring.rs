//! Config-driven construction of the engine's sources and notifiers, the "plugin
//! registry" seam. Adding a provider means adding a branch here plus its crate.

use std::sync::Arc;

use anyhow::{Context, Result};
use navi_notifier_core::traits::{Notifier, Source, StateStore};
use navi_notifier_core::{Engine, RuleEngine};
use navi_notifier_github::{GitHubSource, GitHubSourceConfig};
use navi_notifier_slack::{SlackNotifier, SlackNotifierConfig};

use crate::config::{Config, SlackConfig};

/// Build the fully-wired engine from config and a state store.
pub fn build_engine(config: &Config, state: Arc<dyn StateStore>) -> Result<Engine> {
    let mut sources: Vec<Arc<dyn Source>> = Vec::new();
    if config.github.enabled {
        let token = config.github.resolve_token()?;
        let source = GitHubSource::new(GitHubSourceConfig {
            token,
            api_base: config.github.api_base.clone(),
        })
        .context("initializing GitHub source")?;
        sources.push(Arc::new(source));
    }

    let mut notifiers: Vec<Arc<dyn Notifier>> = Vec::new();
    if config.slack.enabled {
        notifiers.push(Arc::new(build_slack(&config.slack)?));
    }

    anyhow::ensure!(!sources.is_empty(), "no sources enabled in config");
    anyhow::ensure!(!notifiers.is_empty(), "no notifiers enabled in config");

    let rules = RuleEngine::new(config.rules.clone());
    Ok(Engine::new(
        sources,
        notifiers,
        config.engine_routes(),
        rules,
        state,
    ))
}

/// Build the Slack notifier, shared by the engine and `test-slack`.
pub fn build_slack(config: &SlackConfig) -> Result<SlackNotifier> {
    let token = config.resolve_token()?;
    SlackNotifier::new(SlackNotifierConfig {
        token,
        dm_to: config.dm_to.clone(),
        api_base: None,
    })
    .context("initializing Slack notifier")
}
