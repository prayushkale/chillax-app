//! Running apps (ADR 0015): the dev, QA and prod apps Chillax keeps running on
//! localhost ports. This module holds the read model and the pure decision of when an app
//! should be (re)started or stopped; `chillax-dispatch` owns the processes.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::APP_ENVS;
use crate::dispatch::{Action, Context};
use crate::engine::Engine;

/// Why an app that has no code to run is stopped, as shown to the stakeholder.
pub const STOPPED_BY_YOU: &str = "stopped by you";

/// What a configured app looks like in the event log (the command stays in the config).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppInfo {
    pub env: String,
    pub port: u16,
    pub url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppStatus {
    /// Being (re)started, or waiting to restart after a crash.
    Starting,
    Running,
    /// It kept crashing and Chillax gave up; restart it by hand or push new code.
    Crashed,
    Stopped,
}

impl AppStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Crashed => "crashed",
            Self::Stopped => "stopped",
        }
    }
}

/// What the stakeholder asked of an app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppWant {
    Start,
    Stop,
    Restart,
}

/// One supervised app, as projected from the events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunningApp {
    /// `dev`, `qa` or `prod`.
    pub env: String,
    pub port: u16,
    pub url: String,
    pub status: AppStatus,
    /// False once the stakeholder stopped it: Chillax then leaves it stopped.
    pub wanted: bool,
    /// A start or restart was asked for and has not happened yet.
    #[serde(default)]
    pub restart: bool,
    /// The commit it runs (or last ran).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// When the status last changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<DateTime<Utc>>,
    /// Why it is not running (no release yet, crashed with exit 1, stopped by you).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Restarts since it was configured, crash retries included.
    #[serde(default)]
    pub restarts: u32,
}

impl RunningApp {
    pub fn new(info: &AppInfo) -> Self {
        Self {
            env: info.env.clone(),
            port: info.port,
            url: info.url.clone(),
            status: AppStatus::Stopped,
            wanted: true,
            restart: false,
            commit: None,
            since: None,
            reason: None,
            restarts: 0,
        }
    }

    /// Whether the stakeholder may stop it now.
    pub fn can_stop(&self) -> bool {
        self.wanted && matches!(self.status, AppStatus::Running | AppStatus::Starting)
    }

    /// Whether the stakeholder may start it now: it was stopped by hand or gave up.
    pub fn can_start(&self) -> bool {
        !self.wanted || self.status == AppStatus::Crashed
    }

    /// Whether the stakeholder may restart it now.
    pub fn can_restart(&self) -> bool {
        self.wanted
            && matches!(
                self.status,
                AppStatus::Running | AppStatus::Starting | AppStatus::Crashed
            )
    }

    /// Whether the app answers at its URL right now.
    pub fn is_running(&self) -> bool {
        self.status == AppStatus::Running
    }
}

/// The most apps one employee keeps reported at once; a re-report of a label replaces it.
pub const MAX_REPORTED_PER_EMPLOYEE: usize = 5;
/// The reporter recorded for apps Chillax detects by itself; not a hire.
pub const DETECTED_BY: &str = "chillax";
const MAX_LABEL: usize = 40;
const MAX_NOTE: usize = 200;
const MAX_URL: usize = 300;

/// An app an employee says they started (a dev server in their worktree, a preview), as
/// projected from `AppReported`. Chillax does not supervise or probe it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportedApp {
    /// `dev`, `qa`, `prod`, a ticket id or a free-form label.
    pub label: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<String>,
    pub employee: String,
    /// The worktree it runs from, relative to the repository root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// When it was last reported.
    pub since: DateTime<Utc>,
}

/// Where a dashboard row comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppSource {
    /// Chillax runs it (ADR 0015).
    Supervised,
    /// This employee said they started it.
    Reported(String),
}

