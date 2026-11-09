//! Stopping a turn (ADR 0027) against the scripted `fake-acp-agent` (no model, no credits).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use chillax_agents::{AgentRuntime, RuntimeConfig, TicketSession, shared_backends};
use chillax_core::config::{AgentConfig, PermissionMode};
use chillax_core::dispatch::SessionKind;
use chillax_core::{
    Actor, Command, CompanyHandle, Engine, Event, EventKind, Hub, MemoryLog, Role, RoleCatalog,
    Workflow,
};
use tokio::sync::broadcast;

struct Team {
    hub: Arc<Hub>,
    runtime: Arc<AgentRuntime>,
    rx: broadcast::Receiver<Event>,
}

async fn team(grace: Duration) -> Team {
    let role = Role::parse(
        "---\nid: dev\ntitle: dev\ndefault_backend: fake\npermissions:\n  auto_approve: [read, edit]\n---\nYou are {{name}}.\n",
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
                AgentConfig {
                    command: env!("CARGO_BIN_EXE_fake-acp-agent").into(),
                    ..AgentConfig::default()
                },
            )])),
            max_concurrent: 3,
            permissions: PermissionMode::Policy,
            stop_grace: grace,
            ..RuntimeConfig::default()
        },
    );
    runtime.start();
    hub.send(
        Actor::Stakeholder,
        Command::HireEmployee {
            role: "dev".into(),
            name: Some("Dev".into()),
            backend: None,
            model: None,
            effort: None,
            keep_defaults: false,
        },
    )
    .await
    .expect("hired");
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

/// Every event that arrives within `window`.
async fn drain(rx: &mut broadcast::Receiver<Event>, window: Duration) -> Vec<EventKind> {
    let mut out = Vec::new();
    while let Ok(Ok(event)) = tokio::time::timeout(window, rx.recv()).await {
        out.push(event.kind);
    }
    out
}

fn say(text: &str) -> Command {
    Command::SendChat {
        to: "dev".into(),
        text: text.into(),
    }
}

fn stop() -> Command {
    Command::CancelTurn {
        employee: "dev".into(),
    }
}

fn is_chunk(k: &EventKind) -> bool {
    matches!(k, EventKind::AgentMessageChunk { .. })
}

fn is_ended(k: &EventKind) -> bool {
    matches!(k, EventKind::AgentTurnEnded { .. })
}

#[tokio::test]
async fn stopping_a_turn_keeps_the_session_for_the_next_prompt() {
    let mut t = team(Duration::from_secs(10)).await;
    t.hub
        .send(Actor::Stakeholder, say("stall until cancel"))
        .await
        .expect("sent");
    wait_for(&mut t.rx, is_chunk).await;
    t.hub.send(Actor::Stakeholder, stop()).await.expect("stop");
    let ended = wait_for(&mut t.rx, is_ended).await;
    assert!(
        matches!(&ended.kind, EventKind::AgentTurnEnded { stop_reason, .. } if stop_reason == "cancelled"),
        "{ended:?}"
    );
    let after = drain(&mut t.rx, Duration::from_millis(400)).await;
    assert!(
        !after.iter().any(|k| matches!(
            k,
            EventKind::AgentFailed { .. } | EventKind::AgentStopped { .. }
        )),
        "{after:?}"
    );
    assert_eq!(t.runtime.session_count(), 1);
    let view = t
        .hub
        .snapshot()
        .await
        .employee("dev")
        .cloned()
        .expect("dev");
    assert!(view.stopped && !view.turn_open);

    // The same session takes the next prompt and lifts the hold.
    t.hub
        .send(Actor::Stakeholder, say("hello again"))
        .await
        .expect("sent");
    let ended = wait_for(&mut t.rx, is_ended).await;
    assert!(
        matches!(&ended.kind, EventKind::AgentTurnEnded { stop_reason, .. } if stop_reason == "end_turn"),
        "{ended:?}"
    );
    let view = t
        .hub
        .snapshot()
        .await
        .employee("dev")
        .cloned()
        .expect("dev");
    assert!(!view.stopped);
    assert_eq!(t.runtime.session_count(), 1);
}

#[tokio::test]
async fn stopping_an_idle_agent_does_nothing() {
    let t = team(Duration::from_secs(10)).await;
    let events = t.hub.send(Actor::Stakeholder, stop()).await.expect("ok");
    assert!(events.is_empty());
}

#[tokio::test]
async fn an_agent_that_ignores_the_stop_is_closed_not_failed() {
    let mut t = team(Duration::from_millis(300)).await;
    t.hub
        .send(Actor::Stakeholder, say("stall forever"))
        .await
        .expect("sent");
    wait_for(&mut t.rx, is_chunk).await;
    t.hub.send(Actor::Stakeholder, stop()).await.expect("stop");
    let after = drain(&mut t.rx, Duration::from_secs(4)).await;
    assert!(
        after
            .iter()
            .any(|k| matches!(k, EventKind::AgentStopped { .. })),
        "{after:?}"
    );
    assert!(
        !after
            .iter()
            .any(|k| matches!(k, EventKind::AgentFailed { .. })),
        "{after:?}"
    );
    assert_eq!(t.runtime.session_count(), 0);
}

#[tokio::test]
async fn a_message_after_a_stop_goes_back_into_the_stopped_ticket_session() {
    let mut t = team(Duration::from_secs(10)).await;
    let dir = tempfile::tempdir().expect("tempdir");
    t.runtime
        .open_ticket_session(TicketSession {
            employee: "dev".into(),
            ticket: "T-1".into(),
            kind: SessionKind::Work,
            cwd: dir.path().to_path_buf(),
            scratch: None,
            briefing: "stall until cancel".into(),
        })
        .await
        .expect("opens");
    wait_for(&mut t.rx, is_chunk).await;
    t.hub.send(Actor::Stakeholder, stop()).await.expect("stop");
    wait_for(&mut t.rx, is_ended).await;

    t.hub
        .send(Actor::Stakeholder, say("carry on"))
        .await
        .expect("sent");
    let ended = wait_for(&mut t.rx, is_ended).await;
    assert!(
        matches!(&ended.kind, EventKind::AgentTurnEnded { ticket: Some(t), stop_reason, .. } if t == "T-1" && stop_reason == "end_turn"),
        "{ended:?}"
    );
    assert_eq!(t.runtime.session_count(), 1, "no chat session was started");
}
