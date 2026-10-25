//! Top-level on-disk configuration for the `navi` binary.
//!
//! This composes provider auth sections with the provider-agnostic
//! [`RuleConfig`](navi_notifier_core::RuleConfig) from `navi-notifier-core`. Secrets are resolved from
//! environment variables by default (`*_env` fields) so tokens never need to sit in
//! the config file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use navi_notifier_core::{Backfill, RuleConfig};
use serde::de::{DeserializeOwned, Error as _};
use serde::{Deserialize, Deserializer, Serialize};

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
    /// Days navi can go without diffing a PR before its poll cursors are dropped.
    /// Never touches the snapshots, dedup records or merge-queue state that keep
    /// delivery exactly-once, so it can cost a re-fetch but never a duplicate.
    /// `0` keeps everything forever.
    pub state_retention_days: u32,
}

impl Default for General {
    fn default() -> Self {
        Self {
            poll_interval_secs: 60,
            log_level: "info".into(),
            utc_offset_minutes: 0,
            comment_min_age_secs: 0,
            backfill: Backfill::default(),
            state_retention_days: 90,
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
    /// Further Slack destinations, `[slack.<name>]`, each with the id `slack.<name>`.
    #[serde(
        flatten,
        deserialize_with = "named_instances",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub named: BTreeMap<String, SlackConfig>,
}

impl Default for SlackConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            token_env: "NAVI_SLACK_TOKEN".into(),
            token: None,
            dm_to: "self".into(),
            broadcast: vec![
                "merged".into(),
                "closed".into(),
                "review_dismissed".into(),
                "review_approved".into(),
                "review_changes_requested".into(),
            ],
            named: BTreeMap::new(),
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
    /// Further Discord destinations, `[discord.<name>]`, each with the id
    /// `discord.<name>`.
    #[serde(
        flatten,
        deserialize_with = "named_instances",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub named: BTreeMap<String, DiscordConfig>,
}

impl Default for DiscordConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            token_env: "NAVI_DISCORD_TOKEN".into(),
            token: None,
            dm_to: String::new(),
            named: BTreeMap::new(),
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
    /// Further email destinations, `[email.<name>]`, each with the id `email.<name>`.
    #[serde(
        flatten,
        deserialize_with = "named_instances",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub named: BTreeMap<String, EmailConfig>,
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
            named: BTreeMap::new(),
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
    /// When true, this route only receives events that no non-fallback route
    /// claimed: a catch-all for "everything else". Combine with scoped routes to
    /// send some repos one place and the remainder somewhere else.
    #[serde(default)]
    pub fallback: bool,
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

/// A destination section that can hold named instances of itself.
trait NamedDestination: DeserializeOwned {
    const KIND: &'static str;
    /// The secret env var field and its default. A named instance defaults it to
    /// `<default>_<NAME>`, so it never silently reuses the bare section's secret.
    const SECRET_ENV: (&'static str, &'static str);
    fn named(&self) -> &BTreeMap<String, Self>;
}

impl NamedDestination for SlackConfig {
    const KIND: &'static str = "slack";
    const SECRET_ENV: (&'static str, &'static str) = ("token_env", "NAVI_SLACK_TOKEN");
    fn named(&self) -> &BTreeMap<String, Self> {
        &self.named
    }
}

impl NamedDestination for DiscordConfig {
    const KIND: &'static str = "discord";
    const SECRET_ENV: (&'static str, &'static str) = ("token_env", "NAVI_DISCORD_TOKEN");
    fn named(&self) -> &BTreeMap<String, Self> {
        &self.named
    }
}

impl NamedDestination for EmailConfig {
    const KIND: &'static str = "email";
    const SECRET_ENV: (&'static str, &'static str) = ("password_env", "NAVI_EMAIL_PASSWORD");
    fn named(&self) -> &BTreeMap<String, Self> {
        &self.named
    }
}

/// Collect a destination section's sub-tables as named instances. Unknown scalar
/// keys are ignored, as they are everywhere else in the config.
fn named_instances<'de, D, T>(deserializer: D) -> Result<BTreeMap<String, T>, D::Error>
where
    D: Deserializer<'de>,
    T: NamedDestination,
{
    let raw = BTreeMap::<String, toml::Value>::deserialize(deserializer)?;
    let (env_field, env_default) = T::SECRET_ENV;
    let mut out = BTreeMap::new();
    for (name, value) in raw {
        let toml::Value::Table(mut table) = value else {
            continue;
        };
        let id = format!("{}.{name}", T::KIND);
        // The name becomes part of a destination id and an env var name.
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(D::Error::custom(format!(
                "`{id}`: destination names may only use letters, digits, `-` and `_`"
            )));
        }
        table.entry(env_field).or_insert_with(|| {
            toml::Value::String(format!(
                "{env_default}_{}",
                name.to_ascii_uppercase().replace('-', "_")
            ))
        });
        let instance = T::deserialize(toml::Value::Table(table))
            .map_err(|e| D::Error::custom(format!("`{id}`: {e}")))?;
        if let Some(nested) = instance.named().keys().next() {
            return Err(D::Error::custom(format!(
                "`{id}.{nested}`: named destinations can't be nested"
            )));
        }
        out.insert(name, instance);
    }
    Ok(out)
}

