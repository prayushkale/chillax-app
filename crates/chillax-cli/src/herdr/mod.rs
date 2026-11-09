//! The herdr adapter (ADR 0018): reports Chillax's state to the herdr terminal workspace
//! manager and sends notifications through it. Everything herdr-specific lives here;
//! `chillax-core` only knows [`Notifier`](chillax_core::Notifier) and the pure summaries.

mod client;
mod open;
mod reporter;
mod verbs;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use chillax_core::config::{HerdrConfig, HerdrMode};
use chillax_core::{AgentState, CompanyHandle, Level, Notification};
use tokio::task::JoinHandle;

#[cfg(test)]
pub use client::RecordingHerdr;
pub use client::{CliHerdr, HerdrClient, HerdrError};
pub use open::{HerdrCmd, run};
pub use verbs::{Direction, NewPane, SplitTarget, shell_quote};

/// The herdr this process runs inside, from the environment herdr sets in every pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HerdrEnv {
    pub bin: PathBuf,
    pub pane: String,
}

impl HerdrEnv {
    /// Some only when `HERDR_ENV=1`, the pane id is set and the binary exists.
    pub fn detect(env: &BTreeMap<String, String>) -> Option<Self> {
        let get = |k: &str| env.get(k).map(|v| v.trim()).filter(|v| !v.is_empty());
        if get("HERDR_ENV") != Some("1") {
            return None;
        }
        let pane = get("HERDR_PANE_ID")?.to_string();
        let bin = PathBuf::from(get("HERDR_BIN_PATH")?);
        bin.is_file().then_some(Self { bin, pane })
    }
}

/// Whether to talk to herdr. `On` without herdr warns once and stays off.
pub fn activate(mode: HerdrMode, env: &BTreeMap<String, String>) -> Option<HerdrEnv> {
    match mode {
        HerdrMode::Off => None,
        HerdrMode::Auto => HerdrEnv::detect(env),
        HerdrMode::On => {
            let found = HerdrEnv::detect(env);
            if found.is_none() {
                tracing::warn!(
                    "[herdr] enabled = true, but Chillax is not running in a herdr pane \
                     (HERDR_ENV, HERDR_PANE_ID, HERDR_BIN_PATH); not reporting to herdr"
                );
            }
            found
        }
    }
}

/// The herdr versions this adapter was written against (ADR 0018).
const SUPPORTED: &str = "0.9.";

/// The `chillax doctor` lines for herdr; empty when herdr is absent and not asked for. A
/// problem is a `!` warning, never a failure: herdr is optional.
pub async fn doctor(mode: HerdrMode, env: &BTreeMap<String, String>) -> Vec<String> {
    let in_herdr = env.get("HERDR_ENV").is_some_and(|v| v.trim() == "1");
    if !in_herdr && mode != HerdrMode::On {
        return Vec::new();
    }
    let Some(bin) = env.get("HERDR_BIN_PATH").filter(|v| !v.trim().is_empty()) else {
        return vec!["! herdr: HERDR_BIN_PATH is not set; run chillax inside a herdr pane".into()];
    };
    let bin = PathBuf::from(bin);
    if !is_executable(&bin) {
        return vec![format!(
            "! herdr: HERDR_BIN_PATH {} is not an executable file",
            bin.display()
        )];
    }
    match CliHerdr::new(bin).run(&["--version".to_string()]).await {
        Ok(out) => {
            let version = out.trim();
            let known = version.split_whitespace().any(|w| w.starts_with(SUPPORTED));
            vec![if known {
                format!("✓ herdr: {version}")
            } else {
                format!(
                    "! herdr: {version}; Chillax is tested with {SUPPORTED}x, so reports may be ignored"
                )
            }]
        }
        Err(error) => vec![format!("! herdr: {error}")],
    }
}

