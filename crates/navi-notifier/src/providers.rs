//! `navi providers`: see what's wired up (`list`) and how to set a provider up
//! (`setup <name>`). The per-provider setup text is reusable by the guided init.

use anyhow::{bail, Result};

use crate::config::Config;

const SOURCES: [&str; 3] = ["github", "gitlab", "gitea"];
const DESTINATIONS: [&str; 3] = ["slack", "discord", "email"];

/// Print each source and destination with its on/off state and whether its
/// credentials resolve from config/env. Config-level only; no network (use
/// `navi doctor` for a live check).
pub fn list(config: &Config) {
    println!("sources:");
    for id in SOURCES {
        row(id, source_enabled(config, id), source_creds(config, id));
    }
    println!("\ndestinations:");
    for id in DESTINATIONS {
        row(id, dest_enabled(config, id), dest_creds(config, id));
    }
    println!("\nrun `navi providers setup <name>` for setup steps.");
}

fn row(name: &str, enabled: bool, creds: bool) {
    let state = if enabled { "on" } else { "off" };
    let creds = if creds {
        "credentials found"
    } else {
        "no credentials"
    };
    println!("  {name:<8} {state:<3}  {creds}");
}

// The `_` arms below only ever see ids from SOURCES/DESTINATIONS, so a fall-through
// means a const gained a provider whose arm was never added: panic loudly here
// rather than silently reporting it "off / no credentials".
fn source_enabled(config: &Config, id: &str) -> bool {
    match id {
        "github" => config.github.enabled,
        "gitlab" => config.gitlab.enabled,
        "gitea" => config.gitea.enabled,
        _ => unreachable!("no source_enabled arm for `{id}`"),
    }
}

fn source_creds(config: &Config, id: &str) -> bool {
    match id {
        "github" => config.github.resolve_token().is_ok(),
        "gitlab" => config.gitlab.resolve_token().is_ok(),
        "gitea" => config.gitea.resolve_token().is_ok(),
        _ => unreachable!("no source_creds arm for `{id}`"),
    }
}

fn dest_enabled(config: &Config, id: &str) -> bool {
    match id {
        "slack" => config.slack.enabled,
        "discord" => config.discord.enabled,
        "email" => config.email.enabled,
        _ => unreachable!("no dest_enabled arm for `{id}`"),
    }
}

fn dest_creds(config: &Config, id: &str) -> bool {
    match id {
        "slack" => config.slack.resolve_token().is_ok(),
        // Webhook mode (a URL in dm_to) needs no token; DM mode does.
        "discord" => {
            config.discord.dm_to.contains("://") || config.discord.resolve_token().is_some()
        }
        "email" => config.email.resolve_password().is_some(),
        _ => unreachable!("no dest_creds arm for `{id}`"),
    }
}

/// Print setup steps for a provider.
pub fn setup(name: &str) -> Result<()> {
    match setup_text(name) {
        Some(text) => {
            println!("{text}");
            Ok(())
        }
        None => {
            bail!("unknown provider `{name}` (github | gitlab | gitea | slack | discord | email)")
        }
    }
}

/// Per-provider setup instructions. Reused by the guided init in #12.
pub fn setup_text(name: &str) -> Option<&'static str> {
    let text = match name {
        "github" => {
            "GitHub setup:\n\
             1. Create a Personal Access Token: https://github.com/settings/tokens\n\
             2. Scopes: `notifications` + `repo` (read), plus `read:org` for team-review detection.\n\
             3. Export it as NAVI_GITHUB_TOKEN (or put it in navi.env next to your config).\n\
             4. `navi config set github.enabled true`, then `navi test --source github`."
        }
        "gitlab" => {
            "GitLab setup:\n\
             1. Create a token: https://gitlab.com/-/user_settings/personal_access_tokens\n\
             2. Scope: `read_api`.\n\
             3. Export it as NAVI_GITLAB_TOKEN. For self-hosted, set `gitlab.api_base` (…/api/v4).\n\
             4. `navi config set gitlab.enabled true`, then `navi test --source gitlab`."
        }
        "gitea" => {
            "Gitea/Forgejo setup:\n\
             1. Create a token in your instance's Settings → Applications.\n\
             2. Export it as NAVI_GITEA_TOKEN, and set `gitea.api_base` (…/api/v1).\n\
             3. `navi config set gitea.enabled true`, then `navi test --source gitea`."
        }
        "slack" => {
            "Slack setup:\n\
             1. Create an app: https://api.slack.com/apps (use the manifest in assets/app-info.yml\n\
             for the name/description/color).\n\
             2. Bot token scopes: `chat:write` + `im:write`. Install to your workspace.\n\
             3. Copy the `xoxb-…` bot token and export it as NAVI_SLACK_TOKEN.\n\
             4. Set `slack.dm_to` to \"self\" or your Slack user id (U…).\n\
             5. `navi config set slack.enabled true`, then `navi test --destination slack`."
        }
        "discord" => {
            "Discord setup (pick one mode):\n\
             • Webhook (simplest, no token): create a channel webhook and set `discord.dm_to`\n\
             to its https URL.\n\
             • Bot DM: create a bot, export its token as NAVI_DISCORD_TOKEN, and set `discord.dm_to`\n\
             to your user id (a numeric snowflake).\n\
             Then `navi config set discord.enabled true` and `navi test --destination discord`."
        }
        "email" => {
            "Email (SMTP) setup:\n\
             1. Set `email.smtp_host`, `email.smtp_port`, and `email.tls` (none | starttls | implicit).\n\
             2. Set `email.from` and `email.to`; export the SMTP password as NAVI_EMAIL_PASSWORD.\n\
             3. `navi config set email.enabled true`, then `navi test --destination email`."
        }
        _ => return None,
    };
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_text_covers_every_provider() {
        for id in SOURCES.iter().chain(DESTINATIONS.iter()) {
            assert!(setup_text(id).is_some(), "missing setup text for {id}");
        }
        assert!(setup_text("nope").is_none());
    }
}
