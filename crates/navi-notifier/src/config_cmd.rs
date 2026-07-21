//! `navi config get/set`: read and write config values by dotted key. Writes go
//! through `toml_edit` so the file's comments and formatting survive a `set`.

use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use tempfile::NamedTempFile;
use toml_edit::{DocumentMut, Item, Table, Value};

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

/// Set a dotted key. Both the parent section and the leaf key must already
/// exist, so a typo (`github.enabeld`) errors instead of silently writing a
/// garbage key navi then ignores. To add a genuinely new key, edit config.toml.
fn set_value(doc: &mut DocumentMut, key: &str, value: &str) -> Result<()> {
    let parts: Vec<&str> = key.split('.').filter(|s| !s.is_empty()).collect();
    let Some((leaf, parents)) = parts.split_last() else {
        bail!("empty config key");
    };
    let mut table: &mut Table = doc.as_table_mut();
    for part in parents {
        table = table
            .get_mut(part)
            .and_then(Item::as_table_mut)
            .ok_or_else(|| anyhow!("no config section `{part}`; add it to config.toml first"))?;
    }
    if !table.contains_key(leaf) {
        bail!("no config key `{key}`; check the spelling or add it to config.toml first");
    }
    table[leaf] = infer_value(value);
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

    #[test]
    fn set_into_a_missing_section_errors() {
        let mut d = doc();
        assert!(set_value(&mut d, "discord.enabled", "true").is_err());
    }

    #[test]
    fn set_of_a_misspelled_key_errors() {
        let mut d = doc();
        // Section exists, leaf is a typo: must fail, not write a junk key.
        assert!(set_value(&mut d, "github.enabeld", "true").is_err());
        assert!(!d.to_string().contains("enabeld"));
    }
}
