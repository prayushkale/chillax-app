//! The project's gitignored env files (`.env`, `.env.qa`, …) as variables for the
//! processes Chillax starts. Worktrees only get tracked files, so without this
//! an agent or a QA command never sees what the stakeholder put in `.env`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chillax_core::config::{APP_ENVS, EnvConfig};

/// Where each stage's env files are. Files are read on every call, so an edit applies to
/// the next session or command without a restart.
#[derive(Debug, Clone, Default)]
pub struct EnvFiles {
    root: PathBuf,
    /// Stage → its files, later ones winning. A stage not listed gets nothing.
    stages: BTreeMap<String, Vec<String>>,
}

impl EnvFiles {
    /// `root` is the repository root the file names are relative to.
    pub fn new(root: impl Into<PathBuf>, env: &EnvConfig) -> Self {
        Self {
            root: root.into(),
            stages: APP_ENVS
                .iter()
                .map(|stage| (stage.to_string(), env.files_for(stage)))
                .collect(),
        }
    }

    /// The variables for `stage` (`dev`, `qa` or `prod`). A missing file is skipped; an
    /// unreadable line is logged by file and line number only, never its value.
    pub async fn vars(&self, stage: &str) -> BTreeMap<String, String> {
        let mut vars = BTreeMap::new();
        for file in self.stages.get(stage).into_iter().flatten() {
            let Ok(text) = tokio::fs::read(self.root.join(file)).await else {
                continue;
            };
            let mut loaded = 0;
            for (line, entry) in dotenvy::from_read_iter(text.as_slice()).enumerate() {
                match entry {
                    Ok((key, value)) => {
                        vars.insert(key, value);
                        loaded += 1;
                    }
                    // The error's text can quote the line, which may hold a secret.
                    Err(_) => tracing::warn!(file, entry = line + 1, "skipping a bad env entry"),
                }
            }
            tracing::debug!(file, loaded, stage, "loaded env file");
        }
        vars
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(dir: &std::path::Path, toml: &str) -> EnvFiles {
        let env: EnvConfig = toml::from_str(toml).expect("parses");
        EnvFiles::new(dir, &env)
    }

    #[tokio::test]
    async fn a_stage_file_overrides_the_shared_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(".env"), "FOO=shared\nBAR=1\n").expect("write");
        std::fs::write(dir.path().join(".env.local"), "BAR=2\n").expect("write");
        std::fs::write(dir.path().join(".env.qa"), "FOO=qa\n").expect("write");
        let env = files(dir.path(), "");
        let qa = env.vars("qa").await;
        assert_eq!(qa.get("FOO").map(String::as_str), Some("qa"));
        assert_eq!(qa.get("BAR").map(String::as_str), Some("2"));
        let dev = env.vars("dev").await;
        assert_eq!(dev.get("FOO").map(String::as_str), Some("shared"));
    }

    #[tokio::test]
    async fn missing_files_and_unknown_stages_give_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = files(dir.path(), "");
        assert!(env.vars("dev").await.is_empty());
        std::fs::write(dir.path().join(".env"), "FOO=1\n").expect("write");
        assert!(env.vars("staging").await.is_empty());
        assert!(EnvFiles::default().vars("dev").await.is_empty());
    }

    #[tokio::test]
    async fn a_bad_line_is_skipped_and_the_rest_still_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(".env"), "GOOD=1\nnot a pair\n").expect("write");
        let vars = files(dir.path(), "").vars("dev").await;
        assert_eq!(vars.get("GOOD").map(String::as_str), Some("1"));
    }

    #[tokio::test]
    async fn configured_names_replace_the_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("config")).expect("mkdir");
        std::fs::write(dir.path().join("config/app.env"), "FOO=1\n").expect("write");
        std::fs::write(dir.path().join(".env"), "IGNORED=1\n").expect("write");
        let env = files(dir.path(), "files = [\"config/app.env\"]\n");
        let vars = env.vars("prod").await;
        assert_eq!(vars.len(), 1);
        assert_eq!(vars.get("FOO").map(String::as_str), Some("1"));
    }
}
