//! `navi snooze`: set, list, and end snoozes. Snoozes live in the state database,
//! where the daemon picks them up on its next poll.

use std::path::Path;
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use navi_notifier_core::{Snooze, SnoozeDuration, SnoozeScope};
use time::macros::format_description;
use time::{OffsetDateTime, UtcOffset};

use crate::cli::SnoozeTarget;
use crate::config::{resolve_state_path, source_enabled, Config, SOURCE_IDS};
use crate::state::SqliteStore;

pub async fn set(config_path: &Path, duration: &str, target: &SnoozeTarget) -> Result<()> {
    let config = load_config(config_path)?;
    let duration: SnoozeDuration = duration.parse()?;
    let scope = resolve_scope(&config, target)?;
    let now = OffsetDateTime::now_utc();
    let until = duration
        .end_from(now)
        .ok_or_else(|| anyhow!("{duration} is too long"))?;
    let snooze = Snooze {
        scope,
        duration,
        until,
    };
    let replaced = open_store()?.put_snooze(&snooze, now).await?;
    println!(
        "Snoozed {} for {duration}, until {}.{}",
        snooze.scope,
        local_time(until, &config),
        if replaced {
            " This replaces the earlier snooze."
        } else {
            ""
        }
    );
    Ok(())
}

pub async fn off(config_path: &Path, target: &SnoozeTarget) -> Result<()> {
    let config = load_config(config_path)?;
    let scope = resolve_scope(&config, target)?;
    let store = open_store()?;
    let now = OffsetDateTime::now_utc();
    if !store.remove_snooze(&scope, now).await? {
        println!("No active snooze on {scope}.");
        return Ok(());
    }
    let overlapping: Vec<Snooze> = store
        .active_snoozes(now)
        .await?
        .into_iter()
        .filter(|s| overlaps(&s.scope, &scope))
        .collect();
    if overlapping.is_empty() {
        println!("Ended the snooze on {scope}. Held notifications go out on the next poll.");
    } else {
        println!("Ended the snooze on {scope}. These still hold some or all of its notifications:");
        for line in format_snoozes(&overlapping, &config, now) {
            println!("  {line}");
        }
    }
    Ok(())
}

/// Whether one snooze can hold events the other covers.
fn overlaps(a: &SnoozeScope, b: &SnoozeScope) -> bool {
    covers(a, b) || covers(b, a)
}

/// Whether every event `inner` covers is also covered by `outer`.
fn covers(outer: &SnoozeScope, inner: &SnoozeScope) -> bool {
    let within = |path: &str, org: &str| {
        path.eq_ignore_ascii_case(org)
            || path
                .to_lowercase()
                .starts_with(&format!("{}/", org.to_lowercase()))
    };
    match (outer, inner) {
        (SnoozeScope::Global, _) => true,
        (_, SnoozeScope::Global) => false,
        (
            SnoozeScope::Org { provider, org },
            SnoozeScope::Org {
                provider: p,
                org: path,
            }
            | SnoozeScope::Repo {
                provider: p,
                repo: path,
            },
        ) => provider == p && within(path, org),
        (SnoozeScope::Repo { .. }, SnoozeScope::Repo { .. }) => outer.key() == inner.key(),
        (SnoozeScope::Repo { .. }, SnoozeScope::Org { .. }) => false,
    }
}

/// Print active snoozes. Works without a config, falling back to UTC for times.
pub async fn status(config_path: &Path) -> Result<()> {
    let lines = status_lines(config_path).await?;
    if lines.is_empty() {
        println!("No active snoozes.");
    }
    for line in lines {
        println!("{line}");
    }
    Ok(())
}

/// The snooze section of `navi service status`. Reports a problem rather than
/// failing, since a broken config or state is when the command gets run.
pub async fn service_status(config_path: &Path) -> Result<()> {
    match status_lines(config_path).await {
        Ok(lines) if lines.is_empty() => println!("snoozes: none"),
        Ok(lines) => {
            println!("snoozes:");
            for line in lines {
                println!("  {line}");
            }
        }
        Err(err) => println!("snoozes: unavailable ({err:#})"),
    }
    Ok(())
}

