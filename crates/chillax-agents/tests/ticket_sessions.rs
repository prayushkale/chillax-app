//! Dev-loop sessions: one per ticket, in the ticket's worktree, against the scripted
//! `fake-acp-agent` (no model, no credits).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use chillax_agents::{
    AgentRuntime, EnvFiles, McpEndpoint, RuntimeConfig, SessionKey, TicketSession, shared_backends,
};
use chillax_core::config::{AgentConfig, PermissionMode};
use chillax_core::dispatch::SessionKind;
use chillax_core::{
    Actor, Command, CompanyHandle, Engine, Event, EventKind, Hub, MemoryLog, Role, RoleCatalog,
    Speaker, ToolContent, Workflow,
};
use tokio::sync::broadcast;

struct Team {
    hub: Arc<Hub>,
    runtime: Arc<AgentRuntime>,
    rx: broadcast::Receiver<Event>,
}

async fn team() -> Team {
    team_with(EnvFiles::default()).await
}

async fn team_with(env_files: EnvFiles) -> Team {
    let roles = ["dev", "lead"].map(|id| {
        Role::parse(&format!(
            "---\nid: {id}\ntitle: {id}\ndefault_backend: fake\npermissions:\n  \
             auto_approve: [read, edit]\n---\nYou are {{{{name}}}}.\n"
        ))
        .expect("valid role")
    });
    let engine = Engine::new("Acme", RoleCatalog::new(roles), Workflow::default());
    let hub = Arc::new(
        Hub::open(engine, Arc::new(MemoryLog::default()))
            .await
            .expect("hub opens"),
    );
    let rx = hub.subscribe();
    let runtime = AgentRuntime::new(
        Arc::clone(&hub),
        RuntimeConfig {
            company: "Acme".into(),
            cwd: std::env::temp_dir(),
            backends: shared_backends(BTreeMap::from([(
                "fake".to_string(),
                AgentConfig {
                    command: env!("CARGO_BIN_EXE_fake-acp-agent").into(),
                    ..AgentConfig::default()
                },
            )])),
            mcp: Some(McpEndpoint {
                url: "http://127.0.0.1:1/mcp".into(),
                issue_token: Arc::new(|employee| format!("token-{employee}")),
            }),
            max_concurrent: 3,
            permissions: PermissionMode::Policy,
            env_files,
            ..RuntimeConfig::default()
        },
    );
    runtime.start();
    for (role, name) in [("dev", "Dev"), ("lead", "Lead")] {
        hub.send(
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
        .await
        .expect("hired");
    }
    Team { hub, runtime, rx }
}

async fn wait_for(
    rx: &mut broadcast::Receiver<Event>,
    mut pred: impl FnMut(&EventKind) -> bool,
) -> Event {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let event = rx.recv().await.expect("bus open");
            if pred(&event.kind) {
                return event;
            }
        }
    })
    .await
    .expect("event arrived in time")
}

fn session(
    employee: &str,
    kind: SessionKind,
    dir: &std::path::Path,
    briefing: &str,
) -> TicketSession {
    TicketSession {
        employee: employee.into(),
        ticket: "T-1".into(),
        kind,
        cwd: dir.to_path_buf(),
        scratch: None,
        briefing: briefing.into(),
    }
}

fn agent_text(state: &chillax_core::engine::State, employee: &str) -> String {
    state.chats[employee]
        .iter()
        .filter(|l| l.speaker == Speaker::Agent)
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("")
}