/// One destination's config section: a bare `[slack]` etc. or a named instance.
#[derive(Debug, Clone, Copy)]
pub enum DestinationConfig<'a> {
    Slack(&'a SlackConfig),
    Discord(&'a DiscordConfig),
    Email(&'a EmailConfig),
}

impl DestinationConfig<'_> {
    pub fn enabled(&self) -> bool {
        match self {
            Self::Slack(c) => c.enabled,
            Self::Discord(c) => c.enabled,
            Self::Email(c) => c.enabled,
        }
    }

    /// Whether the credentials resolve from config/env (no network).
    pub fn creds(&self) -> bool {
        match self {
            Self::Slack(c) => c.resolve_token().is_ok(),
            // Webhook mode (a URL in dm_to) needs no token; DM mode does.
            Self::Discord(c) => c.dm_to.contains("://") || c.resolve_token().is_some(),
            Self::Email(c) => c.resolve_password().is_some(),
        }
    }
}

/// Look up a destination by id: a kind from [`DESTINATION_IDS`], or `kind.name`
/// for a named instance.
pub fn destination<'a>(config: &'a Config, id: &str) -> Option<DestinationConfig<'a>> {
    fn pick<'a, T: NamedDestination>(base: &'a T, name: Option<&str>) -> Option<&'a T> {
        match name {
            None => Some(base),
            Some(name) => base.named().get(name),
        }
    }
    let (kind, name) = match id.split_once('.') {
        Some((kind, name)) => (kind, Some(name)),
        None => (id, None),
    };
    match kind {
        "slack" => pick(&config.slack, name).map(DestinationConfig::Slack),
        "discord" => pick(&config.discord, name).map(DestinationConfig::Discord),
        "email" => pick(&config.email, name).map(DestinationConfig::Email),
        _ => None,
    }
}

/// Every destination id in the config, enabled or not: each kind, followed by its
/// named instances.
pub fn destination_ids(config: &Config) -> Vec<String> {
    let mut ids = Vec::new();
    for kind in DESTINATION_IDS {
        let names: Vec<&String> = match kind {
            "slack" => config.slack.named.keys().collect(),
            "discord" => config.discord.named.keys().collect(),
            "email" => config.email.named.keys().collect(),
            _ => unreachable!("destination kind `{kind}` has no config section"),
        };
        ids.push(kind.to_string());
        ids.extend(names.into_iter().map(|name| format!("{kind}.{name}")));
    }
    ids
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
                fallback: r.fallback,
            })
            .collect()
    }
}

/// Severity of a [`Finding`] from [`validate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// A misconfiguration that will misbehave or drop events; `doctor` exits non-zero.
    Error,
    /// Legal but probably-not-intended; reported, doesn't fail.
    Warning,
}

