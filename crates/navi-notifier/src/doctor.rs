//! `navi doctor`: report what each enabled provider can see, so silent
//! misconfiguration (e.g. a GitHub token that can't see an org because of SAML
//! SSO, or a destination with no credentials) is visible instead of looking like
//! navi being broken.

use anyhow::Result;
use navi_notifier_github::{GitHubSource, GitHubSourceConfig};

use crate::config::Config;

pub async fn doctor(config: &Config) -> Result<()> {
    println!("sources:");
    check_github(config).await;
    report(
        "gitlab",
        config.gitlab.enabled,
        config.gitlab.resolve_token().is_ok(),
    );
    report(
        "gitea",
        config.gitea.enabled,
        config.gitea.resolve_token().is_ok(),
    );

    println!("\ndestinations:");
    report(
        "slack",
        config.slack.enabled,
        config.slack.resolve_token().is_ok(),
    );
    report(
        "discord",
        config.discord.enabled,
        // Webhook mode (a URL in dm_to) is self-authenticating; user-id DM mode
        // needs a bot token. A bare user id without a token is a misconfig, not creds.
        config.discord.dm_to.contains("://") || config.discord.resolve_token().is_some(),
    );
    report(
        "email",
        config.email.enabled,
        config.email.resolve_password().is_some(),
    );
    Ok(())
}

/// A config-level line: enabled, and whether credentials resolve. No network.
fn report(name: &str, enabled: bool, creds_ok: bool) {
    if !enabled {
        println!("  {name}: off");
    } else if creds_ok {
        println!("  {name}: on, credentials found");
    } else {
        println!("  {name}: on, but NO credentials found (check the token env / config)");
    }
}

/// GitHub gets a live check: identity, the orgs the token can see, and whether
/// team detection works - the things that silently go missing under SSO.
async fn check_github(config: &Config) {
    if !config.github.enabled {
        println!("  github: off");
        return;
    }
    let token = match config.github.resolve_token() {
        Ok(t) => t,
        Err(e) => {
            println!("  github: on, but no token ({e})");
            return;
        }
    };
    let source = match GitHubSource::new(GitHubSourceConfig {
        token,
        api_base: config.github.api_base.clone(),
        track_prs: config.github.track_prs,
        mark_read: false,
        comment_min_age_secs: 0,
        backfill: Default::default(),
    }) {
        Ok(s) => s,
        Err(e) => {
            println!("  github: {e}");
            return;
        }
    };
    match source.doctor().await {
        Ok(d) => {
            println!("  github: authenticated as {}", d.login);
            match d.orgs {
                Some(ref orgs) if orgs.is_empty() => {
                    println!("    visible orgs: none (personal repos only)")
                }
                Some(ref orgs) => println!("    visible orgs: {}", orgs.join(", ")),
                None => println!(
                    "    visible orgs: could not list (token may lack read:org or needs SAML re-authorization)"
                ),
            }
            println!(
                "    team detection (read:org): {}",
                if d.team_detection {
                    "available"
                } else {
                    "unavailable - team review requests won't be detected"
                }
            );
            println!("    if an org you expect is missing, the token isn't authorized for it (e.g. SAML SSO)");
        }
        Err(e) => println!("  github: on, but the check failed: {e}"),
    }
}
