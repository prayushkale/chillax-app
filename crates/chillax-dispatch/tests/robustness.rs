//! How the dev loop copes when agents misbehave: idle turns, failures, usage limits,
//! sign-out and the refresh command. Real git repositories, the real dispatcher and
//! runtime, and the scripted `fake-acp-agent` (no model, no credits).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chillax_agents::{AgentRuntime, RuntimeConfig, shared_backends};
use chillax_core::config::{AgentConfig, PermissionMode};
use chillax_core::engine::State;
use chillax_core::limits::Jitter;
use chillax_core::{
    Actor, Command, CompanyHandle, Engine, Event, EventKind, Hub, InboxItem, MemoryLog, Role,
    RoleCatalog, TicketKind, Workflow,
};
use chillax_dispatch::{DispatchConfig, Dispatcher};
use chillax_vcs::LocalGit;
use tokio::sync::broadcast;

fn fake_agent() -> PathBuf {
    let exe = std::env::current_exe().expect("test binary path");
    let debug = exe.parent().and_then(Path::parent).expect("target dir");
    let agent = debug.join("fake-acp-agent");
    if !agent.exists() {
        let built = std::process::Command::new("cargo")
            .args(["build", "-p", "chillax-agents", "--bin", "fake-acp-agent"])
            .status()
            .expect("cargo runs");
        assert!(built.success(), "building the fake agent");
    }
    agent
}

fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?}");
}

/// Probes come due almost at once, whatever the real schedule would be.
struct Quick;
impl Jitter for Quick {
    fn between(&self, _lo: Duration, _hi: Duration) -> Duration {
        Duration::from_millis(100)
    }
}

struct Opts {
    env: BTreeMap<String, String>,
    idle_grace: Duration,
    backoff: Duration,
    /// The project manager's check-in interval; zero (the default) turns them off.
    checkin: Duration,
    /// The stall watchdog's threshold; zero (the default) turns it off.
    stall: Duration,
    /// How long the project manager gets for a problem; zero (the default) alerts at once.
    grace: Duration,
    attempts: u32,
    retry: Duration,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            env: BTreeMap::new(),
            idle_grace: Duration::from_secs(60),
            backoff: Duration::from_millis(100),
            checkin: Duration::ZERO,
            stall: Duration::ZERO,
            grace: Duration::ZERO,
            attempts: 2,
            retry: Duration::from_millis(300),
        }
    }
}

struct Rig {
    hub: Arc<Hub>,
    runtime: Arc<AgentRuntime>,
    rx: broadcast::Receiver<Event>,
    _repo: tempfile::TempDir,
}

fn role(id: &str, tag: &str, owns: &str) -> Role {
    Role::parse(&format!(
        "---\nid: {id}\ntitle: {id}\ndefault_backend: fake\nmax_parallel_tasks: 1\n\
         tags: [{tag}]\nowns_states: [{owns}]\npermissions:\n  auto_approve: [read, edit]\n---\n\
         You are {{{{name}}}}.\n"
    ))
    .expect("valid role")
}