/// One static-validation result: a severity and a human-facing message.
#[derive(Debug, Clone)]
pub struct Finding {
    pub severity: Severity,
    pub message: String,
}

impl Finding {
    fn error(message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            message: message.into(),
        }
    }
    fn warning(message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            message: message.into(),
        }
    }
}

/// The known source ids, in display order. Single source of truth for the provider
/// loops in `providers`, `wiring`, and `doctor`.
pub const SOURCE_IDS: [&str; 3] = ["github", "gitlab", "gitea"];
/// The destination kinds, in display order. Each is also the id of its bare
/// section; named instances are `kind.name` (see [`destination_ids`]).
pub const DESTINATION_IDS: [&str; 3] = ["slack", "discord", "email"];
/// Event tags accepted in `slack.broadcast` and `digest.kinds`: the `EventKind`
/// tags plus the per-state review shorthands. Kept in sync with the model by the
/// `known_tags_cover_every_event_tag` test.
const KNOWN_TAGS: [&str; 14] = [
    "review_requested",
    "re_review_requested",
    "review_submitted",
    "review_dismissed",
    "comment_reply",
    "mentioned",
    "merged",
    "closed",
    "ready_for_review",
    "entered_merge_queue",
    "removed_merge_queue",
    "review_approved",
    "review_changes_requested",
    "review_commented",
];

/// Statically validate a loaded config with no network or credentials: catch route
/// wiring mistakes, missing required fields, and malformed globs/tags before they
/// silently drop events at runtime. Complements `doctor`'s live credential checks.
pub fn validate(config: &Config) -> Vec<Finding> {
    let mut out = Vec::new();

    for r in &config.routes {
        if !SOURCE_IDS.contains(&r.source.as_str()) {
            out.push(Finding::error(format!(
                "route source `{}` is not a known source (github|gitlab|gitea)",
                r.source
            )));
        } else if !source_enabled(config, &r.source) {
            out.push(Finding::warning(format!(
                "route source `{}` is disabled; the route stays inert until you enable it",
                r.source
            )));
        }
        if destination(config, &r.destination).is_none() {
            out.push(Finding::error(format!(
                "route destination `{}` is not a known destination (slack|discord|email, or a named one like slack.<name>)",
                r.destination
            )));
        } else if !dest_enabled(config, &r.destination) {
            out.push(Finding::warning(format!(
                "route sends to `{}`, which is disabled; those events are dropped",
                r.destination
            )));
        }
        for pat in &r.repos {
            if !is_valid_repo_glob(pat) {
                out.push(Finding::error(format!(
                    "route repo glob `{pat}` is malformed (expected owner/name, e.g. acme/*)"
                )));
            }
        }
    }

    // With routes present, an enabled source no route references delivers nowhere.
    if !config.routes.is_empty() {
        for id in SOURCE_IDS {
            if source_enabled(config, id) && !config.routes.iter().any(|r| r.source == id) {
                out.push(Finding::warning(format!(
                    "source `{id}` is enabled but no route sends its events anywhere"
                )));
            }
        }
    }

    // Required fields for enabled providers.
    for id in destination_ids(config) {
        match destination(config, &id) {
            Some(DestinationConfig::Email(c)) if c.enabled => {
                for (field, val) in [
                    ("smtp_host", &c.smtp_host),
                    ("from", &c.from),
                    ("to", &c.to),
                ] {
                    if val.trim().is_empty() {
                        out.push(Finding::error(format!(
                            "{id} is enabled but {id}.{field} is empty"
                        )));
                    }
                }
            }
            Some(DestinationConfig::Slack(c)) if c.enabled && c.dm_to.trim().is_empty() => {
                out.push(Finding::error(format!(
                    "{id} is enabled but {id}.dm_to is empty"
                )));
            }
            Some(DestinationConfig::Discord(c)) if c.enabled && c.dm_to.trim().is_empty() => {
                out.push(Finding::error(format!(
                    "{id} is enabled but {id}.dm_to is empty (set a webhook URL or user id)"
                )));
            }
            _ => {}
        }
    }
    if config.gitea.enabled
        && config
            .gitea
            .api_base
            .as_deref()
            .unwrap_or("")
            .trim()
            .is_empty()
    {
        out.push(Finding::error(
            "gitea is enabled but gitea.api_base is unset (needs …/api/v1)".to_string(),
        ));
    }

    // Repo-filter globs.
    for pat in config
        .rules
        .repos
        .allow
        .iter()
        .chain(&config.rules.repos.deny)
    {
        if !is_valid_repo_glob(pat) {
            out.push(Finding::error(format!(
                "rules.repos glob `{pat}` is malformed (expected owner/name)"
            )));
        }
    }

    // Quiet-hours clock format.
    if config.rules.quiet_hours.enabled {
        for (field, val) in [
            ("start", &config.rules.quiet_hours.start),
            ("end", &config.rules.quiet_hours.end),
        ] {
            if !is_hhmm(val) {
                out.push(Finding::error(format!(
                    "rules.quiet_hours.{field} `{val}` is not a HH:MM time"
                )));
            }
        }
    }

    // Broadcast / digest event tags.
    for id in destination_ids(config) {
        let Some(DestinationConfig::Slack(c)) = destination(config, &id) else {
            continue;
        };
        for tag in &c.broadcast {
            if !KNOWN_TAGS.contains(&tag.as_str()) {
                out.push(Finding::warning(format!(
                    "{id}.broadcast has unknown tag `{tag}`; it will never match"
                )));
            }
        }
    }
    if config.digest.enabled {
        for tag in &config.digest.kinds {
            if !KNOWN_TAGS.contains(&tag.as_str()) {
                out.push(Finding::warning(format!(
                    "digest.kinds has unknown tag `{tag}`"
                )));
            }
        }
    }

    out
}

