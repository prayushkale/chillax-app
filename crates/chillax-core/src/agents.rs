//! Which coding agents (ACP backends) and models the stakeholder can hire on.
//!
//! Configured backends come from `[agents.*]` in `chillax.toml`; known presets are
//! offered when their command is installed, and adding one writes it to the config.
//! The [`AgentDirectory`] adapter does the IO (probing agents for their models);
//! the data and matching rules here are pure so every frontend agrees.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::config::AgentConfig;

/// A backend the stakeholder can pick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendInfo {
    /// The `[agents.<name>]` key, e.g. `claude`.
    pub name: String,
    /// Human label, e.g. `Claude Code`.
    pub title: String,
    pub command: String,
    /// In `chillax.toml` already. Unconfigured presets are added on first use.
    pub configured: bool,
    /// The command was found on `PATH`.
    pub available: bool,
}

/// A model an agent offers (ACP session config option of category `model`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelOption {
    /// The value to select, e.g. `sonnet`.
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum AgentDirectoryError {
    #[error("no agent backend `{0}`; configure it under [agents.{0}] in chillax.toml")]
    Unknown(String),
    #[error("{0}")]
    Agent(String),
    #[error("{0}")]
    Config(String),
}

/// Lists backends and their models for the hire flow and Settings. Implemented in
/// `chillax-agents`; the CLI reaches a running server's over `/api/v1/agents`.
#[async_trait]
pub trait AgentDirectory: Send + Sync {
    /// Configured backends first, then installed presets that aren't configured yet.
    async fn backends(&self) -> Vec<BackendInfo>;
    /// The models a backend offers. Starts the agent (no prompt, so no cost) the first
    /// time; cached afterwards.
    async fn models(&self, backend: &str) -> Result<Vec<ModelOption>, AgentDirectoryError>;
    /// The effort (thought level) choices a backend offers, empty when it has none.
    /// Read from the same probe as [`models`](Self::models), so with the agent's default
    /// model: the list may not match every model.
    async fn efforts(&self, _backend: &str) -> Result<Vec<ModelOption>, AgentDirectoryError> {
        Ok(Vec::new())
    }
    /// Add a known preset to `chillax.toml` so it can be hired on. No-op if configured.
    async fn enable(&self, backend: &str) -> Result<BackendInfo, AgentDirectoryError>;
}

/// The agents Chillax knows how to launch.
pub fn known_presets() -> Vec<(&'static str, &'static str, AgentConfig)> {
    let preset = |command: &str, args: &[&str]| AgentConfig {
        command: command.into(),
        args: args.iter().map(|a| a.to_string()).collect(),
        env: Default::default(),
        tokens_cumulative: false,
    };
    vec![
        (
            "claude",
            "Claude Code",
            preset("npx", &["-y", "@agentclientprotocol/claude-agent-acp"]),
        ),
        ("codex", "Codex", preset("codex-acp", &[])),
        ("gemini", "Gemini CLI", preset("gemini", &["--acp"])),
        (
            "command-code",
            "Command Code",
            preset("command-code", &["acp"]),
        ),
    ]
}

/// Display name for a backend key.
pub fn backend_title(name: &str) -> String {
    known_presets()
        .into_iter()
        .find(|(key, ..)| *key == name)
        .map_or_else(|| name.to_string(), |(_, title, _)| title.to_string())
}

/// Find the option for a wanted model. Exact ids win. Otherwise the option whose id or
/// name, split into words, best covers the wanted id: `Sonnet 5.5` (alias `sonnet`)
/// for `claude-sonnet-5-5`, not `claude-sonnet-5`. A match that leaves a version
/// number of the wanted id unexplained only counts when nothing better exists.
pub fn match_model<'a>(options: &'a [ModelOption], wanted: &str) -> Option<&'a ModelOption> {
    let wanted = wanted.trim().to_lowercase();
    if wanted.is_empty() {
        return None;
    }
    if let Some(exact) = options.iter().find(|o| o.id.to_lowercase() == wanted) {
        return Some(exact);
    }
    let wanted = words(&wanted);
    options
        .iter()
        .filter(|o| o.id != "default")
        .filter_map(|o| {
            let score = [words(&o.id), words(&o.name)]
                .iter()
                .filter_map(|candidate| coverage(&wanted, candidate))
                .max()?;
            Some((score, o))
        })
        .max_by_key(|(score, _)| *score)
        .map(|(_, o)| o)
}

/// Lowercase alphanumeric runs: `Sonnet 5.5` → `[sonnet, 5, 5]`.
fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// How well `candidate` covers `wanted` when it appears in it as a contiguous run:
/// (no version numbers left over, words covered).
fn coverage(wanted: &[String], candidate: &[String]) -> Option<(bool, usize)> {
    if candidate.is_empty() || candidate.len() > wanted.len() {
        return None;
    }
    let start = (0..=wanted.len() - candidate.len())
        .find(|&i| wanted[i..i + candidate.len()] == *candidate)?;
    let numeric_left = wanted[..start]
        .iter()
        .chain(&wanted[start + candidate.len()..])
        .any(|w| w.chars().all(|c| c.is_ascii_digit()));
    Some((!numeric_left, candidate.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn option(id: &str) -> ModelOption {
        ModelOption {
            id: id.into(),
            name: id.into(),
            description: None,
        }
    }

    #[test]
    fn matches_exact_ids_then_aliases() {
        let options = [option("default"), option("opus"), option("sonnet")];
        assert_eq!(
            match_model(&options, "Sonnet").map(|o| o.id.as_str()),
            Some("sonnet")
        );
        assert_eq!(
            match_model(&options, "claude-sonnet-5-5").map(|o| o.id.as_str()),
            Some("sonnet")
        );
        assert_eq!(
            match_model(&options, "default").map(|o| o.id.as_str()),
            Some("default")
        );
        assert_eq!(match_model(&options, "gpt-5"), None);
        assert_eq!(match_model(&options, " "), None);
    }

    #[test]
    fn versioned_ids_match_the_right_alias() {
        // What claude-agent-acp offers: aliases named by version, plus pinned ids.
        let named = |id: &str, name: &str| ModelOption {
            id: id.into(),
            name: name.into(),
            description: None,
        };
        let options = [
            named("default", "Default (recommended)"),
            named("opus", "Opus 5.5"),
            named("sonnet", "Sonnet 5.5"),
            named("claude-sonnet-5", "Sonnet 5"),
            named("claude-opus-4-8", "Opus 4.8"),
        ];
        let pick = |wanted| match_model(&options, wanted).map(|o| o.id.as_str());
        assert_eq!(pick("claude-sonnet-5-5"), Some("sonnet"));
        assert_eq!(pick("claude-sonnet-5"), Some("claude-sonnet-5"));
        assert_eq!(pick("claude-opus-5-5"), Some("opus"));
        assert_eq!(pick("opus-4-8"), Some("claude-opus-4-8"));
        assert_eq!(pick("Sonnet 5"), Some("claude-sonnet-5"));
    }

    #[test]
    fn presets_have_titles() {
        assert_eq!(backend_title("codex"), "Codex");
        assert_eq!(backend_title("command-code"), "Command Code");
        assert_eq!(backend_title("my-agent"), "my-agent");
    }
}
