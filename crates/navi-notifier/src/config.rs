//! Top-level on-disk configuration for the `navi` binary.
//!
//! This composes provider auth sections with the provider-agnostic
//! [`RuleConfig`](navi_notifier_core::RuleConfig) from `navi-notifier-core`. Secrets are resolved from
//! environment variables by default (`*_env` fields) so tokens never need to sit in
//! the config file.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use navi_notifier_core::{Backfill, RuleConfig};
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
    pub email: EmailConfig,
    pub rules: RuleConfig,
    /// Source→destination wiring. Empty means "every source to every destination".
    pub routes: Vec<RouteConfig>,
    pub digest: DigestConfig,
}

/// Batch low-signal event kinds into a periodic summary instead of alerting on
/// each one. Off by default.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DigestConfig {
    pub enabled: bool,
    /// How often to flush the digest, in seconds. The timer resets on daemon
    /// start, so after a restart a buffered digest waits up to this long before
    /// the next flush.
    pub interval_secs: u64,
    /// Event tags (e.g. `merged`, `closed`, `ready_for_review`) to batch instead
    /// of alerting immediately. Kinds not listed still alert in real time.
    pub kinds: Vec<String>,
}

impl Default for DigestConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_secs: 3600,
            kinds: Vec::new(),
        }
    }
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
    /// Hold a comment back until it is at least this many seconds old before
    /// notifying (0 = off). Lets a bot that posts a placeholder comment and edits it
    /// in place (e.g. "working…" → the finished review) settle to its final text so
    /// you get one accurate alert instead of the transient one. Costs up to this
    /// much delay on comment alerts.
    pub comment_min_age_secs: u64,
    /// How much pre-existing activity to surface on navi's very first poll, before
    /// it has any stored state. `review_requests` (default) shows PRs awaiting your
    /// review; `none` baselines silently; `all_open` backfills every involved PR.
    pub backfill: Backfill,
}

impl Default for General {
    fn default() -> Self {
        Self {
            poll_interval_secs: 60,
            log_level: "info".into(),
            utc_offset_minutes: 0,
            comment_min_age_secs: 0,
            backfill: Backfill::default(),
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
    /// Also poll your involved open PRs directly (via search), not just the
    /// notifications inbox. Catches reviews on your own PRs and activity in muted
    /// repos, which GitHub often doesn't surface as notifications.
    pub track_prs: bool,
    /// Mark a notification thread read once its event has been delivered. Off by
    /// default so navi doesn't touch your read/unread state unless you ask.
    pub mark_read: bool,
}

impl Default for GitHubConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            token_env: "NAVI_GITHUB_TOKEN".into(),
            token: None,
            track_prs: true,
            mark_read: false,
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
    /// Also poll your involved PRs directly (search), on top of notifications, so
    /// self-merges/closes and activity on your own PRs are caught. Matches
    /// `github.track_prs`.
    pub track_prs: bool,
}

impl Default for GiteaConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            token_env: "NAVI_GITEA_TOKEN".into(),
            token: None,
            api_base: None,
            track_prs: true,
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
    /// Event kinds (by tag) that break out of the PR thread: they still post in the
    /// thread but also surface at the top level (`reply_broadcast`), so high-signal
    /// events aren't buried. Empty = pure threading, nothing broadcasts.
    pub broadcast: Vec<String>,
}

impl Default for SlackConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            token_env: "NAVI_SLACK_TOKEN".into(),
            token: None,
            dm_to: "self".into(),
            broadcast: vec!["merged".into(), "closed".into(), "review_dismissed".into()],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscordConfig {
    /// Whether the Discord destination is active. Off by default; opt in.
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
#[serde(default)]
pub struct EmailConfig {
    /// Whether the email destination is active. Off by default; opt in.
    pub enabled: bool,
    pub smtp_host: String,
    pub smtp_port: u16,
    /// `"none"` (local sink), `"starttls"`, or `"implicit"`.
    pub tls: String,
    pub username: Option<String>,
    /// Env var holding the SMTP password.
    pub password_env: String,
    pub password: Option<String>,
    /// Sender, e.g. `navi <navi@example.com>`.
    pub from: String,
    /// Recipient, e.g. `you <you@example.com>`.
    pub to: String,
}

impl Default for EmailConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            smtp_host: String::new(),
            smtp_port: 587,
            tls: "starttls".into(),
            username: None,
            password_env: "NAVI_EMAIL_PASSWORD".into(),
            password: None,
            from: String::new(),
            to: String::new(),
        }
    }
}

impl EmailConfig {
    /// SMTP password from the inline value or env var.
    pub fn resolve_password(&self) -> Option<String> {
        if let Some(p) = self.password.as_deref().filter(|p| !p.is_empty()) {
            return Some(p.to_string());
        }
        std::env::var(&self.password_env)
            .ok()
            .filter(|v| !v.is_empty())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteConfig {
    pub source: String,
    /// `alias` keeps configs that used the older `notifier` key working.
    #[serde(alias = "notifier")]
    pub destination: String,
    /// Optional repo globs (`owner/name`, `owner/*`, `owner/prefix-*`, `*/prefix-*`).
    /// Empty = every repo from this source. When set, the route only fires for
    /// events whose repo matches one of them.
    #[serde(default)]
    pub repos: Vec<String>,
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
                destination: r.destination.clone(),
                repos: r.repos.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_providers_default_to_disabled() {
        // #12: nothing is on until the user (or `navi init`) opts in.
        assert!(!GitHubConfig::default().enabled);
        assert!(!GitLabConfig::default().enabled);
        assert!(!GiteaConfig::default().enabled);
        assert!(!SlackConfig::default().enabled);
        assert!(!DiscordConfig::default().enabled);
        assert!(!EmailConfig::default().enabled);
    }
}