/// One row of the dashboard's "Running apps" list: a supervised and a reported app merged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningAppView {
    pub label: String,
    pub url: String,
    pub port: Option<u16>,
    pub status: AppStatus,
    pub source: AppSource,
    pub commit: Option<String>,
    pub ticket: Option<String>,
    pub worktree: Option<String>,
    pub note: Option<String>,
    pub since: Option<DateTime<Utc>>,
}

impl RunningAppView {
    /// Whether something answers at the URL as far as Chillax knows.
    pub fn is_running(&self) -> bool {
        self.status == AppStatus::Running
    }
}

/// Order of the well-known labels; everything else follows in report order.
fn label_rank(label: &str) -> usize {
    APP_ENVS
        .iter()
        .position(|e| *e == label)
        .unwrap_or(APP_ENVS.len())
}

/// Merge supervised and reported apps, grouped by label (dev, qa, prod, then the rest).
/// A reported and a supervised app of the same label both stay: they can differ.
pub fn merge_running(supervised: &[RunningApp], reported: &[ReportedApp]) -> Vec<RunningAppView> {
    let mut rows: Vec<RunningAppView> = supervised
        .iter()
        .map(|a| RunningAppView {
            label: a.env.clone(),
            url: a.url.clone(),
            port: Some(a.port),
            status: a.status,
            source: AppSource::Supervised,
            commit: a.commit.clone(),
            ticket: None,
            worktree: None,
            note: a.reason.clone(),
            since: a.since,
        })
        .chain(reported.iter().map(|a| RunningAppView {
            label: a.label.clone(),
            url: a.url.clone(),
            port: a.port,
            status: AppStatus::Running,
            source: AppSource::Reported(a.employee.clone()),
            commit: None,
            ticket: a.ticket.clone(),
            worktree: a.worktree.clone(),
            note: a.note.clone(),
            since: Some(a.since),
        }))
        .collect();
    // Stable, so supervised rows stay ahead of reported ones within a label.
    rows.sort_by_key(|r| label_rank(&r.label));
    rows
}

/// A label for a reported app: `dev`/`qa`/`prod` in any case, else free text of at most 40
/// characters without control characters.
pub fn normalize_label(raw: &str) -> Result<String, String> {
    let label = raw.trim();
    if label.is_empty() {
        return Err("env must not be empty".into());
    }
    if label.chars().count() > MAX_LABEL {
        return Err(format!("env is longer than {MAX_LABEL} characters"));
    }
    if label.chars().any(char::is_control) {
        return Err("env must be plain text".into());
    }
    let lower = label.to_lowercase();
    Ok(if APP_ENVS.contains(&lower.as_str()) {
        lower
    } else {
        label.to_string()
    })
}

/// A note: one line of plain text, at most 200 characters.
pub fn normalize_note(raw: &str) -> Result<Option<String>, String> {
    let note = raw.trim();
    if note.is_empty() {
        return Ok(None);
    }
    if note.chars().count() > MAX_NOTE {
        return Err(format!("note is longer than {MAX_NOTE} characters"));
    }
    if note.chars().any(char::is_control) {
        return Err("note must be one line of plain text".into());
    }
    Ok(Some(note.to_string()))
}

/// The URL of a port on this machine.
pub fn localhost_url(port: u16) -> String {
    format!("http://localhost:{port}")
}

/// Whether `host` is this machine: `localhost`, `*.localhost`, `127.x.x.x` or `[::1]`.
fn is_local_host(host: &str) -> bool {
    if host == "[::1]" || host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    let parts: Vec<&str> = host.split('.').collect();
    parts.len() == 4
        && parts[0] == "127"
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.len() <= 3
                && p.bytes().all(|b| b.is_ascii_digit())
                && p.parse::<u8>().is_ok()
        })
}

