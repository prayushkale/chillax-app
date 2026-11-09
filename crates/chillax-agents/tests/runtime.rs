//! Drives the runtime against the scripted `fake-acp-agent`: no model, no credits.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use chillax_agents::{
    AcpAgentDirectory, AgentRuntime, McpEndpoint, RuntimeConfig, handshake, shared_backends,
};
use chillax_core::config::{AgentConfig, PermissionMode};
use chillax_core::{
    Actor, AgentDirectory, Command, CompanyHandle, Engine, Event, EventKind, Hub, MemoryLog, Role,
    RoleCatalog, Speaker, Workflow,
};
use tokio::sync::broadcast;

fn fake_backend(env: BTreeMap<String, String>) -> AgentConfig {
    AgentConfig {
        command: env!("CARGO_BIN_EXE_fake-acp-agent").into(),
        env,
        ..AgentConfig::default()
    }
}

async fn company(max_concurrent: usize) -> (Arc<Hub>, broadcast::Receiver<Event>) {
    let (hub, rx, _) = company_with(max_concurrent, BTreeMap::new()).await;
    (hub, rx)
}

async fn company_with(
    max_concurrent: usize,
    agent_env: BTreeMap<String, String>,
) -> (Arc<Hub>, broadcast::Receiver<Event>, Arc<AgentRuntime>) {
    let role = Role::parse(
        "---\nid: pm\ntitle: Project Manager\ndefault_backend: fake\nmcp_tools: [create_story]\npermissions:\n  auto_approve: [read]\n---\nYou are {{name}}.\n",
    )
    .expect("valid role");
    let engine = Engine::new("Acme", RoleCatalog::new([role]), Workflow::default());
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
                fake_backend(agent_env),
            )])),
            mcp: Some(McpEndpoint {
                url: "http://127.0.0.1:1/mcp".into(),
                issue_token: Arc::new(|employee| format!("token-{employee}")),
            }),
            max_concurrent,
            sweep: Duration::from_millis(100),
            permissions: PermissionMode::Policy,
            ..RuntimeConfig::default()
        },
    );
    runtime.start();
    hub.send(
        Actor::Stakeholder,
        Command::HireEmployee {
            role: "pm".into(),
            name: Some("Pat".into()),
            backend: None,
            model: None,
            effort: None,
            keep_defaults: false,
        },
    )
    .await
    .expect("hired");
    (hub, rx, runtime)
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

async fn chat(hub: &Hub, text: &str) {
    hub.send(
        Actor::Stakeholder,
        Command::SendChat {
            to: "pat".into(),
            text: text.into(),
        },
    )
    .await
    .expect("sent");
}

fn agent_lines(state: &chillax_core::engine::State) -> Vec<String> {
    state.chats["pat"]
        .iter()
        .filter(|l| matches!(l.speaker, Speaker::Agent | Speaker::Tool))
        .map(|l| l.text.clone())
        .collect()
}

#[tokio::test]
async fn chat_round_trip_with_tool_call() {
    let (hub, mut rx) = company(3).await;
    chat(&hub, "hello, please use a tool").await;
    wait_for(&mut rx, |k| matches!(k, EventKind::AgentStarted { agent: Some(a), .. } if a.starts_with("fake-acp-agent"))).await;
    wait_for(
        &mut rx,
        |k| matches!(k, EventKind::AgentTurnEnded { stop_reason, .. } if stop_reason == "end_turn"),
    )
    .await;

    let state = hub.snapshot().await;
    assert_eq!(
        agent_lines(&state),
        [
            "Read README.md [completed]",
            "echo: hello, please use a tool (mcp: yes)"
        ]
    );

    // Second message reuses the session (no second AgentStarted) and has no preamble.
    chat(&hub, "again").await;
    let ended = wait_for(&mut rx, |k| {
        matches!(
            k,
            EventKind::AgentTurnEnded { .. } | EventKind::AgentStarted { .. }
        )
    })
    .await;
    assert!(matches!(ended.kind, EventKind::AgentTurnEnded { .. }));
    assert_eq!(
        agent_lines(&hub.snapshot().await)
            .last()
            .map(String::as_str),
        Some("echo: again (mcp: yes)")
    );
}

