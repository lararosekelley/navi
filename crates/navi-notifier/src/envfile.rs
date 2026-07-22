//! Load `navi.env` (kept next to the config) into the process environment.
//!
//! A background service does not inherit your interactive shell, so tokens have
//! to come from somewhere it can see. navi loads `navi.env` itself at startup so
//! foreground `navi run` and the service read the same file regardless of how
//! they were launched. `navi.env` is **authoritative**: a value in the file
//! overrides any process/shell variable of the same name, so the file is the
//! single source of truth (migration note: shell env no longer wins over it).

use std::io::Write;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use tempfile::NamedTempFile;
use tracing::warn;

/// Env-file name, kept next to the config file.
const ENV_FILE: &str = "navi.env";

/// Set `key=value` in the `navi.env` beside `config_path`: update an existing
/// entry (preserving comments and every other line) or append a new one. Creates
/// the file (chmod 600 on unix) if missing. Idempotent and re-runnable.
pub fn upsert(config_path: &Path, key: &str, value: &str) -> Result<()> {
    let dir = config_path
        .parent()
        .ok_or_else(|| anyhow!("config path has no parent directory"))?;
    std::fs::create_dir_all(dir).ok();
    let path = dir.join(ENV_FILE);
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let contents = upsert_line(&existing, key, value);

    // Write through an owner-only temp file, then rename into place. The token is
    // never briefly world-readable the way a fresh `fs::write` under a 0644 umask
    // would leave it, and a crash mid-write can't truncate the existing file.
    let mut tmp = NamedTempFile::new_in(dir)
        .with_context(|| format!("creating temp file in {}", dir.display()))?;
    set_owner_only(tmp.path());
    tmp.write_all(contents.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    tmp.persist(&path)
        .with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// Open the `navi.env` beside `config_path` in the user's editor, creating it
/// (chmod 600 on unix) first if it doesn't exist so first-time secret entry works.
pub fn edit(config_path: &Path) -> Result<()> {
    let dir = config_path
        .parent()
        .ok_or_else(|| anyhow!("config path has no parent directory"))?;
    std::fs::create_dir_all(dir).ok();
    let path = dir.join(ENV_FILE);
    if !path.exists() {
        // Same owner-only-before-write dance as `upsert`: chmod the temp file
        // before it holds anything, so the file this user will soon paste tokens
        // into is never briefly world-readable under a 0644 umask.
        let seed = "# navi.env: KEY=value per line. Values here override shell variables.\n";
        let mut tmp = NamedTempFile::new_in(dir)
            .with_context(|| format!("creating temp file in {}", dir.display()))?;
        set_owner_only(tmp.path());
        tmp.write_all(seed.as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
        tmp.persist(&path)
            .with_context(|| format!("creating {}", path.display()))?;
    }
    crate::editor::open(&path)
}

/// Pure core of [`upsert`]: return the file text with `key=value` updated in
/// place, or appended if absent.
fn upsert_line(existing: &str, key: &str, value: &str) -> String {
    let new_line = format!("{key}={value}");
    let mut replaced = false;
    let mut lines: Vec<String> = existing
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            if !trimmed.starts_with('#') {
                if let Some((k, _)) = trimmed.split_once('=') {
                    if k.trim() == key {
                        replaced = true;
                        return new_line.clone();
                    }
                }
            }
            line.to_string()
        })
        .collect();
    if !replaced {
        lines.push(new_line);
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

#[cfg(unix)]
fn set_owner_only(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_owner_only(_path: &Path) {}

/// Load the `navi.env` beside `config_path` into the process environment,
/// overriding any variables already set (the file is authoritative). A missing
/// file is not an error.
pub fn load_beside_config(config_path: &Path) {
    let Some(dir) = config_path.parent() else {
        return;
    };
    let path = dir.join(ENV_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "could not read navi.env");
            return;
        }
    };
    warn_if_group_or_world_readable(&path);
    for (key, value) in parse(&text) {
        // Authoritative: the file wins over any existing process/shell variable.
        std::env::set_var(&key, &value);
    }
}

/// Parse `KEY=value` lines. Ignores blank lines and `#` comments; trims
/// whitespace and strips one layer of surrounding quotes on the value.
fn parse(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        out.push((key.to_string(), unquote(value.trim()).to_string()));
    }
    out
}

/// Strip one matching pair of surrounding single or double quotes.
fn unquote(s: &str) -> &str {
    let b = s.as_bytes();
    if b.len() >= 2 && (b[0] == b'"' || b[0] == b'\'') && b[b.len() - 1] == b[0] {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

#[cfg(unix)]
fn warn_if_group_or_world_readable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.permissions().mode() & 0o077 != 0 {
            warn!(path = %path.display(), "navi.env is group/world-accessible; run `chmod 600` on it");
        }
    }
}

#[cfg(not(unix))]
fn warn_if_group_or_world_readable(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_skips_comments_blanks_and_bad_lines() {
        let text = "\
# a comment\n\
\n\
NAVI_GITHUB_TOKEN=ghp_abc\n\
  NAVI_SLACK_TOKEN = xoxb-1  \n\
no_equals_here\n\
=missing_key\n";
        assert_eq!(
            parse(text),
            vec![
                ("NAVI_GITHUB_TOKEN".to_string(), "ghp_abc".to_string()),
                ("NAVI_SLACK_TOKEN".to_string(), "xoxb-1".to_string()),
            ]
        );
    }

    #[test]
    fn parse_strips_matching_quotes_only() {
        assert_eq!(unquote("\"quoted\""), "quoted");
        assert_eq!(unquote("'quoted'"), "quoted");
        assert_eq!(unquote("\"mismatched'"), "\"mismatched'");
        assert_eq!(unquote("bare"), "bare");
        assert_eq!(unquote("\""), "\"");
    }

    /// Removes the named env vars on drop so a failing assert can't leak them
    /// into other (parallel) tests.
    struct EnvGuard(&'static [&'static str]);
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for key in self.0 {
                std::env::remove_var(key);
            }
        }
    }

    #[test]
    fn load_is_authoritative_over_process_env() {
        let _guard = EnvGuard(&["NAVI_ENVFILE_TEST_FRESH", "NAVI_ENVFILE_TEST_PRESET"]);
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        std::fs::write(
            dir.path().join("navi.env"),
            "NAVI_ENVFILE_TEST_FRESH=from_file\nNAVI_ENVFILE_TEST_PRESET=from_file\n",
        )
        .unwrap();

        std::env::set_var("NAVI_ENVFILE_TEST_PRESET", "from_env");
        load_beside_config(&cfg);

        assert_eq!(
            std::env::var("NAVI_ENVFILE_TEST_FRESH").unwrap(),
            "from_file"
        );
        assert_eq!(
            std::env::var("NAVI_ENVFILE_TEST_PRESET").unwrap(),
            "from_file",
            "navi.env is authoritative: the file must win over a shell variable"
        );
    }

    #[test]
    fn upsert_line_updates_or_appends_preserving_others() {
        let existing = "# creds\nNAVI_GITHUB_TOKEN=old\nNAVI_SLACK_TOKEN=xoxb\n";
        // Existing key is updated; comments and other entries survive.
        let out = upsert_line(existing, "NAVI_GITHUB_TOKEN", "new");
        assert!(out.contains("# creds"));
        assert!(out.contains("NAVI_GITHUB_TOKEN=new"));
        assert!(!out.contains("NAVI_GITHUB_TOKEN=old"));
        assert!(out.contains("NAVI_SLACK_TOKEN=xoxb"));
        // A new key is appended, leaving the rest intact.
        let out2 = upsert_line(existing, "NAVI_DISCORD_TOKEN", "bot");
        assert!(out2.contains("NAVI_GITHUB_TOKEN=old"));
        assert!(out2.contains("NAVI_DISCORD_TOKEN=bot"));
        // Empty input yields just the one line.
        assert_eq!(upsert_line("", "K", "v"), "K=v\n");
    }

    #[test]
    fn load_missing_file_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        load_beside_config(&dir.path().join("config.toml"));
    }

    #[cfg(unix)]
    #[test]
    fn upsert_creates_the_file_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        upsert(&cfg, "NAVI_GITHUB_TOKEN", "ghp_secret").unwrap();
        let env_path = dir.path().join(ENV_FILE);
        let mode = std::fs::metadata(&env_path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "the token file must never be readable by others"
        );
        assert!(std::fs::read_to_string(&env_path)
            .unwrap()
            .contains("NAVI_GITHUB_TOKEN=ghp_secret"));
    }
}
