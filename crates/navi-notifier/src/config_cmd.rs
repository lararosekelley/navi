//! `navi config get/set`: read and write config values by dotted key. Writes go
//! through `toml_edit` so the file's comments and formatting survive a `set`.

use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use tempfile::NamedTempFile;
use toml_edit::{DocumentMut, Item, Table, Value};

use crate::config::Config;

/// Print the value at `key` (e.g. `general.poll_interval_secs`).
pub fn get(config_path: &Path, key: &str) -> Result<()> {
    let doc = read_doc(config_path)?;
    println!("{}", get_value(&doc, key)?);
    Ok(())
}

/// Set `key` to `value` in place, preserving the file's comments.
pub fn set(config_path: &Path, key: &str, value: &str) -> Result<()> {
    let mut doc = read_doc(config_path)?;
    set_value(&mut doc, key, value)?;
    write_atomically(config_path, doc.to_string().as_bytes())?;
    println!("set {key} = {value}");
    Ok(())
}

/// Open the config file in the user's editor, then re-parse it so a syntax error
/// introduced by hand is caught now rather than silently at the next load.
pub fn edit(config_path: &Path) -> Result<()> {
    if !config_path.exists() {
        bail!(
            "no config at {}; run `navi init` first",
            config_path.display()
        );
    }
    crate::editor::open(config_path)?;
    if let Err(e) = read_doc(config_path) {
        eprintln!("warning: {} no longer parses: {e:#}", config_path.display());
    }
    Ok(())
}

/// Write `bytes` to `path` without a truncate window: fill a temp file in the
/// same directory, then rename it into place. A crash mid-write leaves the old
/// config intact rather than an empty or half-written one.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut tmp = NamedTempFile::new_in(dir)
        .with_context(|| format!("creating temp file in {}", dir.display()))?;
    tmp.write_all(bytes)
        .with_context(|| format!("writing {}", path.display()))?;
    tmp.persist(path)
        .with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

