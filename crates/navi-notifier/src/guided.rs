//! Guided opt-in: after `navi init`, walk the user through enabling the providers
//! they want (everything ships off). Enabling a provider upserts its token into
//! `navi.env` and flips `<id>.enabled = true` in the config.

use std::io::IsTerminal;
use std::path::Path;

use anyhow::Result;

use crate::{config_cmd, envfile, prompt, providers};

/// `(id, the secret env var to offer to fill, or None when setup isn't a single
/// token — the setup text guides those)`.
const PROVIDERS: &[(&str, Option<&str>)] = &[
    ("github", Some("NAVI_GITHUB_TOKEN")),
    ("gitlab", Some("NAVI_GITLAB_TOKEN")),
    ("gitea", Some("NAVI_GITEA_TOKEN")),
    ("slack", Some("NAVI_SLACK_TOKEN")),
    ("discord", None),
    ("email", None),
];

/// Offer to enable each provider. A no-op when not attached to a terminal (so the
/// service/CI path is untouched).
pub fn opt_in(config_path: &Path) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        return Ok(());
    }
    println!("\nEverything starts off. Let's turn on what you want (y to enable, anything else to skip).");
    for (id, secret_env) in PROVIDERS {
        if !prompt::confirm(&format!("Enable {id}? [y/N] "))? {
            continue;
        }
        if let Some(text) = providers::setup_text(id) {
            println!("\n{text}\n");
        }
        // For bot-mode Discord, offer to build the invite link from the app's id.
        if *id == "discord" {
            let cid =
                prompt::input("Discord app Client ID for a ready invite link (blank to skip): ")?;
            let cid = cid.trim();
            if !cid.is_empty() {
                if cid.chars().all(|c| c.is_ascii_digit()) {
                    println!("  invite: {}", providers::discord_invite_url(cid));
                } else {
                    println!("  (skipped: Client ID should be a numeric snowflake)");
                }
            }
        }
        if let Some(env) = secret_env {
            let value = prompt::input(&format!("Paste {env} now (or leave blank to set later): "))?;
            if !value.is_empty() {
                envfile::upsert(config_path, env, &value)?;
                println!("  saved {env} to navi.env");
            }
        }
        config_cmd::set(config_path, &format!("{id}.enabled"), "true")?;
    }
    println!("\nDone. Check it with `navi doctor`, then `navi run` to start watching.");
    Ok(())
}
