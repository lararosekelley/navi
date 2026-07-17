//! Load `navi.env` (kept next to the config) into the process environment.
//!
//! A background service does not inherit your interactive shell, so tokens have
//! to come from somewhere it can see. navi loads `navi.env` itself at startup so
//! foreground `navi run` and the service read the same file regardless of how
//! they were launched. Loading is additive: an already-set process variable
//! wins, so a value in your shell (or CI) still overrides the file.

use std::path::Path;

use tracing::warn;

/// Env-file name, kept next to the config file.
const ENV_FILE: &str = "navi.env";

/// Load the `navi.env` beside `config_path` into the process environment, filling
/// only variables that are not already set. A missing file is not an error.
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
        if std::env::var_os(&key).is_none() {
            std::env::set_var(&key, &value);
        }
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
    fn load_fills_unset_and_never_overrides() {
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
            "from_env",
            "an already-set variable must win over the file"
        );
    }

    #[test]
    fn load_missing_file_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        load_beside_config(&dir.path().join("config.toml"));
    }
}
