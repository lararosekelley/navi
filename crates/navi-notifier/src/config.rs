//! Top-level on-disk configuration for the `navi` binary.
//!
//! This composes provider auth sections with the provider-agnostic
//! [`RuleConfig`](navi_notifier_core::RuleConfig) from `navi-notifier-core`. Secrets are resolved from
//! environment variables by default (`*_env` fields) so tokens never need to sit in
//! the config file.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use navi_notifier_core::RuleConfig;
use serde::{Deserialize, Serialize};

/// The full configuration tree.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub general: General,
    pub github: GitHubConfig,
    pub gitlab: GitLabConfig,
    pub gitea: GiteaConfig,
    pub slack: SlackConfig,
    pub discord: DiscordConfig,
    pub rules: RuleConfig,
    /// Source→notifier wiring. Empty means "every source to every notifier".
    pub routes: Vec<RouteConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    /// Seconds between poll passes when running as a daemon.
    pub poll_interval_secs: u64,
    /// `tracing` filter, e.g. `"info"` or `"navi=debug,octocrab=warn"`.
    pub log_level: String,
    /// Offset from UTC in minutes, used only to evaluate quiet hours in local time
    /// (e.g. `-420` for US Pacific, `60` for CET). Determining the OS local offset
    /// reliably inside a multithreaded runtime is unsound, so we take it explicitly.
    pub utc_offset_minutes: i32,
}

impl Default for General {
    fn default() -> Self {
        Self {
            poll_interval_secs: 60,
            log_level: "info".into(),
            utc_offset_minutes: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GitHubConfig {
    /// Whether the GitHub source is active.
    pub enabled: bool,
    /// Name of the environment variable holding the personal access token.
    pub token_env: String,
    /// Inline token (discouraged; prefer `token_env`). Overrides `token_env` if set.
    pub token: Option<String>,
    /// API base, override for GitHub Enterprise Server.
    pub api_base: Option<String>,
}

impl Default for GitHubConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            token_env: "NAVI_GITHUB_TOKEN".into(),
            token: None,
            api_base: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GitLabConfig {
    /// Whether the GitLab source is active. Off by default; opt in.
    pub enabled: bool,
    pub token_env: String,
    pub token: Option<String>,
    /// API base, e.g. `https://gitlab.example.com/api/v4` for self-hosted.
    pub api_base: Option<String>,
}

impl Default for GitLabConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            token_env: "NAVI_GITLAB_TOKEN".into(),
            token: None,
            api_base: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GiteaConfig {
    /// Whether the Gitea/Forgejo source is active. Off by default; opt in.
    pub enabled: bool,
    pub token_env: String,
    pub token: Option<String>,
    /// API base, e.g. `https://gitea.example.com/api/v1` (Gitea or Forgejo).
    pub api_base: Option<String>,
}

impl Default for GiteaConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            token_env: "NAVI_GITEA_TOKEN".into(),
            token: None,
            api_base: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SlackConfig {
    pub enabled: bool,
    /// Name of the environment variable holding the Slack bot token (`xoxb-…`).
    pub token_env: String,
    pub token: Option<String>,
    /// DM target: a Slack user id (`U…`) or the literal `"self"` to DM the user the
    /// bot token's `auth.test` resolves to.
    pub dm_to: String,
}

impl Default for SlackConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            token_env: "NAVI_SLACK_TOKEN".into(),
            token: None,
            dm_to: "self".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscordConfig {
    /// Whether the Discord notifier is active. Off by default; opt in.
    pub enabled: bool,
    /// Bot token env var (needed only for user-DM mode, not webhook mode).
    pub token_env: String,
    pub token: Option<String>,
    /// A webhook URL (`https://discord.com/api/webhooks/...`) or a user id to DM.
    pub dm_to: String,
}

impl Default for DiscordConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            token_env: "NAVI_DISCORD_TOKEN".into(),
            token: None,
            dm_to: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteConfig {
    pub source: String,
    pub notifier: String,
}

impl GitHubConfig {
    /// Resolve the token from the inline value or the named env var.
    pub fn resolve_token(&self) -> Result<String> {
        resolve_secret("github", self.token.as_deref(), &self.token_env)
    }
}

impl GitLabConfig {
    pub fn resolve_token(&self) -> Result<String> {
        resolve_secret("gitlab", self.token.as_deref(), &self.token_env)
    }
}

impl GiteaConfig {
    pub fn resolve_token(&self) -> Result<String> {
        resolve_secret("gitea", self.token.as_deref(), &self.token_env)
    }
}

impl SlackConfig {
    pub fn resolve_token(&self) -> Result<String> {
        resolve_secret("slack", self.token.as_deref(), &self.token_env)
    }
}

impl DiscordConfig {
    /// Optional token from the inline value or env var. `None` in webhook mode.
    pub fn resolve_token(&self) -> Option<String> {
        if let Some(t) = self.token.as_deref().filter(|t| !t.is_empty()) {
            return Some(t.to_string());
        }
        std::env::var(&self.token_env)
            .ok()
            .filter(|v| !v.is_empty())
    }
}

fn resolve_secret(what: &str, inline: Option<&str>, env_var: &str) -> Result<String> {
    if let Some(tok) = inline.filter(|t| !t.is_empty()) {
        return Ok(tok.to_string());
    }
    let val = std::env::var(env_var).map_err(|_| {
        anyhow!("{what} token not found: set env var `{env_var}` (or the inline `token` field)")
    })?;
    if val.is_empty() {
        return Err(anyhow!("{what} token env var `{env_var}` is empty"));
    }
    Ok(val)
}

impl Config {
    /// Load and parse the config file at `path`.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config at {}", path.display()))?;
        let cfg: Config = toml::from_str(&text)
            .with_context(|| format!("parsing config at {}", path.display()))?;
        Ok(cfg)
    }

    /// Convert config routes into engine routes.
    pub fn engine_routes(&self) -> Vec<navi_notifier_core::Route> {
        self.routes
            .iter()
            .map(|r| navi_notifier_core::Route {
                source: r.source.clone(),
                notifier: r.notifier.clone(),
            })
            .collect()
    }
}

/// Resolve the config file path: explicit `--config`, else the platform config dir
/// (`~/.config/navi/config.toml` on Linux).
pub fn resolve_config_path(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p);
    }
    let dirs = directories::ProjectDirs::from("dev", "navi", "navi")
        .ok_or_else(|| anyhow!("could not determine a config directory for this platform"))?;
    Ok(dirs.config_dir().join("config.toml"))
}

/// Resolve the state (database) file path under the platform data dir.
pub fn resolve_state_path() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("dev", "navi", "navi")
        .ok_or_else(|| anyhow!("could not determine a data directory for this platform"))?;
    Ok(dirs.data_dir().join("navi.sqlite3"))
}