impl Rig {
    async fn new(opts: Opts) -> Self {
        let repo = tempfile::tempdir().expect("tempdir");
        git(repo.path(), &["init", "--quiet", "-b", "main"]);
        std::fs::write(repo.path().join("README.md"), "hi\n").expect("write");
        std::fs::write(
            repo.path().join(".gitignore"),
            ".chillax/state/\n.chillax/worktrees/\n",
        )
        .expect("write");
        git(repo.path(), &["add", "-A"]);
        git(repo.path(), &["commit", "--quiet", "-m", "initial"]);
        git(repo.path(), &["branch", "dev"]);
        git(repo.path(), &["branch", "qa"]);

        let roles = [
            role("pm", "manager", "backlog, ready"),
            role("dev", "developer", "in_progress"),
            role("lead", "reviewer", "in_review"),
        ];
        let engine = Engine::new("Acme", RoleCatalog::new(roles), Workflow::default());
        let hub = Arc::new(
            Hub::open(engine, Arc::new(MemoryLog::default()))
                .await
                .expect("hub"),
        );
        let rx = hub.subscribe();
        let runtime = AgentRuntime::new(
            Arc::clone(&hub),
            RuntimeConfig {
                company: "Acme".into(),
                cwd: repo.path().to_path_buf(),
                backends: shared_backends(BTreeMap::from([(
                    "fake".to_string(),
                    AgentConfig {
                        command: fake_agent().to_string_lossy().to_string(),
                        env: opts.env,
                        ..AgentConfig::default()
                    },
                )])),
                max_concurrent: 8,
                permissions: PermissionMode::Auto,
                ..RuntimeConfig::default()
            },
        );
        runtime.start();
        let mut config = DispatchConfig::new(repo.path(), "dev");
        config.max_concurrent = 8;
        config.poll = Duration::from_millis(50);
        config.cooldown = Duration::from_millis(100);
        config.backoff_base = opts.backoff;
        config.backoff_cap = opts.backoff * 8;
        config.idle_grace = opts.idle_grace;
        config.checkin = opts.checkin;
        config.stall = opts.stall;
        config.problem_grace = opts.grace;
        config.problem_max_attempts = opts.attempts;
        config.problem_retry = opts.retry;
        config.jitter = Arc::new(Quick);
        Dispatcher::new(
            Arc::clone(&hub),
            Arc::clone(&runtime),
            Arc::new(LocalGit::new(repo.path(), "dev")),
            config,
        )
        .start();
        let rig = Self {
            hub,
            runtime,
            rx,
            _repo: repo,
        };
        for (role, name) in [("pm", "Pam"), ("lead", "Lee"), ("dev", "Dana")] {
            rig.send(
                Actor::Stakeholder,
                Command::HireEmployee {
                    role: role.into(),
                    name: Some(name.into()),
                    backend: None,
                    model: None,
                    effort: None,
                    keep_defaults: false,
                },
            )
            .await;
        }
        rig
    }

    async fn send(&self, actor: Actor, cmd: Command) {
        self.hub.send(actor, cmd).await.expect("command accepted");
    }

    /// One approved story for Dana with `description` as her task.
    async fn story(&self, description: &str) {
        let pam = || Actor::Employee("pam".into());
        self.send(
            pam(),
            Command::CreateTicket {
                kind: TicketKind::Story,
                title: "Story".into(),
                description: description.into(),
                acceptance_criteria: vec!["works".into()],
                estimate: None,
                epic: None,
                external: None,
            },
        )
        .await;
        self.send(
            pam(),
            Command::ProposeSprint {
                name: "S1".into(),
                goal: String::new(),
                tickets: vec!["T-1".into()],
            },
        )
        .await;
        self.send(
            Actor::Stakeholder,
            Command::ApproveSprint { id: "SP-1".into() },
        )
        .await;
        self.send(
            pam(),
            Command::AssignTicket {
                id: "T-1".into(),
                assignee: Some("dana".into()),
            },
        )
        .await;
    }

    fn dana_has_session(&self) -> bool {
        self.runtime
            .ticket_sessions()
            .contains(&("dana".to_string(), "T-1".to_string()))
    }

    async fn until(&self, what: &str, mut cond: impl FnMut(&State) -> bool) {
        let waited = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if cond(&self.hub.snapshot().await) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await;
        assert!(waited.is_ok(), "timed out waiting until {what}");
    }

    /// The failures recorded so far.
    fn failures(&mut self) -> usize {
        let mut n = 0;
        while let Ok(event) = self.rx.try_recv() {
            if matches!(event.kind, EventKind::AgentFailed { .. }) {
                n += 1;
            }
        }
        n
    }
}

fn alert(state: &State, needle: &str) -> bool {
    state
        .inbox
        .iter()
        .any(|i| matches!(i, InboxItem::Alert { title, .. } if title.contains(needle)))
}

fn limit_file(text: &str) -> (tempfile::TempDir, BTreeMap<String, String>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("limit");
    std::fs::write(&file, text).expect("write");
    let env = BTreeMap::from([(
        "FAKE_RATE_LIMIT_FILE".to_string(),
        file.to_string_lossy().to_string(),
    )]);
    (dir, env)
}

#[tokio::test]
async fn the_developers_session_ends_when_her_tests_pass() {
    let rig = Rig::new(Opts::default()).await;
    rig.story("write file hello.txt").await;
    rig.until("Dana has a session", |_| rig.dana_has_session())
        .await;
    rig.send(
        Actor::Employee("dana".into()),
        Command::RequestReview {
            id: "T-1".into(),
            summary: "done".into(),
        },
    )
    .await;
    rig.until("the ticket is in review", |s| {
        s.ticket("T-1").is_some_and(|t| t.state == "in_review")
    })
    .await;
    rig.until("Dana's session is closed", |_| !rig.dana_has_session())
        .await;
}