#[tokio::test]
async fn escalated_permission_waits_for_the_stakeholder() {
    let (hub, mut rx) = company(3).await;
    chat(&hub, "ask permission").await;
    let requested = wait_for(&mut rx, |k| {
        matches!(k, EventKind::PermissionRequested { .. })
    })
    .await;
    let EventKind::PermissionRequested { id, title, .. } = requested.kind else {
        unreachable!("filtered above")
    };
    assert_eq!(title, "Run rm -rf build");
    assert_eq!(hub.snapshot().await.inbox.len(), 1);

    hub.send(
        Actor::Stakeholder,
        Command::ResolvePermission {
            id,
            choice: Some("reject".into()),
        },
    )
    .await
    .expect("resolved");
    wait_for(&mut rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;
    let lines = agent_lines(&hub.snapshot().await);
    assert_eq!(
        lines.last().map(String::as_str),
        Some("permission: reject. echo: ask permission (mcp: yes)")
    );
}

#[tokio::test]
async fn missing_backend_is_reported_as_failure() {
    let (hub, mut rx) = company(3).await;
    hub.send(
        Actor::Stakeholder,
        Command::HireEmployee {
            role: "pm".into(),
            name: Some("Quinn".into()),
            backend: Some("nope".into()),
            model: None,
            effort: None,
            keep_defaults: false,
        },
    )
    .await
    .expect("hired");
    hub.send(
        Actor::Stakeholder,
        Command::SendChat {
            to: "quinn".into(),
            text: "hi".into(),
        },
    )
    .await
    .expect("sent");
    let failed = wait_for(&mut rx, |k| matches!(k, EventKind::AgentFailed { .. })).await;
    assert!(
        matches!(failed.kind, EventKind::AgentFailed { error, .. } if error.contains("[agents.nope]"))
    );
}

#[tokio::test]
async fn letting_go_stops_the_session() {
    let (hub, mut rx) = company(3).await;
    chat(&hub, "hi").await;
    wait_for(&mut rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;
    hub.send(
        Actor::Stakeholder,
        Command::LetGo {
            employee: "pat".into(),
        },
    )
    .await
    .expect("let go");
    wait_for(&mut rx, |k| matches!(k, EventKind::AgentStopped { .. })).await;
}

#[tokio::test]
async fn changing_the_agent_ends_sessions_and_the_next_chat_starts_anew() {
    let (hub, mut rx) = company(3).await;
    chat(&hub, "hi").await;
    wait_for(&mut rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;
    hub.send(
        Actor::Stakeholder,
        Command::SetEmployeeAgent {
            employee: "pat".into(),
            when_idle: false,
            backend: None,
            model: Some("other-model".into()),
            effort: None,
        },
    )
    .await
    .expect("change agent");
    wait_for(&mut rx, |k| matches!(k, EventKind::AgentStopped { .. })).await;
    let pat = hub.snapshot().await.employees[0].employee.clone();
    assert_eq!(pat.model.as_deref(), Some("other-model"));
    chat(&hub, "again").await;
    wait_for(&mut rx, |k| matches!(k, EventKind::AgentStarted { .. })).await;
    wait_for(&mut rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;
}

#[tokio::test]
async fn an_agent_change_queued_mid_turn_applies_when_the_turn_ends() {
    let (hub, mut rx) = company(3).await;
    chat(&hub, "stall until cancel").await;
    wait_for(&mut rx, |k| {
        matches!(k, EventKind::AgentMessageChunk { .. })
    })
    .await;
    let change = |when_idle| Command::SetEmployeeAgent {
        employee: "pat".into(),
        when_idle,
        backend: None,
        model: Some("other-model".into()),
        effort: None,
    };
    assert!(hub.send(Actor::Stakeholder, change(false)).await.is_err());
    hub.send(Actor::Stakeholder, change(true))
        .await
        .expect("queued");
    let view = hub.snapshot().await.employees[0].clone();
    assert!(view.pending_agent.is_some());
    assert_eq!(view.employee.model, None, "not applied mid-turn");
    hub.send(
        Actor::Stakeholder,
        Command::CancelTurn {
            employee: "pat".into(),
        },
    )
    .await
    .expect("stop");
    wait_for(&mut rx, |k| {
        matches!(k, EventKind::EmployeeAgentChanged { .. })
    })
    .await;
    wait_for(&mut rx, |k| matches!(k, EventKind::AgentStopped { .. })).await;
    let view = hub.snapshot().await.employees[0].clone();
    assert_eq!(view.employee.model.as_deref(), Some("other-model"));
    assert!(view.pending_agent.is_none() && !view.stopped);
    chat(&hub, "again").await;
    wait_for(&mut rx, |k| matches!(k, EventKind::AgentStarted { .. })).await;
    wait_for(&mut rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;
}

#[tokio::test]
async fn chat_cleared_closes_session() {
    let (hub, mut rx) = company(3).await;
    chat(&hub, "hi").await;
    wait_for(&mut rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;
    hub.send(
        Actor::Stakeholder,
        Command::ClearChat {
            employee: "pat".into(),
        },
    )
    .await
    .expect("clear chat");
    wait_for(&mut rx, |k| matches!(k, EventKind::AgentStopped { .. })).await;
    assert!(
        hub.snapshot()
            .await
            .chats
            .get("pat")
            .is_none_or(|c| c.is_empty())
    );
}

#[tokio::test]
async fn employee_context_cleared_keeps_the_chat_session() {
    let (hub, mut rx) = company(3).await;
    chat(&hub, "hi").await;
    wait_for(&mut rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;
    hub.send(
        Actor::System,
        Command::ClearEmployeeContext {
            employee: "pat".into(),
            ticket: "T-1".into(),
        },
    )
    .await
    .expect("clear context");
    assert!(
        hub.snapshot()
            .await
            .chats
            .get("pat")
            .is_none_or(|c| c.is_empty())
    );
    // The session is still there: the next message gets a reply without a restart.
    chat(&hub, "again").await;
    wait_for(&mut rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;
    assert!(!hub.snapshot().await.chats["pat"].is_empty());
}

#[tokio::test]
async fn pm_chat_closed_after_sprint_proposed() {
    let (hub, mut rx) = company(3).await;
    chat(&hub, "plan please").await;
    wait_for(&mut rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;
    let pat = Actor::Employee("pat".into());
    hub.send(
        pat.clone(),
        Command::CreateTicket {
            kind: chillax_core::TicketKind::Story,
            title: "T".into(),
            description: String::new(),
            acceptance_criteria: vec!["works".into()],
            estimate: None,
            epic: None,
            external: None,
        },
    )
    .await
    .expect("ticket");
    hub.send(
        pat,
        Command::ProposeSprint {
            name: "S1".into(),
            goal: String::new(),
            tickets: vec!["T-1".into()],
        },
    )
    .await
    .expect("sprint proposed");
    wait_for(&mut rx, |k| matches!(k, EventKind::AgentStopped { .. })).await;
}

#[tokio::test]
async fn doctor_handshake_reports_agent_info() {
    let info = handshake(&fake_backend(BTreeMap::new()), Duration::from_secs(20))
        .await
        .expect("handshake succeeds");
    assert_eq!(info.name, "fake-acp-agent");
    assert!(info.mcp_http);

    let broken = AgentConfig {
        command: "/definitely/not/a/binary".into(),
        ..AgentConfig::default()
    };
    assert!(handshake(&broken, Duration::from_secs(5)).await.is_err());
}

#[tokio::test]
async fn sessions_run_on_the_employees_model() {
    let (hub, mut rx) = company(3).await;
    hub.send(
        Actor::Stakeholder,
        Command::HireEmployee {
            role: "pm".into(),
            name: Some("Mo".into()),
            backend: None,
            // Matched to the agent's `smart` option by alias.
            model: Some("acme-smart-2".into()),
            effort: None,
            keep_defaults: false,
        },
    )
    .await
    .expect("hired");
    hub.send(
        Actor::Stakeholder,
        Command::SendChat {
            to: "mo".into(),
            text: "which model are you?".into(),
        },
    )
    .await
    .expect("sent");
    let started = wait_for(&mut rx, |k| matches!(k, EventKind::AgentStarted { .. })).await;
    assert!(
        matches!(&started.kind, EventKind::AgentStarted { agent: Some(a), .. } if a.ends_with("model smart")),
        "{started:?}"
    );
    wait_for(&mut rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;
    let state = hub.snapshot().await;
    let reply = state.chats["mo"].last().expect("a reply").text.clone();
    assert!(reply.ends_with("(model: smart)"), "{reply}");
}

/// Hire `mo` on the fake agent with `effort`, ask which effort it runs, and return the
/// `AgentStarted` description and the reply.
async fn run_with_effort(effort: &str) -> (String, String) {
    let (hub, mut rx) = company(3).await;
    hub.send(
        Actor::Stakeholder,
        Command::HireEmployee {
            role: "pm".into(),
            name: Some("Mo".into()),
            backend: None,
            model: Some("smart".into()),
            effort: Some(effort.into()),
            keep_defaults: false,
        },
    )
    .await
    .expect("hired");
    hub.send(
        Actor::Stakeholder,
        Command::SendChat {
            to: "mo".into(),
            text: "which effort are you?".into(),
        },
    )
    .await
    .expect("sent");
    let started = wait_for(&mut rx, |k| matches!(k, EventKind::AgentStarted { .. })).await;
    let EventKind::AgentStarted { agent, .. } = started.kind else {
        unreachable!("filtered above")
    };
    wait_for(&mut rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;
    let state = hub.snapshot().await;
    let reply = state.chats["mo"].last().expect("a reply").text.clone();
    (agent.expect("agent info"), reply)
}

#[tokio::test]
async fn sessions_run_at_the_employees_effort() {
    let (agent, reply) = run_with_effort("High").await;
    assert!(agent.ends_with("model smart, effort high"), "{agent}");
    assert!(reply.ends_with("(effort: high)"), "{reply}");
}

#[tokio::test]
async fn an_unknown_effort_keeps_the_default_and_never_fails_the_session() {
    let (agent, reply) = run_with_effort("ludicrous").await;
    assert!(agent.ends_with("model smart"), "{agent}");
    assert!(reply.ends_with("(effort: medium)"), "{reply}");
}

#[tokio::test]
async fn signed_out_agents_say_how_to_sign_in() {
    let (hub, mut rx) = company(3).await;
    chat(&hub, "sign me out").await;
    let failed = wait_for(&mut rx, |k| matches!(k, EventKind::AgentFailed { .. })).await;
    let EventKind::AgentFailed { error, .. } = failed.kind else {
        unreachable!("filtered above")
    };
    assert!(error.contains("isn't signed in"), "{error}");
}

#[tokio::test]
async fn directory_lists_models_and_enables_presets() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let written = Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = Arc::clone(&written);
    let backends = shared_backends(BTreeMap::from([(
        "fake".to_string(),
        fake_backend(BTreeMap::new()),
    )]));
    let directory = AcpAgentDirectory::new(
        Arc::clone(&backends),
        scratch.path().to_path_buf(),
        Arc::new(move |name: &str, config: &AgentConfig| {
            log.lock()
                .expect("log")
                .push((name.to_string(), config.command.clone()));
            Ok(())
        }),
    );
    let listed = directory.backends().await;
    assert_eq!(listed[0].name, "fake");
    assert!(listed[0].configured && listed[0].available);

    let models = directory.models("fake").await.expect("lists models");
    let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["default", "fast", "smart"]);
    assert_eq!(std::fs::read_dir(scratch.path()).expect("ls").count(), 0);
    let efforts = directory.efforts("fake").await.expect("lists efforts");
    let ids: Vec<_> = efforts.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["low", "medium", "high"]);
    assert!(directory.models("nope").await.is_err());
    assert!(directory.efforts("nope").await.is_err());

    // Presets are written to the config and usable right away.
    let codex = directory.enable("codex").await.expect("enables");
    assert!(codex.configured);
    assert_eq!(
        *written.lock().expect("log"),
        [("codex".to_string(), "codex-acp".to_string())]
    );
    assert!(backends.read().expect("lock").contains_key("codex"));
    directory.enable("codex").await.expect("already configured");
    assert_eq!(written.lock().expect("log").len(), 1);
    assert!(directory.enable("unknown-agent").await.is_err());
}

/// Wait until the process in `pid_file` is gone (signal 0 fails).
async fn assert_process_gone(pid_file: &std::path::Path) {
    let pid = std::fs::read_to_string(pid_file)
        .expect("agent wrote its pid")
        .trim()
        .to_string();
    for _ in 0..40 {
        let alive = std::process::Command::new("kill")
            .args(["-0", &pid])
            .stderr(std::process::Stdio::null())
            .status()
            .expect("kill runs")
            .success();
        if !alive {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("agent process {pid} still alive after shutdown");
}

async fn stopped_events(hub: &Hub) -> usize {
    hub.events_since(0)
        .await
        .expect("reads")
        .iter()
        .filter(|e| matches!(e.kind, EventKind::AgentStopped { .. }))
        .count()
}

#[tokio::test]
async fn shutdown_all_stops_sessions_and_kills_the_agent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pid_file = dir.path().join("pid");
    let env = BTreeMap::from([("FAKE_PID_FILE".to_string(), pid_file.display().to_string())]);
    let (hub, mut rx, runtime) = company_with(3, env).await;
    chat(&hub, "hello").await;
    wait_for(&mut rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;
    assert_eq!(runtime.session_count(), 1);

    runtime.shutdown_all(Duration::from_secs(5)).await;

    assert_eq!(runtime.session_count(), 0);
    assert_eq!(stopped_events(&hub).await, 1);
    assert!(hub.snapshot().await.stale_agents().is_empty());
    assert_process_gone(&pid_file).await;
}

#[tokio::test]
async fn shutdown_all_kills_processes_the_agent_started() {
    let dir = tempfile::tempdir().expect("tempdir");
    let child_file = dir.path().join("child");
    let env = BTreeMap::from([(
        "FAKE_CHILD_PID_FILE".to_string(),
        child_file.display().to_string(),
    )]);
    let (hub, mut rx, runtime) = company_with(3, env).await;
    chat(&hub, "hello").await;
    wait_for(&mut rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;
    assert!(!chillax_agents::procgroup::registered().is_empty());

    runtime.shutdown_all(Duration::from_secs(5)).await;

    assert_process_gone(&child_file).await;
}

#[tokio::test]
async fn shutdown_all_mid_turn_still_records_one_stop() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pid_file = dir.path().join("pid");
    let env = BTreeMap::from([("FAKE_PID_FILE".to_string(), pid_file.display().to_string())]);
    let (hub, mut rx, runtime) = company_with(3, env).await;
    chat(&hub, "please stall forever").await;
    wait_for(&mut rx, |k| {
        matches!(k, EventKind::AgentMessageChunk { .. })
    })
    .await;
    assert!(hub.snapshot().await.employee("pat").expect("pat").turn_open);

    runtime.shutdown_all(Duration::from_secs(5)).await;

    assert_eq!(stopped_events(&hub).await, 1);
    let state = hub.snapshot().await;
    let pat = state.employee("pat").expect("pat");
    assert!(!pat.turn_open);
    assert_eq!(pat.status, chillax_core::model::EmployeeStatus::Offline);
    assert_process_gone(&pid_file).await;
}

#[tokio::test]
async fn no_session_starts_after_shutdown() {
    let (hub, mut rx, runtime) = company_with(3, BTreeMap::new()).await;
    runtime.shutdown_all(Duration::from_secs(5)).await;
    chat(&hub, "hello").await;
    // The chat is answered with a failure, never with a session.
    wait_for(&mut rx, |k| matches!(k, EventKind::AgentFailed { .. })).await;
    assert_eq!(runtime.session_count(), 0);
}

async fn hire_sam(hub: &Hub) {
    hub.send(
        Actor::Stakeholder,
        Command::HireEmployee {
            role: "pm".into(),
            name: Some("Sam".into()),
            backend: None,
            model: None,
            effort: None,
            keep_defaults: false,
        },
    )
    .await
    .expect("hired");
}

#[tokio::test]
async fn company_chat_to_all_waits_for_the_next_prompt_and_mentions_wake() {
    let (hub, mut rx) = company(3).await;
    hire_sam(&hub).await;
    hub.send(
        Actor::Stakeholder,
        Command::PostCompanyChat {
            text: "standup moved to 10".into(),
            to: None,
        },
    )
    .await
    .expect("posted");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        !hub.snapshot().await.employees.iter().any(|e| e.turn_open),
        "an @all post must not wake anyone"
    );

    chat(&hub, "show prompt").await;
    wait_for(
        &mut rx,
        |k| matches!(k, EventKind::AgentTurnEnded { employee, .. } if employee == "pat"),
    )
    .await;
    let state = hub.snapshot().await;
    let reply = agent_lines(&state).join("");
    assert!(
        reply.contains(
            "Company chat since your last prompt: | - Stakeholder to everyone: standup moved to 10"
        ),
        "{reply}"
    );

    hub.send(
        Actor::Stakeholder,
        Command::PostCompanyChat {
            text: "@sam are you there".into(),
            to: None,
        },
    )
    .await
    .expect("posted");
    wait_for(
        &mut rx,
        |k| matches!(k, EventKind::AgentTurnEnded { employee, .. } if employee == "sam"),
    )
    .await;
    let state = hub.snapshot().await;
    assert!(
        state.chats["sam"]
            .iter()
            .any(|l| l.speaker == Speaker::Agent)
    );
}