/// One line per active snooze. Reading never creates the state database, and a
/// config that is missing or fails to load only costs the local time offset.
async fn status_lines(config_path: &Path) -> Result<Vec<String>> {
    let config = Config::load(config_path).unwrap_or_default();
    let path = resolve_state_path()?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let store = SqliteStore::open(&path).context("opening state store")?;
    let now = OffsetDateTime::now_utc();
    Ok(format_snoozes(
        &store.active_snoozes(now).await?,
        &config,
        now,
    ))
}

fn format_snoozes(snoozes: &[Snooze], config: &Config, now: OffsetDateTime) -> Vec<String> {
    let width = snoozes
        .iter()
        .map(|s| s.scope.to_string().len())
        .max()
        .unwrap_or(0);
    snoozes
        .iter()
        .map(|s| {
            format!(
                "{:<width$}  {:>4}  until {} ({} left)",
                s.scope.to_string(),
                s.duration.to_string(),
                local_time(s.until, config),
                remaining(s.until - now),
            )
        })
        .collect()
}

fn load_config(config_path: &Path) -> Result<Config> {
    if !config_path.exists() {
        bail!(
            "no config at {}; run `navi init` first",
            config_path.display()
        );
    }
    Config::load(config_path)
}

fn open_store() -> Result<SqliteStore> {
    SqliteStore::open(&resolve_state_path()?).context("opening state store")
}

fn resolve_scope(config: &Config, target: &SnoozeTarget) -> Result<SnoozeScope> {
    if target.global {
        return Ok(SnoozeScope::Global);
    }
    let provider = target.provider.as_deref();
    if let Some(org) = &target.org {
        let org = org.trim().trim_matches('/');
        if org.is_empty() || org.split('/').any(str::is_empty) {
            bail!("invalid org `{org}`");
        }
        return Ok(SnoozeScope::Org {
            provider: resolve_provider(config, provider, None, &format!("org {org}"))?,
            org: org.to_string(),
        });
    }
    let (host, repo) = match &target.repo {
        Some(repo) => (
            None,
            normalize_repo(repo).ok_or_else(|| anyhow!("invalid repo `{repo}`; use owner/name"))?,
        ),
        None => {
            let remote = git_remote()?;
            let (host, repo) = parse_remote(&remote).ok_or_else(|| {
                anyhow!("can't read a repo from git remote `{remote}`; pass --repo")
            })?;
            (Some(host), repo)
        }
    };
    Ok(SnoozeScope::Repo {
        provider: resolve_provider(config, provider, host.as_deref(), &format!("repo {repo}"))?,
        repo,
    })
}

/// The provider a repo or org is on: `--provider` if given, else the enabled
/// source whose host matches the git remote, else the only enabled source.
fn resolve_provider(
    config: &Config,
    flag: Option<&str>,
    host: Option<&str>,
    what: &str,
) -> Result<String> {
    if let Some(p) = flag {
        if !SOURCE_IDS.contains(&p) {
            bail!("unknown provider `{p}`; use github, gitlab, or gitea");
        }
        if !source_enabled(config, p) {
            bail!("the {p} source is not enabled");
        }
        return Ok(p.to_string());
    }
    let enabled = SOURCE_IDS
        .into_iter()
        .filter(|id| source_enabled(config, id));
    let candidates: Vec<&str> = match host {
        Some(host) => enabled
            .filter(|id| provider_host(config, id).is_some_and(|h| h.eq_ignore_ascii_case(host)))
            .collect(),
        None => enabled.collect(),
    };
    match candidates.as_slice() {
        [one] => Ok(one.to_string()),
        [] => match host {
            Some(host) => bail!("{host} doesn't match any enabled source; pass --provider"),
            None => bail!("no source is enabled"),
        },
        many => bail!("{what} could be on {}; pass --provider", many.join(" or ")),
    }
}

/// The web host a source's repos live on, for matching a git remote.
fn provider_host(config: &Config, id: &str) -> Option<String> {
    let (api_base, default) = match id {
        "github" => (config.github.api_base.as_deref(), Some("github.com")),
        "gitlab" => (config.gitlab.api_base.as_deref(), Some("gitlab.com")),
        "gitea" => (config.gitea.api_base.as_deref(), None),
        _ => return None,
    };
    match api_base.and_then(url_host) {
        Some(host) if host.eq_ignore_ascii_case("api.github.com") => Some("github.com".into()),
        Some(host) => Some(host),
        None => default.map(str::to_string),
    }
}

