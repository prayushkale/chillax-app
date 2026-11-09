//! Process groups of everything agents start (ADR 0015, amendment).
//!
//! Agent processes, test and environment shell steps and supervised apps each run in their
//! own process group. The group ids live in one process-wide registry so a normal shutdown,
//! a signal or a panic can end every dev server, `pnpm dev` or `wrangler dev` an agent left
//! behind. Signals go through `kill(1)` to avoid a libc dependency. Processes that call
//! `setsid` leave their group and are not reached (a known limit).

use std::collections::BTreeSet;
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::process::Command;

fn groups() -> &'static Mutex<BTreeSet<u32>> {
    static GROUPS: OnceLock<Mutex<BTreeSet<u32>>> = OnceLock::new();
    GROUPS.get_or_init(Mutex::default)
}

/// Remember a process group so it is ended with Chillax. Groups that are already gone are
/// dropped first, which keeps a recycled id from being signalled by mistake.
pub fn register(pgid: u32) {
    prune();
    if let Ok(mut g) = groups().lock() {
        g.insert(pgid);
    }
}

/// Stop tracking a group (its processes ended or were killed).
pub fn forget(pgid: u32) {
    if let Ok(mut g) = groups().lock() {
        g.remove(&pgid);
    }
}

/// The groups tracked now.
pub fn registered() -> Vec<u32> {
    groups()
        .lock()
        .map(|g| g.iter().copied().collect())
        .unwrap_or_default()
}

fn prune() {
    for pgid in registered() {
        if !alive_blocking(pgid) {
            forget(pgid);
        }
    }
}

/// Send `signal` (`TERM`, `KILL`, `0`) to every process of the group, blocking.
pub fn signal_blocking(pgid: u32, signal: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-s", signal, "--", &format!("-{pgid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// [`signal_blocking`] for async code.
pub async fn signal(pgid: u32, signal: &str) -> bool {
    Command::new("kill")
        .args(["-s", signal, "--", &format!("-{pgid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success())
}

fn alive_blocking(pgid: u32) -> bool {
    signal_blocking(pgid, "0")
}

/// Whether any process of the group is still there (`kill -0`).
pub async fn alive(pgid: u32) -> bool {
    signal(pgid, "0").await
}

/// End a group: SIGTERM, wait up to `grace` for it to empty, then SIGKILL survivors.
/// Forgets the group.
pub async fn terminate(pgid: u32, grace: Duration) {
    signal(pgid, "TERM").await;
    let deadline = Instant::now() + grace;
    while alive(pgid).await && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if alive(pgid).await {
        signal(pgid, "KILL").await;
    }
    forget(pgid);
}

/// After a step's leader exited normally: keep the group tracked only while something of
/// it is still running (a server the step started on purpose), so it is stopped at exit.
pub async fn settle(pgid: u32) {
    if !alive(pgid).await {
        forget(pgid);
    }
}

/// [`terminate`] for every registered group at once.
pub async fn terminate_all(grace: Duration) {
    let all = registered();
    futures::future::join_all(all.into_iter().map(|pgid| terminate(pgid, grace))).await;
}

/// Blocking [`terminate_all`] for panic hooks and signal handlers.
pub fn kill_all(grace: Duration) {
    let all = registered();
    for pgid in &all {
        signal_blocking(*pgid, "TERM");
    }
    let deadline = Instant::now() + grace;
    while all.iter().any(|p| alive_blocking(*p)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    for pgid in all {
        if alive_blocking(pgid) {
            signal_blocking(pgid, "KILL");
        }
        forget(pgid);
    }
}