/// Check and normalize a URL an employee reports. Only http(s) on this machine passes, so a
/// report can never point the stakeholder's browser at someone else's site. Returns the URL
/// and its explicit port.
pub fn normalize_reported_url(raw: &str) -> Result<(String, Option<u16>), String> {
    let raw = raw.trim();
    if raw.chars().count() > MAX_URL {
        return Err("url is too long".into());
    }
    if raw
        .chars()
        .any(|c| !c.is_ascii() || c.is_control() || c.is_whitespace() || c == '\\' || c == '@')
    {
        return Err(
            "url must not contain spaces, control or non-ASCII characters, backslashes or '@'"
                .into(),
        );
    }
    let lower = raw.to_lowercase();
    let (scheme, rest) = if let Some(rest) = lower.strip_prefix("http://") {
        ("http", rest)
    } else if let Some(rest) = lower.strip_prefix("https://") {
        ("https", rest)
    } else {
        return Err("url must start with http:// or https://".into());
    };
    let (authority, tail) = match rest.find(['/', '?', '#']) {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    let (host, port) = if let Some(after) = authority.strip_prefix('[') {
        let end = after.find(']').ok_or("url has a malformed host")?;
        let host = &authority[..end + 2];
        let port = after[end + 1..].strip_prefix(':');
        (host, port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        }
    };
    if !is_local_host(host) {
        return Err("url must point at this machine (localhost or 127.0.0.1)".into());
    }
    let port = match port {
        None => None,
        Some(p) => {
            if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                return Err("url has a malformed port".into());
            }
            match p.parse::<u16>() {
                Ok(n) if n >= 1 => Some(n),
                _ => return Err("port must be between 1 and 65535".into()),
            }
        }
    };
    // Keep the path as typed (only the scheme and host are case-insensitive).
    let tail_start = raw.len() - tail.len();
    let tail = &raw[tail_start..];
    let port_part = port.map(|p| format!(":{p}")).unwrap_or_default();
    Ok((format!("{scheme}://{host}{port_part}{tail}"), port))
}

/// What an environment's app should be running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppTarget {
    /// Run this commit.
    Commit(String),
    /// Nothing to run yet, with the reason to show (prod before the first release).
    Idle(String),
}

/// Short commit for messages.
pub fn short(commit: &str) -> &str {
    commit.get(..7).unwrap_or(commit)
}

/// Starts, restarts and stops that bring the apps in line with the code, one per app.
/// Runs even while dispatch is paused: apps are not agent work.
pub fn app_actions(engine: &Engine, ctx: &Context, actions: &mut Vec<Action>) {
    for env in APP_ENVS {
        let Some(app) = engine.state.app(env) else {
            continue;
        };
        let Some(target) = ctx.app_targets.get(env) else {
            continue;
        };
        if ctx.busy.contains(crate::dispatch::app_claim(env)) {
            continue;
        }
        let live = ctx.app_live.get(env);
        if !app.wanted {
            if live.is_some() || app.status != AppStatus::Stopped {
                actions.push(Action::StopApp {
                    env: env.to_string(),
                    reason: STOPPED_BY_YOU.to_string(),
                });
            }
            continue;
        }
        match target {
            AppTarget::Idle(reason) => {
                let settled = live.is_none()
                    && app.status == AppStatus::Stopped
                    && app.reason.as_deref() == Some(reason.as_str());
                if !settled {
                    actions.push(Action::StopApp {
                        env: env.to_string(),
                        reason: reason.clone(),
                    });
                }
            }
            AppTarget::Commit(commit) => {
                let why = match live {
                    Some(running) if running == commit && !app.restart => continue,
                    Some(_) if app.restart => "restart requested".to_string(),
                    Some(running) => {
                        format!("{} moved from {} to {}", env, short(running), short(commit))
                    }
                    None if app.status == AppStatus::Crashed
                        && !app.restart
                        && app.commit.as_deref() == Some(commit.as_str()) =>
                    {
                        // It crashed on this code and Chillax gave up; new code or the
                        // stakeholder's restart tries again.
                        continue;
                    }
                    None => format!("{} at {}", env, short(commit)),
                };
                actions.push(Action::RunApp {
                    env: env.to_string(),
                    commit: commit.clone(),
                    why,
                });
            }
        }
    }
}