/// The host of an `scheme://[user@]host[:port]/...` URL.
fn url_host(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split('/').next()?;
    let host = authority.rsplit('@').next()?;
    let host = host.split(':').next()?;
    (!host.is_empty()).then(|| host.to_string())
}

/// The origin remote's URL, or the only remote's if there is no origin.
fn git_remote() -> Result<String> {
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git").args(args).output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    if let Some(url) = git(&["remote", "get-url", "origin"]) {
        return Ok(url);
    }
    let remotes = git(&["remote"])
        .ok_or_else(|| anyhow!("not in a git repository; pass --repo, --org, or --global"))?;
    let remotes: Vec<&str> = remotes.lines().collect();
    match remotes.as_slice() {
        [one] => git(&["remote", "get-url", one])
            .ok_or_else(|| anyhow!("could not read the URL of git remote `{one}`")),
        [] => bail!("this repository has no git remote; pass --repo"),
        _ => bail!("this repository has several remotes and none is `origin`; pass --repo"),
    }
}

/// Split a git remote URL into its host and `owner/name` path. Handles
/// `https://host/owner/name(.git)`, `ssh://git@host[:port]/owner/name.git` and
/// `git@host:owner/name.git`. The owner may span several segments (GitLab
/// subgroups).
fn parse_remote(url: &str) -> Option<(String, String)> {
    let (host, path) = if url.contains("://") {
        let host = url_host(url)?;
        let path = url.split_once("://")?.1.split_once('/')?.1;
        (host, path)
    } else {
        let (authority, path) = url.split_once(':')?;
        let host = authority.rsplit('@').next()?;
        (host.to_string(), path)
    };
    if host.is_empty() {
        return None;
    }
    Some((host, normalize_repo(path)?))
}

fn normalize_repo(repo: &str) -> Option<String> {
    let repo = repo.trim().trim_matches('/');
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    let segments: Vec<&str> = repo.split('/').collect();
    (segments.len() >= 2 && segments.iter().all(|s| !s.is_empty())).then(|| repo.to_string())
}

fn local_time(t: OffsetDateTime, config: &Config) -> String {
    let offset = UtcOffset::from_whole_seconds(config.general.utc_offset_minutes * 60)
        .unwrap_or(UtcOffset::UTC);
    t.to_offset(offset)
        .format(format_description!("[year]-[month]-[day] [hour]:[minute]"))
        .unwrap_or_else(|_| t.to_string())
}

