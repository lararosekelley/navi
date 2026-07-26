//! `navi doctor`: report what each enabled provider can see, so silent
//! misconfiguration (e.g. a GitHub token that can't see an org because of SAML
//! SSO, or a destination with no credentials) is visible instead of looking like
//! navi being broken.

use anyhow::{bail, Result};
use navi_notifier_github::{GitHubSource, GitHubSourceConfig};

use crate::config::{self, Config, Severity};

/// `navi doctor`: static config validation followed by live provider probes. With
/// `offline`, only the static pass runs (no network). Exits non-zero if the static
/// pass found any errors.
pub async fn doctor(config: &Config, offline: bool) -> Result<()> {
    let findings = config::validate(config);
    print_findings(&findings);
    let errors = findings
        .iter()
        .filter(|f| f.severity == Severity::Error)
        .count();

    println!("\nsources:");
    if offline {
        report(
            "github",
            config::source_enabled(config, "github"),
            config::source_creds(config, "github"),
        );
    } else {
        check_github(config).await;
    }
    report(
        "gitlab",
        config::source_enabled(config, "gitlab"),
        config::source_creds(config, "gitlab"),
    );
    report(
        "gitea",
        config::source_enabled(config, "gitea"),
        config::source_creds(config, "gitea"),
    );

    println!("\ndestinations:");
    for id in config::DESTINATION_IDS {
        // Webhook-mode Discord is self-authenticating; the shared `dest_creds` knows.
        report(
            id,
            config::dest_enabled(config, id),
            config::dest_creds(config, id),
        );
    }

    if errors > 0 {
        bail!("config has {errors} error(s) above; fix them before running navi");
    }
    Ok(())
}

/// Print the static-validation findings under a `config:` header (or a clean line).
fn print_findings(findings: &[crate::config::Finding]) {
    println!("config:");
    if findings.is_empty() {
        println!("  no problems found");
        return;
    }
    for f in findings {
        let label = match f.severity {
            Severity::Error => "error",
            Severity::Warning => "warn",
        };
        println!("  {label}: {}", f.message);
    }
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