fn read_doc(config_path: &Path) -> Result<DocumentMut> {
    if !config_path.exists() {
        bail!(
            "no config at {}; run `navi init` first",
            config_path.display()
        );
    }
    let text = fs::read_to_string(config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;
    text.parse::<DocumentMut>()
        .with_context(|| format!("parsing {}", config_path.display()))
}

/// Look up a dotted key and render its value (or a whole section) as a string.
fn get_value(doc: &DocumentMut, key: &str) -> Result<String> {
    let mut parts = key.split('.');
    let first = parts.next().filter(|s| !s.is_empty());
    let mut item = first
        .and_then(|p| doc.as_table().get(p))
        .filter(|i| !i.is_none())
        .ok_or_else(|| anyhow!("no config value at `{key}`"))?;
    for part in parts {
        item = item
            .get(part)
            .filter(|i| !i.is_none())
            .ok_or_else(|| anyhow!("no config value at `{key}`"))?;
    }
    Ok(render_item(item))
}

/// Set a dotted key, creating it (and its section) when absent.
///
/// A typo like `github.enabeld` must still error rather than silently write a key
/// navi ignores, but the file is the wrong thing to check that against: a key can
/// be perfectly valid and simply not written yet, either because it postdates the
/// user's `navi init` or because it is an `Option` that `init` never emits at all
/// (`gitea.api_base`, `email.username`). So the write is validated against navi's
/// own config schema instead, by [`known_key`].
fn set_value(doc: &mut DocumentMut, key: &str, value: &str) -> Result<()> {
    let parts: Vec<&str> = key.split('.').filter(|s| !s.is_empty()).collect();
    let Some((leaf, parents)) = parts.split_last() else {
        bail!("empty config key");
    };

    // Write into a candidate first, so a rejected key leaves the real document
    // untouched - including any section this would otherwise have created.
    let mut candidate = doc.clone();
    let mut table: &mut Table = candidate.as_table_mut();
    for part in parents {
        let entry = table.entry(part).or_insert_with(|| {
            let mut created = Table::new();
            // Implicit tables render their header only once they hold values, so
            // creating `rules.events` doesn't also leave a bare `[rules]` behind.
            created.set_implicit(true);
            Item::Table(created)
        });
        table = entry.as_table_mut().ok_or_else(|| {
            anyhow!("`{part}` is not a config section; check the spelling or fix it in config.toml")
        })?;
    }
    table[leaf] = infer_value(value);

    known_key(&candidate, &parts)?;
    *doc = candidate;
    Ok(())
}

/// Whether `path` is a key navi actually understands, decided by round-tripping the
/// candidate document through [`Config`].
///
/// serde drops unknown keys on the way in, so a typo is gone by the time the parsed
/// config is written back out, while a real key survives. That makes the struct
/// itself the schema, with no hand-maintained key list to drift from it.
///
/// Deserialization also type-checks, so `poll_interval_secs abc` is rejected here
/// rather than written and left to break the next startup.
fn known_key(candidate: &DocumentMut, path: &[&str]) -> Result<()> {
    let key = path.join(".");
    let parsed: Config = toml::from_str(&candidate.to_string())
        .with_context(|| format!("`{key}` cannot be set to that value"))?;
    let round_tripped =
        toml::Value::try_from(&parsed).context("re-serializing the config to validate the key")?;

    let mut item = &round_tripped;
    for part in path {
        match item.get(part) {
            Some(next) => item = next,
            None => bail!(
                "no config key `{key}`; check the spelling (`navi config get {}` lists a section)",
                path.first().copied().unwrap_or_default()
            ),
        }
    }
    Ok(())
}

/// Parse a string the way a user means it: `true`/`false` → bool, digits →
/// integer, anything else → string.
fn infer_value(s: &str) -> Item {
    if let Ok(b) = s.parse::<bool>() {
        toml_edit::value(b)
    } else if let Ok(i) = s.parse::<i64>() {
        toml_edit::value(i)
    } else {
        toml_edit::value(s)
    }
}

/// Render an item for display: bare string values without quotes, other scalars
/// as written, and whole tables as their TOML text.
fn render_item(item: &Item) -> String {
    match item.as_value() {
        Some(Value::String(s)) => s.value().clone(),
        Some(v) => v.to_string().trim().to_string(),
        None => item.to_string().trim().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
# top comment
[general]
# how often to poll
poll_interval_secs = 60

[github]
enabled = true
token_env = \"NAVI_GITHUB_TOKEN\"
";

    fn doc() -> DocumentMut {
        SAMPLE.parse().unwrap()
    }

    #[test]
    fn get_reads_scalars_without_quotes() {
        assert_eq!(
            get_value(&doc(), "general.poll_interval_secs").unwrap(),
            "60"
        );
        assert_eq!(get_value(&doc(), "github.enabled").unwrap(), "true");
        // Strings come back bare, not quoted.
        assert_eq!(
            get_value(&doc(), "github.token_env").unwrap(),
            "NAVI_GITHUB_TOKEN"
        );
    }

    #[test]
    fn get_unknown_key_errors() {
        assert!(get_value(&doc(), "github.nope").is_err());
        assert!(get_value(&doc(), "nosuch.section").is_err());
    }

    #[test]
    fn set_preserves_comments_and_infers_types() {
        let mut d = doc();
        set_value(&mut d, "general.poll_interval_secs", "30").unwrap();
        set_value(&mut d, "github.enabled", "false").unwrap();
        let out = d.to_string();
        // Comments survive.
        assert!(out.contains("# top comment"));
        assert!(out.contains("# how often to poll"));
        // Values changed, and with the right (unquoted) types.
        assert!(out.contains("poll_interval_secs = 30"));
        assert!(out.contains("enabled = false"));
    }

    /// The issue's headline case: a real key that `navi init` never wrote, because
    /// it is an `Option`. Setting it must work, not send the user to a text editor.
    #[test]
    fn set_writes_a_known_key_absent_from_the_file() {
        let mut d = doc();
        set_value(&mut d, "github.api_base", "https://ghe.example.com/api/v3").unwrap();
        let out = d.to_string();
        assert!(out.contains("api_base = \"https://ghe.example.com/api/v3\""));
        // Written into the existing section, and nothing else disturbed.
        assert!(out.contains("# how often to poll"));
        assert!(out.contains("token_env = \"NAVI_GITHUB_TOKEN\""));
    }

    /// Same for a field added to navi after the user ran `navi init`. The sample
    /// stands in for a config written before the field existed.
    #[test]
    fn set_writes_a_known_key_the_config_predates() {
        let mut d = doc();
        assert!(!SAMPLE.contains("comment_min_age_secs"));
        set_value(&mut d, "general.comment_min_age_secs", "30").unwrap();
        assert!(d.to_string().contains("comment_min_age_secs = 30"));
    }

    #[test]
    fn set_creates_a_missing_section_for_a_known_key() {
        let mut d = doc();
        // The sample has no [discord] at all.
        set_value(&mut d, "discord.enabled", "true").unwrap();
        let out = d.to_string();
        assert!(out.contains("[discord]"), "section created: {out}");
        assert!(out.contains("enabled = true"));
        // Still parses as a config, with the value we asked for.
        let parsed: Config = toml::from_str(&out).unwrap();
        assert!(parsed.discord.enabled);
    }

    #[test]
    fn set_of_a_misspelled_key_errors() {
        let mut d = doc();
        // Section exists, leaf is a typo: must fail, not write a junk key.
        assert!(set_value(&mut d, "github.enabeld", "true").is_err());
        assert!(!d.to_string().contains("enabeld"));
    }

    /// A rejected key must not leave the section it would have needed behind.
    #[test]
    fn set_of_an_unknown_section_errors_and_creates_nothing() {
        let mut d = doc();
        let before = d.to_string();
        assert!(set_value(&mut d, "nosuch.enabled", "true").is_err());
        assert_eq!(d.to_string(), before);
    }

    /// Deserializing to validate the key type-checks it too, so a value that would
    /// break the next startup is refused now instead of written.
    #[test]
    fn set_of_a_wrongly_typed_value_errors() {
        let mut d = doc();
        assert!(set_value(&mut d, "general.poll_interval_secs", "abc").is_err());
        assert!(d.to_string().contains("poll_interval_secs = 60"));
        // Enum fields are checked the same way.
        assert!(set_value(&mut d, "general.backfill", "sideways").is_err());
        assert!(set_value(&mut d, "general.backfill", "none").is_ok());
    }

    /// Collect every dotted path to a scalar in navi's default config. Arrays of
    /// tables (`routes`, `rules.overrides`) aren't addressable by dotted key, so
    /// they're skipped.
    fn scalar_paths(v: &toml::Value, prefix: &str, out: &mut Vec<(String, String)>) {
        match v {
            toml::Value::Table(t) => {
                for (k, child) in t {
                    let path = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    };
                    scalar_paths(child, &path, out);
                }
            }
            toml::Value::String(s) if !prefix.is_empty() => {
                out.push((prefix.to_string(), s.clone()))
            }
            toml::Value::Boolean(_) | toml::Value::Integer(_) if !prefix.is_empty() => {
                out.push((prefix.to_string(), v.to_string()))
            }
            _ => {}
        }
    }

    /// Every key navi understands must be settable on a config that has none of
    /// them. Derived from `Config::default()`, so it can't drift as fields are
    /// added - a new field that `set` can't reach fails here.
    #[test]
    fn every_default_key_is_settable_from_an_empty_config() {
        let mut paths = Vec::new();
        scalar_paths(
            &toml::Value::try_from(Config::default()).unwrap(),
            "",
            &mut paths,
        );
        assert!(
            paths.len() > 40,
            "expected the whole config surface, got {}",
            paths.len()
        );

        for (key, value) in &paths {
            let mut d: DocumentMut = "".parse().unwrap();
            set_value(&mut d, key, value)
                .unwrap_or_else(|e| panic!("`{key}` should be settable: {e:#}"));
            // And the result is still a config navi can read back.
            let out = d.to_string();
            toml::from_str::<Config>(&out)
                .unwrap_or_else(|e| panic!("`{key}` produced unparseable config: {e:#}\n{out}"));
        }

        // Spot-check that nesting deeper than one section is covered.
        assert!(paths.iter().any(|(k, _)| k == "rules.events.mentioned"));
        assert!(paths.iter().any(|(k, _)| k == "rules.quiet_hours.start"));
    }

    #[test]
    fn creating_a_nested_section_leaves_no_bare_parent_header() {
        let mut d: DocumentMut = "".parse().unwrap();
        set_value(&mut d, "rules.events.mentioned", "false").unwrap();
        let out = d.to_string();
        assert!(out.contains("[rules.events]"), "{out}");
        assert!(!out.contains("[rules]\n"), "bare parent header: {out}");
    }
}