/// A short "time left", in the two largest units: `3d 4h`, `59m 10s`, `40s`.
fn remaining(d: time::Duration) -> String {
    let secs = d.whole_seconds().max(0);
    let parts = [
        (secs / 86_400, "d"),
        (secs % 86_400 / 3_600, "h"),
        (secs % 3_600 / 60, "m"),
        (secs % 60, "s"),
    ];
    let first = parts.iter().position(|(n, _)| *n > 0).unwrap_or(3);
    parts[first..]
        .iter()
        .take(2)
        .filter(|(n, _)| *n > 0 || first == 3)
        .map(|(n, u)| format!("{n}{u}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_remote_urls() {
        let cases = [
            (
                "https://github.com/acme/widgets.git",
                "github.com",
                "acme/widgets",
            ),
            (
                "https://github.com/acme/widgets",
                "github.com",
                "acme/widgets",
            ),
            (
                "git@github.com:acme/widgets.git",
                "github.com",
                "acme/widgets",
            ),
            (
                "ssh://git@gitea.example.com:2222/acme/widgets.git",
                "gitea.example.com",
                "acme/widgets",
            ),
            (
                "https://user@gitlab.com/group/sub/proj.git",
                "gitlab.com",
                "group/sub/proj",
            ),
        ];
        for (url, host, repo) in cases {
            assert_eq!(
                parse_remote(url),
                Some((host.to_string(), repo.to_string())),
                "{url}"
            );
        }
        assert_eq!(parse_remote("https://github.com/acme"), None);
        assert_eq!(parse_remote("/local/path"), None);
    }

    fn config_with(sources: &[&str]) -> Config {
        let mut c = Config::default();
        c.github.enabled = sources.contains(&"github");
        c.gitlab.enabled = sources.contains(&"gitlab");
        c.gitea.enabled = sources.contains(&"gitea");
        c.gitea.api_base = Some("https://gitea.example.com/api/v1".into());
        c
    }

    #[test]
    fn provider_comes_from_the_flag_the_host_or_the_only_source() {
        let both = config_with(&["github", "gitea"]);
        assert_eq!(
            resolve_provider(&both, Some("gitea"), None, "repo a/b").unwrap(),
            "gitea"
        );
        assert_eq!(
            resolve_provider(&both, None, Some("github.com"), "repo a/b").unwrap(),
            "github"
        );
        assert_eq!(
            resolve_provider(&both, None, Some("gitea.example.com"), "repo a/b").unwrap(),
            "gitea"
        );
        let err = resolve_provider(&both, None, None, "repo a/b").unwrap_err();
        assert!(
            err.to_string().contains("could be on github or gitea"),
            "{err}"
        );
        assert!(resolve_provider(&both, None, Some("example.org"), "repo a/b").is_err());
        assert!(resolve_provider(&both, Some("gitlab"), None, "repo a/b").is_err());
        assert!(resolve_provider(&both, Some("bitbucket"), None, "repo a/b").is_err());

        let one = config_with(&["github"]);
        assert_eq!(
            resolve_provider(&one, None, None, "repo a/b").unwrap(),
            "github"
        );
    }

    #[test]
    fn github_enterprise_and_public_api_hosts() {
        let mut c = config_with(&["github"]);
        assert_eq!(provider_host(&c, "github").as_deref(), Some("github.com"));
        c.github.api_base = Some("https://api.github.com".into());
        assert_eq!(provider_host(&c, "github").as_deref(), Some("github.com"));
        c.github.api_base = Some("https://ghe.example.com/api/v3".into());
        assert_eq!(
            provider_host(&c, "github").as_deref(),
            Some("ghe.example.com")
        );
    }

    #[test]
    fn explicit_scopes() {
        let c = config_with(&["github"]);
        let target = |repo: Option<&str>, org: Option<&str>, global| SnoozeTarget {
            repo: repo.map(str::to_string),
            org: org.map(str::to_string),
            global,
            provider: None,
        };
        assert_eq!(
            resolve_scope(&c, &target(None, None, true)).unwrap(),
            SnoozeScope::Global
        );
        assert_eq!(
            resolve_scope(&c, &target(Some("acme/widgets.git"), None, false)).unwrap(),
            SnoozeScope::Repo {
                provider: "github".into(),
                repo: "acme/widgets".into()
            }
        );
        assert_eq!(
            resolve_scope(&c, &target(None, Some("acme"), false)).unwrap(),
            SnoozeScope::Org {
                provider: "github".into(),
                org: "acme".into()
            }
        );
        assert!(resolve_scope(&c, &target(Some("widgets"), None, false)).is_err());
    }

    #[test]
    fn overlapping_scopes() {
        let org = |p: &str, o: &str| SnoozeScope::Org {
            provider: p.into(),
            org: o.into(),
        };
        let repo = |p: &str, r: &str| SnoozeScope::Repo {
            provider: p.into(),
            repo: r.into(),
        };
        let widgets = repo("github", "acme/widgets");
        assert!(overlaps(&SnoozeScope::Global, &widgets));
        assert!(overlaps(&widgets, &SnoozeScope::Global));
        assert!(overlaps(&org("github", "Acme"), &widgets));
        assert!(overlaps(
            &org("gitlab", "acme"),
            &org("gitlab", "acme/infra")
        ));
        assert!(overlaps(&widgets, &repo("github", "ACME/widgets")));
        assert!(!overlaps(&org("gitea", "acme"), &widgets));
        assert!(!overlaps(&org("github", "acmecorp"), &widgets));
        assert!(!overlaps(&repo("github", "acme/gadgets"), &widgets));
    }

    #[test]
    fn remaining_shows_two_units() {
        let d = time::Duration::seconds;
        assert_eq!(remaining(d(3 * 86_400 + 4 * 3_600 + 5)), "3d 4h");
        assert_eq!(remaining(d(3 * 3_600 + 59 * 60)), "3h 59m");
        assert_eq!(remaining(d(2 * 3_600)), "2h");
        assert_eq!(remaining(d(40)), "40s");
        assert_eq!(remaining(d(0)), "0s");
    }
}
