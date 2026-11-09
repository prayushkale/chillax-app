//! Running the `herdr` CLI. Arguments only, never a shell (AGENTS.md rule 3, ADR 0018).

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;

/// Why a herdr call failed. Text only: the reporter logs it once and moves on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HerdrError(pub String);

impl std::fmt::Display for HerdrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HerdrError {}

/// Runs `herdr <args>` and returns its standard output.
#[async_trait]
pub trait HerdrClient: Send + Sync {
    async fn run(&self, args: &[String]) -> Result<String, HerdrError>;
}

/// The real thing: spawns the binary `HERDR_BIN_PATH` names.
pub struct CliHerdr {
    bin: PathBuf,
    timeout: Duration,
}

impl CliHerdr {
    pub fn new(bin: PathBuf) -> Self {
        Self {
            bin,
            timeout: Duration::from_secs(3),
        }
    }
}

#[async_trait]
impl HerdrClient for CliHerdr {
    async fn run(&self, args: &[String]) -> Result<String, HerdrError> {
        let child = tokio::process::Command::new(&self.bin)
            .args(args)
            // herdr must never read the terminal Chillax's TUI owns.
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| HerdrError(format!("could not run herdr: {e}")))?;
        let out = tokio::time::timeout(self.timeout, child.wait_with_output())
            .await
            .map_err(|_| HerdrError(format!("herdr did not answer within {:?}", self.timeout)))?
            .map_err(|e| HerdrError(format!("herdr failed: {e}")))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            let err = String::from_utf8_lossy(&out.stderr);
            Err(HerdrError(format!(
                "herdr {} exited with {}: {}",
                args.iter().take(2).cloned().collect::<Vec<_>>().join(" "),
                out.status,
                err.trim()
            )))
        }
    }
}

/// Test fake: records every argument vector and replays canned results.
#[cfg(test)]
#[derive(Default)]
pub struct RecordingHerdr {
    calls: std::sync::Mutex<Vec<Vec<String>>>,
    fail: std::sync::atomic::AtomicBool,
    replies: std::sync::Mutex<std::collections::VecDeque<String>>,
}

#[cfg(test)]
impl RecordingHerdr {
    pub fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().expect("lock").clone()
    }

    /// Queue the stdout of the next call that has none queued (default: empty).
    pub fn push_reply(&self, out: &str) {
        self.replies.lock().expect("lock").push_back(out.into());
    }

    /// Every call from now on fails (or succeeds again).
    pub fn set_failing(&self, fail: bool) {
        self.fail.store(fail, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
#[async_trait]
impl HerdrClient for RecordingHerdr {
    async fn run(&self, args: &[String]) -> Result<String, HerdrError> {
        self.calls.lock().expect("lock").push(args.to_vec());
        if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
            Err(HerdrError("herdr is down".into()))
        } else {
            Ok(self
                .replies
                .lock()
                .expect("lock")
                .pop_front()
                .unwrap_or_default())
        }
    }
}