#[tokio::test]
async fn a_ticket_session_works_in_its_worktree_and_shows_plan_and_diff() {
    let mut t = team().await;
    let dir = tempfile::tempdir().expect("tempdir");
    t.runtime
        .open_ticket_session(session(
            "dev",
            SessionKind::Work,
            dir.path(),
            "write file hello.txt then make a plan and tell me where am i",
        ))
        .await
        .expect("opens");
    wait_for(&mut t.rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;

    assert_eq!(
        std::fs::read_to_string(dir.path().join("hello.txt")).expect("written in the worktree"),
        "hello.txt\n"
    );
    let state = t.hub.snapshot().await;
    assert!(
        agent_text(&state, "dev").contains(&format!("cwd: {}", dir.path().display())),
        "{}",
        agent_text(&state, "dev")
    );
    let live = &state.live["dev"];
    assert_eq!(live.plan.len(), 2);
    assert_eq!(live.plan[0].status, "completed");
    assert_eq!(live.tools[0].status, "completed");
    assert!(matches!(
        live.tools[0].content.first(),
        Some(ToolContent::Diff { path, new, .. })
            if path.ends_with("hello.txt") && new == "hello.txt\n"
    ));
    assert_eq!(
        t.runtime.ticket_sessions(),
        [("dev".to_string(), "T-1".to_string())].into()
    );
}

#[tokio::test]
async fn edits_outside_the_worktree_go_to_the_stakeholder() {
    let mut t = team().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let inside = dir.path().join("a.txt");
    t.runtime
        .open_ticket_session(session(
            "dev",
            SessionKind::Work,
            dir.path(),
            &format!("edit at {}", inside.display()),
        ))
        .await
        .expect("opens");
    let first = wait_for(&mut t.rx, |k| {
        matches!(
            k,
            EventKind::PermissionResolved { .. } | EventKind::PermissionRequested { .. }
        )
    })
    .await;
    assert!(
        matches!(
            first.kind,
            EventKind::PermissionResolved {
                automatic: true,
                ..
            }
        ),
        "inside the worktree the role policy decides: {:?}",
        first.kind
    );
    wait_for(&mut t.rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;

    // A second briefing for the same ticket joins the running session.
    t.runtime
        .open_ticket_session(session(
            "dev",
            SessionKind::Work,
            dir.path(),
            "edit at /etc/hosts",
        ))
        .await
        .expect("queued");
    let asked = wait_for(&mut t.rx, |k| {
        matches!(
            k,
            EventKind::PermissionResolved { .. } | EventKind::PermissionRequested { .. }
        )
    })
    .await;
    assert!(
        matches!(asked.kind, EventKind::PermissionRequested { .. }),
        "outside the worktree the stakeholder decides: {:?}",
        asked.kind
    );
}

#[tokio::test]
async fn reviewers_may_only_look() {
    let mut t = team().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let inside = dir.path().join("a.txt");
    t.runtime
        .open_ticket_session(session(
            "lead",
            SessionKind::Review,
            dir.path(),
            &format!("edit at {}", inside.display()),
        ))
        .await
        .expect("opens");
    let first = wait_for(&mut t.rx, |k| {
        matches!(
            k,
            EventKind::PermissionResolved { .. } | EventKind::PermissionRequested { .. }
        )
    })
    .await;
    assert!(
        matches!(first.kind, EventKind::PermissionRequested { .. }),
        "a review session never auto-approves an edit: {:?}",
        first.kind
    );
}

#[tokio::test]
async fn stakeholder_chat_gets_its_own_session_never_the_tickets() {
    let mut t = team().await;
    let dir = tempfile::tempdir().expect("tempdir");
    t.runtime
        .open_ticket_session(session("dev", SessionKind::Work, dir.path(), "go"))
        .await
        .expect("opens");
    wait_for(&mut t.rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;
    t.hub
        .send(
            Actor::Stakeholder,
            Command::SendChat {
                to: "dev".into(),
                text: "where am i".into(),
            },
        )
        .await
        .expect("sent");
    // A second session starts for the chat, in the repository root, not the worktree.
    wait_for(&mut t.rx, |k| matches!(k, EventKind::AgentStarted { .. })).await;
    wait_for(&mut t.rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;
    assert_eq!(t.runtime.session_count(), 2);
    assert_eq!(t.runtime.ticket_session_count(), 1);
    let text = agent_text(&t.hub.snapshot().await, "dev");
    assert!(
        text.contains(&format!("cwd: {}", std::env::temp_dir().display())),
        "{text}"
    );
    assert!(
        !text.contains(&format!("cwd: {}", dir.path().display())),
        "{text}"
    );
}

#[tokio::test]
async fn a_teammates_message_reaches_the_ticket_session() {
    let mut t = team().await;
    let dir = tempfile::tempdir().expect("tempdir");
    t.runtime
        .open_ticket_session(session("dev", SessionKind::Work, dir.path(), "go"))
        .await
        .expect("opens");
    wait_for(&mut t.rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;
    t.hub
        .send(
            Actor::Employee("lead".into()),
            Command::SendChat {
                to: "dev".into(),
                text: "where am i".into(),
            },
        )
        .await
        .expect("sent");
    wait_for(&mut t.rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;
    assert_eq!(t.runtime.session_count(), 1, "no second session starts");
    assert!(
        agent_text(&t.hub.snapshot().await, "dev")
            .contains(&format!("cwd: {}", dir.path().display()))
    );
}

#[tokio::test]
async fn closing_a_ticket_session_stops_the_agent() {
    let mut t = team().await;
    let dir = tempfile::tempdir().expect("tempdir");
    t.runtime
        .open_ticket_session(session("dev", SessionKind::Work, dir.path(), "go"))
        .await
        .expect("opens");
    wait_for(&mut t.rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;
    assert!(t.runtime.close(&SessionKey::ticket("dev", "T-1")).await);
    wait_for(&mut t.rx, |k| matches!(k, EventKind::AgentStopped { .. })).await;
    assert_eq!(t.runtime.session_count(), 0);
    assert!(!t.runtime.close(&SessionKey::ticket("dev", "T-1")).await);
    assert_eq!(
        t.hub.snapshot().await.employee("dev").map(|e| e.status),
        Some(chillax_core::EmployeeStatus::Offline)
    );
}

#[tokio::test]
async fn pausing_closes_ticket_sessions_but_not_chats() {
    let mut t = team().await;
    let dir = tempfile::tempdir().expect("tempdir");
    t.runtime
        .open_ticket_session(session("dev", SessionKind::Work, dir.path(), "go"))
        .await
        .expect("opens");
    t.hub
        .send(
            Actor::Stakeholder,
            Command::SendChat {
                to: "lead".into(),
                text: "hello".into(),
            },
        )
        .await
        .expect("sent");
    for _ in 0..2 {
        wait_for(&mut t.rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;
    }
    let closed = t.runtime.close_ticket_sessions().await;
    assert_eq!(closed, [SessionKey::ticket("dev", "T-1")]);
    assert_eq!(t.runtime.session_count(), 1, "the chat session stays");
}

#[tokio::test]
async fn the_concurrent_agent_limit_covers_ticket_sessions() {
    let t = team().await;
    let dir = tempfile::tempdir().expect("tempdir");
    for n in 1..=3 {
        t.runtime
            .open_ticket_session(TicketSession {
                ticket: format!("T-{n}"),
                ..session("dev", SessionKind::Work, dir.path(), "go")
            })
            .await
            .expect("opens");
    }
    let result = t
        .runtime
        .open_ticket_session(TicketSession {
            ticket: "T-4".into(),
            ..session("dev", SessionKind::Work, dir.path(), "go")
        })
        .await;
    assert!(result.is_err_and(|e| e.contains("max_concurrent_agents")));
}

fn dev_t(n: u32) -> (String, String) {
    ("dev".to_string(), format!("T-{n}"))
}

fn open_for(n: u32, dir: &std::path::Path, briefing: &str) -> TicketSession {
    TicketSession {
        ticket: format!("T-{n}"),
        ..session("dev", SessionKind::Work, dir, briefing)
    }
}

#[tokio::test]
async fn starting_a_new_ticket_closes_the_employees_idle_session_for_another() {
    let mut t = team().await;
    let dir = tempfile::tempdir().expect("tempdir");
    t.runtime
        .open_ticket_session(open_for(1, dir.path(), "go"))
        .await
        .expect("opens");
    wait_for(&mut t.rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;

    let closed = t
        .runtime
        .close_other_ticket_sessions("dev", "T-2", |_| false)
        .await;
    assert_eq!(closed, [SessionKey::ticket("dev", "T-1")]);
    wait_for(&mut t.rx, |k| matches!(k, EventKind::AgentStopped { .. })).await;
    t.runtime
        .open_ticket_session(open_for(2, dir.path(), "go"))
        .await
        .expect("opens");
    assert_eq!(t.runtime.ticket_sessions(), [dev_t(2)].into());
    let stopped = std::iter::from_fn(|| t.rx.try_recv().ok())
        .filter(|e| matches!(e.kind, EventKind::AgentStopped { .. }))
        .count();
    assert_eq!(stopped, 0, "no AgentStopped beyond the one waited for");
}

#[tokio::test]
async fn a_spared_ticket_keeps_its_session_for_parallel_work() {
    let t = team().await;
    let dir = tempfile::tempdir().expect("tempdir");
    for n in [1, 2] {
        t.runtime
            .open_ticket_session(open_for(n, dir.path(), "go"))
            .await
            .expect("opens");
    }
    let closed = t
        .runtime
        .close_other_ticket_sessions("dev", "T-3", |ticket| ticket == "T-1")
        .await;
    assert_eq!(closed, [SessionKey::ticket("dev", "T-2")]);
    assert_eq!(t.runtime.ticket_sessions(), [dev_t(1)].into());
}

#[tokio::test]
async fn a_session_in_the_middle_of_a_turn_is_never_cut() {
    let mut t = team().await;
    let dir = tempfile::tempdir().expect("tempdir");
    t.runtime
        .open_ticket_session(open_for(1, dir.path(), "stall forever"))
        .await
        .expect("opens");
    wait_for(&mut t.rx, |k| matches!(k, EventKind::AgentStarted { .. })).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let closed = t
        .runtime
        .close_other_ticket_sessions("dev", "T-2", |_| false)
        .await;
    assert!(closed.is_empty());
    assert_eq!(t.runtime.ticket_sessions(), [dev_t(1)].into());
}

#[tokio::test]
async fn reopening_the_same_ticket_reuses_its_session_and_chat_survives_the_guard() {
    let mut t = team().await;
    let dir = tempfile::tempdir().expect("tempdir");
    t.hub
        .send(
            Actor::Stakeholder,
            Command::SendChat {
                to: "dev".into(),
                text: "where am i".into(),
            },
        )
        .await
        .expect("sent");
    wait_for(&mut t.rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;
    for briefing in ["first", "second"] {
        t.runtime
            .open_ticket_session(open_for(1, dir.path(), briefing))
            .await
            .expect("opens");
        wait_for(&mut t.rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;
        let closed = t
            .runtime
            .close_other_ticket_sessions("dev", "T-1", |_| false)
            .await;
        assert!(closed.is_empty());
    }
    assert_eq!(t.runtime.session_count(), 2, "one chat and one ticket");
    let closed = t
        .runtime
        .close_other_ticket_sessions("dev", "T-2", |_| false)
        .await;
    assert_eq!(closed.len(), 1);
    assert_eq!(t.runtime.session_count(), 1, "the chat session stays");
}

#[tokio::test]
async fn a_qa_session_gets_the_qa_env_file_over_the_shared_one() {
    let root = tempfile::tempdir().expect("tempdir");
    let pid = |name: &str| root.path().join(name);
    std::fs::write(
        root.path().join(".env"),
        format!("FAKE_PID_FILE={}\n", pid("dev.pid").display()),
    )
    .expect("write");
    std::fs::write(
        root.path().join(".env.qa"),
        format!("FAKE_PID_FILE={}\n", pid("qa.pid").display()),
    )
    .expect("write");
    let env = chillax_core::config::EnvConfig::default();
    let t = team_with(EnvFiles::new(root.path(), &env)).await;
    let tree = tempfile::tempdir().expect("tempdir");
    t.runtime
        .open_ticket_session(session("dev", SessionKind::Qa, tree.path(), "hello"))
        .await
        .expect("session opens");
    tokio::time::timeout(Duration::from_secs(20), async {
        while !pid("qa.pid").exists() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the agent saw .env.qa");
    assert!(!pid("dev.pid").exists());
}

#[tokio::test]
async fn closing_after_the_turn_lets_a_running_turn_finish_without_a_cancel() {
    let mut t = team().await;
    let dir = tempfile::tempdir().expect("tempdir");
    t.runtime
        .open_ticket_session(session(
            "dev",
            SessionKind::Work,
            dir.path(),
            "finish slowly",
        ))
        .await
        .expect("opens");
    wait_for(&mut t.rx, |k| {
        matches!(k, EventKind::AgentMessageChunk { .. })
    })
    .await;
    let key = SessionKey::ticket("dev", "T-1");
    assert!(
        t.runtime
            .close_after_turn(&key, Duration::from_secs(10))
            .await
    );
    assert_eq!(t.runtime.session_count(), 0, "the slot is free");
    let ended = wait_for(&mut t.rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;
    assert!(
        matches!(&ended.kind, EventKind::AgentTurnEnded { stop_reason, .. } if stop_reason == "end_turn"),
        "{ended:?}"
    );
    wait_for(&mut t.rx, |k| matches!(k, EventKind::AgentStopped { .. })).await;
    assert!(
        !t.runtime
            .close_after_turn(&key, Duration::from_secs(1))
            .await
    );
}

#[tokio::test]
async fn closing_after_the_turn_cancels_once_the_grace_runs_out() {
    let mut t = team().await;
    let dir = tempfile::tempdir().expect("tempdir");
    t.runtime
        .open_ticket_session(session(
            "dev",
            SessionKind::Work,
            dir.path(),
            "stall until cancel",
        ))
        .await
        .expect("opens");
    wait_for(&mut t.rx, |k| {
        matches!(k, EventKind::AgentMessageChunk { .. })
    })
    .await;
    let key = SessionKey::ticket("dev", "T-1");
    assert!(
        t.runtime
            .close_after_turn(&key, Duration::from_millis(200))
            .await
    );
    let ended = wait_for(&mut t.rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;
    assert!(
        matches!(&ended.kind, EventKind::AgentTurnEnded { stop_reason, .. } if stop_reason == "cancelled"),
        "{ended:?}"
    );
}
