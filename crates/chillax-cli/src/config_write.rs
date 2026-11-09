//! Edits to `.chillax/chillax.toml` that keep the stakeholder's comments and layout.

use std::path::Path;

use anyhow::{Context, Result, bail};
use toml_edit::{DocumentMut, Item, Table, value};

/// Set `[budget] max_concurrent_agents` to `n`. Writes a temporary file beside the config
/// and renames it over, so a crash never leaves a half-written file.
pub fn set_budget_max(path: &Path, n: usize) -> Result<()> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut doc: DocumentMut = text
        .parse()
        .with_context(|| format!("parsing {}", path.display()))?;
    let budget = doc
        .entry("budget")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut();
    let Some(budget) = budget else {
        bail!("[budget] in {} is not a table", path.display());
    };
    budget["max_concurrent_agents"] = value(i64::try_from(n).context("limit out of range")?);

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("creating a temp file in {}", dir.display()))?;
    std::fs::write(tmp.path(), doc.to_string())?;
    tmp.persist(path)
        .with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_the_value_and_keeps_comments_and_other_tables() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("chillax.toml");
        std::fs::write(
            &path,
            "# my project\n[project]\nname = \"x\" # keep\n\n[budget]\n# agents at once\nmax_concurrent_agents = 3\n\n[team]\nx = 1\n",
        )
        .expect("written");
        set_budget_max(&path, 5).expect("set");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("# my project"));
        assert!(text.contains("name = \"x\" # keep"));
        assert!(text.contains("# agents at once"));
        assert!(text.contains("max_concurrent_agents = 5"));
        assert!(text.contains("[team]"));
        let parsed: toml::Table = text.parse().expect("still valid toml");
        assert_eq!(
            parsed["budget"]["max_concurrent_agents"].as_integer(),
            Some(5)
        );
    }

    #[test]
    fn adds_the_table_and_key_when_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("chillax.toml");
        std::fs::write(&path, "[project]\nname = \"x\"\n").expect("written");
        set_budget_max(&path, 2).expect("set");
        let parsed: toml::Table = std::fs::read_to_string(&path)
            .expect("read")
            .parse()
            .expect("valid");
        assert_eq!(
            parsed["budget"]["max_concurrent_agents"].as_integer(),
            Some(2)
        );
    }

    #[test]
    fn a_missing_file_is_an_error_and_creates_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("chillax.toml");
        assert!(set_budget_max(&path, 2).is_err());
        assert!(!path.exists());
    }
}