#[tokio::test]
async fn a_turn_the_stakeholder_stopped_is_not_nudged_closed_or_restarted() {
    let mut rig = Rig::new(Opts {
        idle_grace: Duration::from_millis(100),
        ..Opts::default()
    })
    .await;
    rig.story("stall until cancel").await;
    rig.until("Dana is mid-turn", |s| {
        rig.dana_has_session()
            && s.employee("dana")
                .is_some_and(|e| e.status == chillax_core::EmployeeStatus::Working)
    })
    .await;
    rig.send(
        Actor::Stakeholder,
        Command::CancelTurn {
            employee: "dana".into(),
        },
    )
    .await;
    rig.until("the stop is recorded", |s| {
        s.employee("dana").is_some_and(|e| e.stopped)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(rig.dana_has_session(), "her session and context are kept");
    let state = rig.hub.snapshot().await;
    assert!(!alert(&state, "Dana stopped on T-1"));
    let says: Vec<&str> = state.chats["dana"]
        .iter()
        .map(|l| l.text.as_str())
        .collect();
    assert!(
        !says.iter().any(|t| t.contains("has not moved")),
        "{says:?}"
    );
    assert_eq!(rig.failures(), 0);
}

#[tokio::test]
async fn an_agent_that_goes_idle_twice_is_closed_with_an_alert_until_refreshed() {
    let rig = Rig::new(Opts {
        idle_grace: Duration::from_millis(100),
        ..Opts::default()
    })
    .await;
    // The scripted agent answers and stops; it never asks for a review.
    rig.story("nothing to write").await;
    rig.until("Dana has a session", |_| rig.dana_has_session())
        .await;
    rig.until("the stalled agent is reported", |s| {
        alert(s, "Dana stopped on T-1")
    })
    .await;
    rig.until("the session is closed", |_| !rig.dana_has_session())
        .await;
    // It is not reopened on its own.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(!rig.dana_has_session());
    // The nudge reached her: it is the second thing she was told.
    let state = rig.hub.snapshot().await;
    let says: Vec<&str> = state.chats["dana"]
        .iter()
        .map(|l| l.text.as_str())
        .collect();
    assert!(says.iter().any(|t| t.contains("has not moved")), "{says:?}");

    rig.send(Actor::Stakeholder, Command::Kick).await;
    rig.until("a refresh gives her a fresh session", |_| {
        rig.dana_has_session()
    })
    .await;
}

#[tokio::test]
async fn a_failing_session_backs_off_and_a_refresh_skips_the_wait() {
    let mut rig = Rig::new(Opts {
        backoff: Duration::from_secs(60),
        ..Opts::default()
    })
    .await;
    rig.story("explode").await;
    rig.until("the first failure", |s| {
        s.employee("dana")
            .is_some_and(|e| e.status == chillax_core::EmployeeStatus::Failed)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(rig.failures(), 1, "no retry inside the backoff");
    rig.send(Actor::Stakeholder, Command::Kick).await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(rig.failures(), 1, "the refresh retried once");
}

#[tokio::test]
async fn backoff_grows_between_retries() {
    let mut rig = Rig::new(Opts {
        backoff: Duration::from_millis(300),
        ..Opts::default()
    })
    .await;
    rig.story("explode").await;
    tokio::time::sleep(Duration::from_millis(2200)).await;
    // Waits of 0.3, 0.6, 1.2 s: three or four failures, not one per poll.
    let n = rig.failures();
    assert!((3..=5).contains(&n), "{n} failures");
}

#[tokio::test]
async fn a_signed_out_agent_raises_one_alert_and_stops_retrying() {
    let mut rig = Rig::new(Opts::default()).await;
    rig.story("sign me out").await;
    rig.until("the sign-in alert", |s| alert(s, "not signed in"))
        .await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(rig.failures(), 1);
    let state = rig.hub.snapshot().await;
    let alerts = state
        .inbox
        .iter()
        .filter(|i| matches!(i, InboxItem::Alert { .. }))
        .count();
    assert_eq!(alerts, 1);
    rig.send(Actor::Stakeholder, Command::Kick).await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(rig.failures(), 1, "signing in and refreshing tries again");
}

#[tokio::test]
async fn a_usage_limit_pauses_the_backend_until_a_probe_gets_through() {
    let (guard, env) = limit_file("You've hit your session limit");
    let mut rig = Rig::new(Opts {
        env,
        ..Opts::default()
    })
    .await;
    rig.story("nothing to write").await;
    rig.until("the backend is marked limited", |s| {
        s.backend_limits.contains_key("fake")
    })
    .await;
    rig.until("the stakeholder is told", |s| alert(s, "usage limit"))
        .await;
    // While limited, no session is retried; the probes keep failing quietly.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(!rig.dana_has_session());
    assert!(
        rig.failures() <= 2,
        "no retry storm while the backend is limited"
    );

    std::fs::remove_file(guard.path().join("limit")).expect("lift the limit");
    rig.until("a probe finds the backend back", |s| {
        s.backend_limits.is_empty()
    })
    .await;
    rig.until("the alert is cleared", |s| !alert(s, "usage limit"))
        .await;
    rig.until("Dana is back at work", |_| rig.dana_has_session())
        .await;
}

const INTERVAL: Duration = Duration::from_millis(1000);

fn checkins(state: &State) -> usize {
    state.chats.get("pam").map_or(0, |chat| {
        chat.iter()
            .filter(|l| l.text.contains("Board check-in"))
            .count()
    })
}

async fn checkin_rig() -> Rig {
    Rig::new(Opts {
        checkin: INTERVAL,
        ..Opts::default()
    })
    .await
}

#[tokio::test]
async fn the_project_manager_gets_a_board_check_in_after_the_interval_not_at_boot() {
    let rig = checkin_rig().await;
    rig.story("stall until cancel").await;
    // Dana's story is open from the first moment, but nothing goes out at boot.
    assert_eq!(checkins(&rig.hub.snapshot().await), 0);
    rig.until("the check-in lands", |s| checkins(s) == 1).await;
    let state = rig.hub.snapshot().await;
    let line = state.chats["pam"]
        .iter()
        .find(|l| l.text.contains("Board check-in"))
        .expect("check-in line");
    assert!(line.text.contains("Open: 1"), "{}", line.text);
    assert!(line.text.contains("Dana"), "{}", line.text);
}

#[tokio::test]
async fn an_unchanged_board_is_checked_again_only_after_three_quiet_intervals() {
    let rig = checkin_rig().await;
    rig.story("stall until cancel").await;
    rig.until("the first check-in", |s| checkins(s) == 1).await;
    // Let Pam's turn end, then watch the next two intervals pass without a second one.
    tokio::time::sleep(INTERVAL * 5 / 2).await;
    assert_eq!(checkins(&rig.hub.snapshot().await), 1);
    // A stuck board is unchanged by definition: it is checked anyway.
    rig.until("the forced check-in", |s| checkins(s) == 2).await;
}

#[tokio::test]
async fn a_board_that_moved_is_checked_at_the_next_interval() {
    let rig = checkin_rig().await;
    rig.story("stall until cancel").await;
    rig.until("the first check-in", |s| checkins(s) == 1).await;
    rig.send(
        Actor::Employee("pam".into()),
        Command::CreateTicket {
            kind: TicketKind::Story,
            title: "Another".into(),
            description: String::new(),
            acceptance_criteria: vec!["works".into()],
            estimate: None,
            epic: None,
            external: None,
        },
    )
    .await;
    let moved = std::time::Instant::now();
    rig.until("the second check-in", |s| checkins(s) == 2).await;
    assert!(
        moved.elapsed() < INTERVAL * 5 / 2,
        "sent because the board moved, not by the forced rule"
    );
}

#[tokio::test]
async fn a_stopped_project_manager_gets_no_check_in() {
    let rig = checkin_rig().await;
    rig.send(
        Actor::Stakeholder,
        Command::SendChat {
            to: "pam".into(),
            text: "stall until cancel".into(),
        },
    )
    .await;
    rig.until("Pam is mid-turn", |s| {
        s.employee("pam")
            .is_some_and(|e| e.status == chillax_core::EmployeeStatus::Working)
    })
    .await;
    rig.send(
        Actor::Stakeholder,
        Command::CancelTurn {
            employee: "pam".into(),
        },
    )
    .await;
    rig.until("the stop is recorded", |s| {
        s.employee("pam").is_some_and(|e| e.stopped)
    })
    .await;
    rig.story("stall until cancel").await;
    tokio::time::sleep(INTERVAL * 5 / 2).await;
    assert_eq!(checkins(&rig.hub.snapshot().await), 0);
}

fn briefings(state: &State) -> usize {
    state.chats.get("pam").map_or(0, |chat| {
        chat.iter()
            .filter(|l| l.text.contains("Problem briefing"))
            .count()
    })
}

fn alert_detail(state: &State, needle: &str) -> Option<String> {
    state.inbox.iter().find_map(|i| match i {
        InboxItem::Alert { title, detail, .. } if title.contains(needle) => Some(detail.clone()),
        _ => None,
    })
}

fn pam() -> Actor {
    Actor::Employee("pam".into())
}

/// A rig where Dana's agent crashes once and then stays down for a long while.
async fn crashing_rig(grace: Duration, attempts: u32, retry: Duration) -> Rig {
    let rig = Rig::new(Opts {
        backoff: Duration::from_secs(60),
        grace,
        attempts,
        retry,
        ..Opts::default()
    })
    .await;
    rig.story("explode").await;
    rig
}

#[tokio::test]
async fn a_crashed_teammate_goes_to_the_pm_who_resolves_it_without_alerting() {
    let rig = crashing_rig(Duration::from_secs(60), 2, Duration::from_secs(60)).await;
    rig.until("Pam is briefed", |s| briefings(s) == 1).await;
    let state = rig.hub.snapshot().await;
    let line = state.chats["pam"]
        .iter()
        .find(|l| l.text.contains("Problem briefing"))
        .expect("briefing");
    assert!(
        line.text.contains("P-agent-dana | agent-crashed | Dana"),
        "{}",
        line.text
    );
    assert!(line.text.contains("tried 1/2"), "{}", line.text);
    assert!(alert_detail(&state, "agent failed").is_none());
    // Pam restarts her, then reports it handled.
    rig.send(
        pam(),
        Command::RestartEmployee {
            employee: "dana".into(),
        },
    )
    .await;
    rig.until("Dana is no longer failed", |s| {
        s.employee("dana")
            .is_some_and(|e| e.status != chillax_core::EmployeeStatus::Failed)
    })
    .await;
    rig.send(
        pam(),
        Command::ResolveProblem {
            key: "agent-dana".into(),
            summary: "restarted Dana".into(),
        },
    )
    .await;
    rig.until("the stakeholder sees what was done", |s| {
        alert_detail(s, "handled agent-dana").is_some_and(|d| d == "PM handled: restarted Dana")
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let state = rig.hub.snapshot().await;
    assert!(alert_detail(&state, "agent failed").is_none());
    assert_eq!(briefings(&state), 1);
}

#[tokio::test]
async fn a_pm_who_never_resolves_is_briefed_at_most_twice_then_the_stakeholder_hears() {
    let rig = crashing_rig(Duration::from_secs(60), 2, Duration::from_millis(300)).await;
    rig.until("the stakeholder is told", |s| {
        alert_detail(s, "Dana's agent failed").is_some()
    })
    .await;
    let state = rig.hub.snapshot().await;
    let detail = alert_detail(&state, "Dana's agent failed").expect("alert");
    assert!(detail.contains("boom"), "{detail}");
    assert!(
        detail.ends_with("The project manager tried: no summary (2 attempts)."),
        "{detail}"
    );
    assert_eq!(briefings(&state), 2);
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert_eq!(briefings(&rig.hub.snapshot().await), 2, "no loop");
}

#[tokio::test]
async fn the_grace_window_ends_the_wait_even_with_attempts_left() {
    let rig = crashing_rig(Duration::from_millis(800), 5, Duration::from_secs(60)).await;
    rig.until("the stakeholder is told", |s| {
        alert_detail(s, "Dana's agent failed").is_some()
    })
    .await;
    let state = rig.hub.snapshot().await;
    assert_eq!(briefings(&state), 1);
    assert!(
        alert_detail(&state, "Dana's agent failed")
            .expect("alert")
            .contains("The project manager tried: no summary (1 attempts)."),
    );
}

#[tokio::test]
async fn a_stopped_pm_is_not_briefed_and_the_stakeholder_hears_at_once() {
    let rig = Rig::new(Opts {
        idle_grace: Duration::from_millis(100),
        grace: Duration::from_secs(60),
        ..Opts::default()
    })
    .await;
    rig.send(
        Actor::Stakeholder,
        Command::SendChat {
            to: "pam".into(),
            text: "stall until cancel".into(),
        },
    )
    .await;
    rig.until("Pam is mid-turn", |s| {
        s.employee("pam")
            .is_some_and(|e| e.status == chillax_core::EmployeeStatus::Working)
    })
    .await;
    rig.send(
        Actor::Stakeholder,
        Command::CancelTurn {
            employee: "pam".into(),
        },
    )
    .await;
    rig.until("the stop is recorded", |s| {
        s.employee("pam").is_some_and(|e| e.stopped)
    })
    .await;
    rig.story("nothing to write").await;
    // With no usable project manager the problem is not held back at all.
    rig.until("the stakeholder is told", |s| {
        alert(s, "Dana stopped on T-1")
    })
    .await;
    assert_eq!(briefings(&rig.hub.snapshot().await), 0);
}

#[tokio::test]
async fn a_sign_in_problem_alerts_at_once_and_is_never_deferred() {
    let rig = Rig::new(Opts {
        grace: Duration::from_secs(60),
        ..Opts::default()
    })
    .await;
    rig.story("sign me out").await;
    rig.until("the sign-in alert", |s| alert(s, "not signed in"))
        .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(briefings(&rig.hub.snapshot().await), 0);
}

#[tokio::test]
async fn with_the_grace_window_off_a_crash_is_not_deferred() {
    let rig = Rig::new(Opts {
        backoff: Duration::from_secs(60),
        ..Opts::default()
    })
    .await;
    rig.story("explode").await;
    rig.until("Dana failed", |s| {
        s.employee("dana")
            .is_some_and(|e| e.status == chillax_core::EmployeeStatus::Failed)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let state = rig.hub.snapshot().await;
    assert_eq!(briefings(&state), 0);
    assert!(alert_detail(&state, "agent failed").is_none());
}

#[tokio::test]
async fn a_stuck_agent_is_briefed_and_a_retry_gives_it_a_fresh_session() {
    let rig = Rig::new(Opts {
        idle_grace: Duration::from_millis(100),
        grace: Duration::from_secs(60),
        retry: Duration::from_secs(60),
        ..Opts::default()
    })
    .await;
    rig.story("nothing to write").await;
    rig.until("Pam is briefed about the stuck agent", |s| {
        briefings(s) == 1
    })
    .await;
    rig.until("the session is closed", |_| !rig.dana_has_session())
        .await;
    let state = rig.hub.snapshot().await;
    assert!(!alert(&state, "Dana stopped on T-1"));
    assert!(
        state.chats["pam"]
            .iter()
            .any(|l| l.text.contains("P-stuck-T-1 | agent-stuck"))
    );
    rig.send(
        pam(),
        Command::RetryAction {
            key: "stuck-T-1".into(),
        },
    )
    .await;
    rig.until("Dana gets a fresh session", |_| rig.dana_has_session())
        .await;
}

#[tokio::test]
async fn a_stalled_board_closes_the_hung_turn_briefs_the_pm_then_alerts_until_work_moves() {
    let rig = Rig::new(Opts {
        stall: Duration::from_millis(1000),
        ..Opts::default()
    })
    .await;
    // Dana's agent answers once and then hangs mid-turn.
    rig.story("stall until cancel").await;
    rig.until("Pam is briefed about the stall", |s| {
        s.chats
            .get("pam")
            .is_some_and(|chat| chat.iter().any(|l| l.text.contains("Stall check")))
    })
    .await;
    let state = rig.hub.snapshot().await;
    let briefing = state.chats["pam"]
        .iter()
        .find(|l| l.text.contains("Stall check"))
        .expect("briefing");
    assert!(
        briefing
            .text
            .contains("- T-1 \"Story\" in_progress, Dana: "),
        "{}",
        briefing.text
    );
    rig.until("Dana's hung turn is closed", |s| {
        s.ticket("T-1").is_some_and(|t| {
            t.comments
                .iter()
                .any(|c| c.body.contains("Chillax closed Dana's session"))
        })
    })
    .await;
    // Nothing moves after the briefing: the stakeholder is told, once.
    rig.until("the stakeholder is told", |s| alert(s, "Work has stalled"))
        .await;
    // Any movement ends the stall and clears the alert.
    rig.send(
        Actor::Stakeholder,
        Command::CommentTicket {
            id: "T-1".into(),
            body: "Any news?".into(),
            external_id: None,
            attachments: Vec::new(),
        },
    )
    .await;
    rig.until("the alert is cleared", |s| !alert(s, "Work has stalled"))
        .await;
}