fn is_executable(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Consecutive failures after which herdr reporting turns itself off for this run.
const MAX_FAILURES: u32 = 5;

#[derive(Default)]
struct Health {
    failures: u32,
    retry_after: Option<Instant>,
    last_error: Option<String>,
    disabled: bool,
}

/// The typed layer over [`HerdrClient`]: builds the argument vectors, and degrades
/// silently. A failing herdr is logged once per distinct error, backed off, and given up
/// on after [`MAX_FAILURES`] in a row; nothing here ever returns an error or panics.
pub struct Herdr {
    client: Arc<dyn HerdrClient>,
    pane: String,
    source: String,
    seq: AtomicU64,
    health: Mutex<Health>,
    /// First pause after a failure; doubles per failure up to 30 seconds.
    backoff: Duration,
}

/// The agent label herdr shows for Chillax.
const AGENT: &str = "chillax";

/// How long herdr keeps the hires metadata without a refresh, so a crashed Chillax does
/// not leave stale names behind.
const HIRES_TTL_MS: u64 = 60_000;

/// Per-hire detail for herdr's metadata: ids and names only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HiresMeta {
    /// The `hires` token: a compact `Dana working T-4 · Sam idle`.
    pub hires: String,
    /// The `active` token: how many hires are working.
    pub active: usize,
    /// `STATUS=TEXT` labels herdr shows for the pane's current state.
    pub labels: Vec<(&'static str, String)>,
    pub display_agent: String,
}

impl Herdr {
    pub fn new(client: Arc<dyn HerdrClient>, pane: String, project: &str) -> Self {
        Self {
            client,
            pane,
            source: format!("chillax:{}", chillax_core::engine::slug(project)),
            // Milliseconds since the epoch, so seq rises across restarts too.
            seq: AtomicU64::new(chrono::Utc::now().timestamp_millis().max(0) as u64),
            health: Mutex::default(),
            backoff: Duration::from_secs(1),
        }
    }

    #[cfg(test)]
    fn without_backoff(mut self) -> Self {
        self.backoff = Duration::ZERO;
        self
    }

    #[cfg(test)]
    pub fn is_disabled(&self) -> bool {
        self.health.lock().expect("lock").disabled
    }

    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// `pane report-agent`: the semantic state, plus how to resume Chillax after herdr
    /// restarts (the source already holds the pane in the same call).
    pub fn report_agent_args(&self, state: AgentState, seq: u64) -> Vec<String> {
        let label = match state {
            AgentState::Idle => "idle",
            AgentState::Working => "working",
            AgentState::Blocked => "blocked",
        };
        let mut args = self.base("report-agent");
        args.extend(["--state".into(), label.into()]);
        if state == AgentState::Blocked {
            args.extend(["--message".into(), "Waiting for you in Chillax".into()]);
        }
        args.extend(["--seq".into(), seq.to_string()]);
        // herdr takes no trailing command: a `--` is rejected as an unknown option.
        args.extend(["--agent-session-id".into(), self.source.clone()]);
        args
    }

    pub fn release_agent_args(&self, seq: u64) -> Vec<String> {
        let mut args = self.base("release-agent");
        args.extend(["--seq".into(), seq.to_string()]);
        args
    }

    /// `pane report-metadata`: display-only tokens. `None` clears a token.
    pub fn report_metadata_args(
        &self,
        tokens: &BTreeMap<&'static str, Option<String>>,
        seq: u64,
    ) -> Vec<String> {
        let mut args = self.base("report-metadata");
        for (name, value) in tokens {
            match value {
                Some(v) => args.extend(["--token".into(), format!("{name}={v}")]),
                None => args.extend(["--clear-token".into(), (*name).into()]),
            }
        }
        args.extend(["--seq".into(), seq.to_string()]);
        args
    }

    /// `pane report-metadata` for the per-hire detail, expiring unless refreshed.
    pub fn report_hires_args(&self, meta: &HiresMeta, seq: u64) -> Vec<String> {
        let mut args = self.base("report-metadata");
        args.extend([
            "--token".into(),
            format!("hires={}", flag_safe(&meta.hires, 80)),
            "--token".into(),
            format!("active={}", meta.active),
        ]);
        for (status, text) in &meta.labels {
            args.extend([
                "--state-label".into(),
                format!("{status}={}", flag_safe(text, 120)),
            ]);
        }
        args.extend([
            "--display-agent".into(),
            flag_safe(&meta.display_agent, 60),
            "--ttl-ms".into(),
            HIRES_TTL_MS.to_string(),
            "--seq".into(),
            seq.to_string(),
        ]);
        args
    }

    /// Everything [`Self::report_hires_args`] sets, cleared. With `labels_only`, just the
    /// state labels (so a label that no longer applies does not linger).
    pub fn clear_hires_args(&self, labels_only: bool, seq: u64) -> Vec<String> {
        let mut args = self.base("report-metadata");
        args.push("--clear-state-labels".into());
        if !labels_only {
            args.extend([
                "--clear-token".into(),
                "hires".into(),
                "--clear-token".into(),
                "active".into(),
                "--clear-display-agent".into(),
            ]);
        }
        args.extend(["--seq".into(), seq.to_string()]);
        args
    }

    /// `notification show`. The sound is for things that need the stakeholder.
    pub fn notification_args(n: &Notification) -> Vec<String> {
        let mut args = vec![
            "notification".to_string(),
            "show".into(),
            flag_safe(&n.title, 120),
        ];
        if !n.body.trim().is_empty() {
            args.extend(["--body".into(), flag_safe(&n.body, 240)]);
        }
        if n.level != Level::Info {
            args.extend(["--sound".into(), "request".into()]);
        }
        args
    }

    fn base(&self, verb: &str) -> Vec<String> {
        vec![
            "pane".into(),
            verb.into(),
            self.pane.clone(),
            "--source".into(),
            self.source.clone(),
            "--agent".into(),
            AGENT.into(),
        ]
    }

    pub async fn report_agent(&self, state: AgentState) -> bool {
        self.call(self.report_agent_args(state, self.next_seq()))
            .await
    }

    pub async fn release(&self) -> bool {
        self.run(self.release_agent_args(self.next_seq())).await
    }

    pub async fn report_hires(&self, meta: &HiresMeta) -> bool {
        self.call(self.report_hires_args(meta, self.next_seq()))
            .await
    }

    pub async fn clear_hires(&self, labels_only: bool) -> bool {
        self.call(self.clear_hires_args(labels_only, self.next_seq()))
            .await
    }

    /// Clear the hires metadata on quit. Like [`Self::release`] it ignores backoff.
    pub async fn release_hires(&self) -> bool {
        self.run(self.clear_hires_args(false, self.next_seq()))
            .await
    }

    pub async fn report_metadata(&self, tokens: &BTreeMap<&'static str, Option<String>>) -> bool {
        self.call(self.report_metadata_args(tokens, self.next_seq()))
            .await
    }

    pub async fn show(&self, n: &Notification) -> bool {
        self.call(Self::notification_args(n)).await
    }

    /// Run one command under the failure policy. True when herdr accepted it.
    async fn call(&self, args: Vec<String>) -> bool {
        {
            let health = self.health.lock().expect("lock");
            if health.disabled || health.retry_after.is_some_and(|t| Instant::now() < t) {
                return false;
            }
        }
        self.run(args).await
    }

    /// Run one command and record the outcome, without the backoff / disabled check.
    /// The release on quit uses this: skipping it would leave the pane "running".
    async fn run(&self, args: Vec<String>) -> bool {
        let result = self.client.run(&args).await;
        let mut health = self.health.lock().expect("lock");
        match result {
            Ok(_) => {
                health.failures = 0;
                health.retry_after = None;
                true
            }
            Err(error) => {
                health.failures += 1;
                let text = error.to_string();
                if health.last_error.as_deref() != Some(text.as_str()) {
                    tracing::warn!(%error, "herdr call failed");
                    health.last_error = Some(text);
                }
                if health.failures >= MAX_FAILURES {
                    health.disabled = true;
                    tracing::warn!(
                        "herdr failed {MAX_FAILURES} times in a row; no longer reporting to it"
                    );
                } else {
                    let pause = self.backoff * 2u32.pow(health.failures - 1);
                    health.retry_after = Some(Instant::now() + pause.min(Duration::from_secs(30)));
                }
                false
            }
        }
    }
}

/// Text for a positional or flag value: single line, bounded, and never starting with `-`
/// (herdr would read it as a flag).
fn flag_safe(text: &str, max: usize) -> String {
    let line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = line.chars().take(max).collect();
    if out.starts_with('-') {
        out.insert(0, ' ');
    }
    out
}

/// A running reporter: the background task and the herdr it talks to.
pub struct HerdrHandle {
    task: JoinHandle<()>,
    herdr: Arc<Herdr>,
    metadata: bool,
}

impl HerdrHandle {
    /// Stop reporting and clear Chillax's entry in herdr. Bounded; never fails.
    pub async fn shutdown(&self) {
        self.task.abort();
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            if self.metadata {
                self.herdr.release_hires().await;
            }
            self.herdr.release().await
        })
        .await;
    }
}