/// Whether the source with this id is enabled. Only ever called with an id from
/// [`SOURCE_IDS`] (callers guard with `SOURCE_IDS.contains` first), so an unknown
/// id is a wiring bug, not a runtime input.
pub fn source_enabled(config: &Config, id: &str) -> bool {
    match id {
        "github" => config.github.enabled,
        "gitlab" => config.gitlab.enabled,
        "gitea" => config.gitea.enabled,
        _ => unreachable!("source_enabled called with unknown id `{id}`"),
    }
}

/// Whether the destination with this id exists and is enabled.
pub fn dest_enabled(config: &Config, id: &str) -> bool {
    destination(config, id).is_some_and(|d| d.enabled())
}

/// Whether a source's credentials resolve from config/env (no network).
pub fn source_creds(config: &Config, id: &str) -> bool {
    match id {
        "github" => config.github.resolve_token().is_ok(),
        "gitlab" => config.gitlab.resolve_token().is_ok(),
        "gitea" => config.gitea.resolve_token().is_ok(),
        _ => false,
    }
}

/// Whether a destination's credentials resolve from config/env (no network).
pub fn dest_creds(config: &Config, id: &str) -> bool {
    destination(config, id).is_some_and(|d| d.creds())
}

/// An `owner/name` glob: one `/`, both sides non-empty (either may be/contain `*`).
fn is_valid_repo_glob(pattern: &str) -> bool {
    // Exactly one `/`, both sides non-empty: a real repo full name is `owner/name`,
    // so a multi-slash pattern like `acme/repo/sub` could never match anything.
    let mut parts = pattern.split('/');
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(owner), Some(name), None) if !owner.is_empty() && !name.is_empty()
    )
}

