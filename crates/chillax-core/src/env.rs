//! The environment adapter: runs a project's own commands for an
//! environment (`up`, `test`, `down`, `deploy`). `chillax-dispatch` implements it by
//! running shell commands; the core only names the shape.

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;

use crate::model::EnvStep;

#[derive(Debug, thiserror::Error)]
pub enum EnvError {
    #[error("could not run `{command}`: {detail}")]
    Spawn { command: String, detail: String },
}

/// What one command did. A command that exits non-zero or times out is a result
/// (`ok = false`), not an error: the caller records it and decides what to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvRun {
    pub ok: bool,
    /// No command is configured for the step, so nothing ran.
    pub skipped: bool,
    pub exit: Option<i32>,
    /// The end of stdout and stderr, capped.
    pub output_tail: String,
    pub duration: Duration,
}

impl EnvRun {
    pub fn skipped() -> Self {
        Self {
            ok: true,
            skipped: true,
            exit: None,
            output_tail: String::new(),
            duration: Duration::ZERO,
        }
    }
}

#[async_trait]
pub trait Environment: Send + Sync {
    /// Run `step` of environment `env` (`dev`, `qa` or `prod`) in `cwd`. A step with no
    /// configured command is [`EnvRun::skipped`].
    async fn run(&self, env: &str, step: EnvStep, cwd: &Path) -> Result<EnvRun, EnvError>;
}