/// Start reporting when `[herdr]` and the environment say so.
pub fn start(
    config: &HerdrConfig,
    project: &str,
    company: Arc<dyn CompanyHandle>,
) -> Option<HerdrHandle> {
    let env = activate(config.enabled, &std::env::vars().collect())?;
    let client = Arc::new(CliHerdr::new(env.bin));
    Some(spawn(client, env.pane, project, config, company))
}

fn spawn(
    client: Arc<dyn HerdrClient>,
    pane: String,
    project: &str,
    config: &HerdrConfig,
    company: Arc<dyn CompanyHandle>,
) -> HerdrHandle {
    let herdr = Arc::new(Herdr::new(client, pane, project));
    let options = reporter::Options {
        metadata: config.metadata,
        notifications: config.notifications,
        debounce: Duration::from_millis(300),
        hires_interval: Duration::from_secs(2),
        hires_refresh: Duration::from_secs(40),
    };
    // Subscribe before the task starts so nothing between here and the first snapshot is
    // missed, and the replay of old events never notifies.
    let events = company.subscribe();
    let task = tokio::spawn(reporter::run(company, herdr.clone(), events, options));
    HerdrHandle {
        task,
        herdr,
        metadata: config.metadata,
    }
}

#[cfg(test)]
mod tests;