/// A 24-hour `HH:MM` clock time.
fn is_hhmm(s: &str) -> bool {
    matches!(s.split_once(':'), Some((h, m))
        if h.len() == 2 && m.len() == 2
        && h.parse::<u8>().is_ok_and(|h| h < 24)
        && m.parse::<u8>().is_ok_and(|m| m < 60))
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

    fn route(source: &str, destination: &str, repos: Vec<String>) -> RouteConfig {
        RouteConfig {
            source: source.into(),
            destination: destination.into(),
            repos,
            fallback: false,
        }
    }

    fn errors(findings: &[Finding]) -> usize {
        findings
            .iter()
            .filter(|f| f.severity == Severity::Error)
            .count()
    }

    #[test]
    fn default_config_validates_clean() {
        // The default (all off, default broadcast tags) has nothing to flag.
        assert!(validate(&Config::default()).is_empty());
    }

    #[test]
    fn flags_unknown_and_disabled_route_targets() {
        let mut c = Config::default();
        c.github.enabled = true;
        c.routes = vec![
            route("github", "bogus", vec![]), // unknown destination -> error
            route("github", "slack", vec![]), // known but disabled -> warning
        ];
        let f = validate(&c);
        assert!(f
            .iter()
            .any(|x| x.severity == Severity::Error && x.message.contains("bogus")));
        assert!(f
            .iter()
            .any(|x| x.severity == Severity::Warning && x.message.contains("slack")));
    }

    #[test]
    fn flags_missing_required_fields_and_bad_glob() {
        let mut c = Config::default();
        c.email.enabled = true; // smtp_host/from/to all empty by default
        c.rules.repos.deny = vec!["not-a-glob".into()];
        let f = validate(&c);
        // 3 empty email fields + 1 malformed glob.
        assert_eq!(errors(&f), 4);
        assert!(f.iter().any(|x| x.message.contains("email.smtp_host")));
        assert!(f.iter().any(|x| x.message.contains("not-a-glob")));
    }

    #[test]
    fn flags_empty_discord_dm_to_and_multislash_glob() {
        let mut c = Config::default();
        c.discord.enabled = true; // dm_to defaults to ""
        c.rules.repos.allow = vec!["acme/repo/sub".into()]; // more than one `/`
        let f = validate(&c);
        assert!(f
            .iter()
            .any(|x| x.severity == Severity::Error && x.message.contains("discord.dm_to")));
        assert!(f
            .iter()
            .any(|x| x.severity == Severity::Error && x.message.contains("acme/repo/sub")));
    }

    #[test]
    fn flags_bad_quiet_hours_time() {
        let mut c = Config::default();
        c.rules.quiet_hours.enabled = true;
        c.rules.quiet_hours.start = "9am".into();
        c.rules.quiet_hours.end = "08:00".into();
        let f = validate(&c);
        assert!(f
            .iter()
            .any(|x| x.severity == Severity::Error && x.message.contains("quiet_hours.start")));
        assert!(!f.iter().any(|x| x.message.contains("quiet_hours.end")));
    }

    #[test]
    fn configs_without_named_destinations_parse_as_before() {
        // A stray scalar under a destination section was ignored before named
        // instances existed and must stay ignored, not become an instance.
        let c: Config = toml::from_str(
            r#"
[slack]
enabled = true
dm_to = "U1"
retired_key = "x"

[[routes]]
source = "github"
destination = "slack"
"#,
        )
        .unwrap();
        assert!(c.slack.named.is_empty());
        assert_eq!(c.slack.token_env, "NAVI_SLACK_TOKEN");
        assert_eq!(destination_ids(&c), ["slack", "discord", "email"]);
        // And nothing new appears when it is written back.
        let v = toml::Value::try_from(&c).unwrap();
        assert!(v["slack"]
            .as_table()
            .unwrap()
            .values()
            .all(|v| !v.is_table()));
    }

    #[test]
    fn named_destinations_parse_with_their_own_secret_env() {
        let c: Config = toml::from_str(
            r#"
[slack]
enabled = true

[slack.grey-lock]
enabled = true
dm_to = "U2"

[slack.other]
token_env = "MY_TOKEN"

[email.work]
smtp_host = "smtp.example.com"
"#,
        )
        .unwrap();
        assert_eq!(
            destination_ids(&c),
            [
                "slack",
                "slack.grey-lock",
                "slack.other",
                "discord",
                "email",
                "email.work"
            ]
        );
        let named = &c.slack.named["grey-lock"];
        assert_eq!(named.token_env, "NAVI_SLACK_TOKEN_GREY_LOCK");
        assert_eq!(named.dm_to, "U2");
        // Unset fields take the usual defaults, not the bare section's values.
        assert_eq!(named.broadcast, SlackConfig::default().broadcast);
        assert_eq!(c.slack.named["other"].token_env, "MY_TOKEN");
        assert_eq!(
            c.email.named["work"].password_env,
            "NAVI_EMAIL_PASSWORD_WORK"
        );

        assert!(dest_enabled(&c, "slack.grey-lock"));
        assert!(!dest_enabled(&c, "slack.other"));
        assert!(destination(&c, "slack.missing").is_none());
        assert!(destination(&c, "nope").is_none());
        // Written back, the named table survives a round trip.
        let back: Config = toml::from_str(&toml::to_string(&c).unwrap()).unwrap();
        assert_eq!(back.slack.named["grey-lock"].dm_to, "U2");
    }

    #[test]
    fn rejects_bad_and_nested_destination_names() {
        let err = toml::from_str::<Config>("[slack.\"a.b\"]\nenabled = true\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("slack.a.b"), "{err}");
        let err = toml::from_str::<Config>("[discord.a.b]\nenabled = true\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("can't be nested"), "{err}");
    }

    #[test]
    fn validates_named_destinations_and_routes_to_them() {
        let mut c: Config = toml::from_str(
            r#"
[github]
enabled = true

[slack.work]
enabled = true
dm_to = ""
broadcast = ["nope"]
"#,
        )
        .unwrap();
        c.routes = vec![
            route("github", "slack.work", vec!["acme/*".into()]),
            route("github", "slack.missing", vec![]),
        ];
        let f = validate(&c);
        assert!(f
            .iter()
            .any(|x| x.severity == Severity::Error && x.message.contains("slack.work.dm_to")));
        assert!(
            f.iter()
                .any(|x| x.severity == Severity::Warning
                    && x.message.contains("slack.work.broadcast"))
        );
        assert!(f
            .iter()
            .any(|x| x.severity == Severity::Error && x.message.contains("slack.missing")));
        assert!(!f.iter().any(|x| x.message.contains("`slack.work` is not")));
    }

    #[test]
    fn default_broadcast_tags_are_all_known() {
        // If a new default broadcast tag isn't added to KNOWN_TAGS, this warns.
        let c = Config::default();
        let f = validate(&c);
        assert!(!f.iter().any(|x| x.message.contains("unknown tag")));
    }

    #[test]
    fn known_tags_cover_every_event_tag() {
        use navi_notifier_core::model::{EventKind, MergeQueueRemoval, ReviewState};
        // Every tag the model can emit must be a KNOWN_TAG, else a valid config value
        // gets a spurious "unknown tag" warning. Extend this list with new EventKinds.
        let kinds = [
            EventKind::ReviewRequested,
            EventKind::ReReviewRequested,
            EventKind::ReviewSubmitted {
                state: ReviewState::Approved,
            },
            EventKind::ReviewSubmitted {
                state: ReviewState::ChangesRequested,
            },
            EventKind::ReviewSubmitted {
                state: ReviewState::Commented,
            },
            EventKind::ReviewDismissed,
            EventKind::CommentReply {
                on_your_comment: true,
            },
            EventKind::Mentioned,
            EventKind::Merged,
            EventKind::Closed,
            EventKind::ReadyForReview,
            EventKind::EnteredMergeQueue,
            EventKind::RemovedFromMergeQueue {
                reason: MergeQueueRemoval::Dequeued,
            },
        ];
        for kind in &kinds {
            for tag in kind.match_tags() {
                assert!(KNOWN_TAGS.contains(&tag), "KNOWN_TAGS is missing `{tag}`");
            }
        }
    }
}
