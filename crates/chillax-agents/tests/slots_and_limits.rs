//! Session slots, idle chats, permission modes and usage limits, against the scripted
//! `fake-acp-agent` (no model, no credits).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use chillax_agents::{AgentRuntime, RuntimeConfig, SessionKey, TicketSession, shared_backends};
use chillax_core::config::{AgentConfig, PermissionMode};
use chillax_core::dispatch::SessionKind;
use chillax_core::{
    Actor, Command, CompanyHandle, Engine, Event, EventKind, Hub, MemoryLog, Role, RoleCatalog,
    Speaker, Workflow,
};
use tokio::sync::broadcast;

struct Setup {
    max_concurrent: usize,
    live: Option<Arc<AtomicUsize>>,
    max_chat: usize,
    chat_idle: Duration,
    permissions: PermissionMode,
    env: BTreeMap<String, String>,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            max_concurrent: 2,
            live: None,
            max_chat: 2,
            chat_idle: Duration::from_secs(600),
            permissions: PermissionMode::Auto,
            env: BTreeMap::new(),
        }
    }
}

struct Team {
    hub: Arc<Hub>,
    runtime: Arc<AgentRuntime>,
    rx: broadcast::Receiver<Event>,
}

async fn team(setup: Setup) -> Team {
    let role = Role::parse(
        "---\nid: dev\ntitle: dev\ndefault_backend: fake\npermissions:\n  auto_approve: [read]\n---\n\
         You are {{name}}.\n",
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
                    env: setup.env,
                    ..AgentConfig::default()
                },
            )])),
            max_concurrent: setup.max_concurrent,
            max_concurrent_live: setup.live,
            max_chat: setup.max_chat,
            chat_idle: setup.chat_idle,
            sweep: Duration::from_millis(40),
            permissions: setup.permissions,
            ..RuntimeConfig::default()
        },
    );
    runtime.start();
    for name in ["Ann", "Bob", "Cy"] {
        hub.send(
            Actor::Stakeholder,
            Command::HireEmployee {
                role: "dev".into(),
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

fn ticket(employee: &str, id: &str, dir: &std::path::Path, briefing: &str) -> TicketSession {
    TicketSession {
        employee: employee.into(),
        ticket: id.into(),
        kind: SessionKind::Work,
        cwd: dir.to_path_buf(),
        scratch: None,
        briefing: briefing.into(),
    }
}

async fn chat(t: &Team, to: &str, text: &str) {
    t.hub
        .send(
            Actor::Stakeholder,
            Command::SendChat {
                to: to.into(),
                text: text.into(),
            },
        )
        .await
        .expect("sent");
}

fn said(state: &chillax_core::engine::State, employee: &str) -> String {
    state.chats[employee]
        .iter()
        .filter(|l| l.speaker == Speaker::Agent)
        .map(|l| l.text.as_str())
        .collect()
}

#[tokio::test]
async fn chats_do_not_take_ticket_slots() {
    let mut t = team(Setup::default()).await;
    let dir = tempfile::tempdir().expect("tempdir");
    chat(&t, "cy", "hello").await;
    wait_for(&mut t.rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;
    chat(&t, "bob", "hello").await;
    wait_for(&mut t.rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;
    assert_eq!(t.runtime.session_count(), 2);
    // Two chats are open and both ticket slots are still free.
    for (employee, id) in [("ann", "T-1"), ("bob", "T-2")] {
        t.runtime
            .open_ticket_session(ticket(employee, id, dir.path(), "go"))
            .await
            .expect("slot free");
    }
    assert_eq!(t.runtime.ticket_session_count(), 2);
    let full = t
        .runtime
        .open_ticket_session(ticket("cy", "T-3", dir.path(), "go"))
        .await;
    assert!(full.is_err_and(|e| e.contains("max_concurrent_agents")));
}

#[tokio::test]
async fn a_full_chat_cap_closes_the_least_recently_used_idle_chat() {
    let mut t = team(Setup::default()).await;
    for who in ["ann", "bob", "cy"] {
        chat(&t, who, "hello").await;
        wait_for(&mut t.rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;
        // Distinct last-activity times.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        t.runtime.session_count(),
        2,
        "Ann's chat made room for Cy's"
    );
    assert!(!t.runtime.send_to(&SessionKey::chat("ann"), "x".into()));
    assert!(t.runtime.send_to(&SessionKey::chat("cy"), "x".into()));
}

#[tokio::test]
async fn idle_chats_close_but_ticket_sessions_stay() {
    let mut t = team(Setup {
        chat_idle: Duration::from_millis(1500),
        ..Setup::default()
    })
    .await;
    let dir = tempfile::tempdir().expect("tempdir");
    t.runtime
        .open_ticket_session(ticket("bob", "T-1", dir.path(), "go"))
        .await
        .expect("opens");
    chat(&t, "ann", "hello").await;
    for _ in 0..2 {
        wait_for(&mut t.rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;
    }
    // The chat may have been swept while the turns were awaited, so poll instead of
    // waiting for an event that might already have gone by.
    tokio::time::timeout(Duration::from_secs(20), async {
        while t.runtime.session_count() != 1 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the idle chat was closed");
    assert_eq!(t.runtime.ticket_session_count(), 1);
}

#[tokio::test]
async fn refresh_closes_idle_sessions_of_every_kind() {
    let mut t = team(Setup::default()).await;
    let dir = tempfile::tempdir().expect("tempdir");
    t.runtime
        .open_ticket_session(ticket("bob", "T-1", dir.path(), "go"))
        .await
        .expect("opens");
    chat(&t, "ann", "hello").await;
    for _ in 0..2 {
        wait_for(&mut t.rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;
    }
    assert_eq!(t.runtime.close_idle().await.len(), 2);
    assert_eq!(t.runtime.session_count(), 0);
}

#[tokio::test]
async fn auto_permissions_approve_everything_and_enter_the_bypass_mode() {
    let mut t = team(Setup::default()).await;
    chat(&t, "ann", "edit at /etc/hosts and which mode").await;
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
        "{:?}",
        first.kind
    );
    wait_for(&mut t.rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;
    let state = t.hub.snapshot().await;
    let text = said(&state, "ann");
    assert!(text.contains("edit permission: allow"), "{text}");
    assert!(text.contains("(mode: bypassPermissions)"), "{text}");
    assert!(state.inbox.is_empty());
}

#[tokio::test]
async fn policy_permissions_keep_the_roles_rules_and_the_default_mode() {
    let mut t = team(Setup {
        permissions: PermissionMode::Policy,
        ..Setup::default()
    })
    .await;
    chat(&t, "ann", "edit at /etc/hosts and which mode").await;
    let first = wait_for(&mut t.rx, |k| {
        matches!(
            k,
            EventKind::PermissionResolved { .. } | EventKind::PermissionRequested { .. }
        )
    })
    .await;
    assert!(
        matches!(first.kind, EventKind::PermissionRequested { .. }),
        "{:?}",
        first.kind
    );
}

#[tokio::test]
async fn policy_sessions_stay_in_the_default_mode() {
    let mut t = team(Setup {
        permissions: PermissionMode::Policy,
        ..Setup::default()
    })
    .await;
    chat(&t, "ann", "which mode").await;
    wait_for(&mut t.rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;
    assert!(said(&t.hub.snapshot().await, "ann").contains("(mode: default)"));
}

fn limited() -> (tempfile::TempDir, Setup) {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("limit");
    std::fs::write(&file, "You've hit your session limit · resets in 2 hours").expect("write");
    let setup = Setup {
        env: BTreeMap::from([(
            "FAKE_RATE_LIMIT_FILE".to_string(),
            file.to_string_lossy().to_string(),
        )]),
        ..Setup::default()
    };
    (dir, setup)
}

#[tokio::test]
async fn a_usage_limit_fails_the_session_in_words_the_dispatcher_can_read() {
    let (_guard, setup) = limited();
    let mut t = team(setup).await;
    let dir = tempfile::tempdir().expect("tempdir");
    t.runtime
        .open_ticket_session(ticket("ann", "T-1", dir.path(), "go"))
        .await
        .expect("opens");
    let failed = wait_for(&mut t.rx, |k| matches!(k, EventKind::AgentFailed { .. })).await;
    let EventKind::AgentFailed { error, ticket, .. } = failed.kind else {
        unreachable!()
    };
    assert_eq!(ticket.as_deref(), Some("T-1"));
    assert!(error.contains("usage limit"), "{error}");
    assert!(chillax_core::limits::is_rate_limit(&error));
    assert!(
        chillax_core::limits::parse_reset(
            &error,
            chrono::Utc::now(),
            chrono::FixedOffset::east_opt(0).expect("utc")
        )
        .is_some(),
        "the reset time survives: {error}"
    );
    // A chat session fails without a ticket.
    chat(&t, "bob", "hi").await;
    let failed = wait_for(&mut t.rx, |k| matches!(k, EventKind::AgentFailed { .. })).await;
    assert!(matches!(
        failed.kind,
        EventKind::AgentFailed { ticket: None, .. }
    ));
}

#[tokio::test]
async fn a_probe_succeeds_or_fails_with_the_limit_and_leaves_no_trace() {
    let (guard, setup) = limited();
    let mut t = team(setup).await;
    let error = t.runtime.probe_backend("ann").await.expect_err("limited");
    assert!(chillax_core::limits::is_rate_limit(&error), "{error}");
    std::fs::remove_file(guard.path().join("limit")).expect("lift the limit");
    t.runtime.probe_backend("ann").await.expect("answers now");
    assert_eq!(t.runtime.session_count(), 0, "a probe holds no slot");
    // Drain: nothing the probe did reached the event log.
    while let Ok(event) = t.rx.try_recv() {
        assert!(
            !matches!(
                event.kind,
                EventKind::AgentStarted { .. }
                    | EventKind::AgentTurnEnded { .. }
                    | EventKind::AgentMessageChunk { .. }
                    | EventKind::AgentFailed { .. }
            ),
            "{:?}",
            event.kind
        );
    }
}

#[tokio::test]
async fn the_live_limit_is_read_on_every_open_and_lowering_it_closes_nothing() {
    let live = Arc::new(AtomicUsize::new(1));
    let t = team(Setup {
        live: Some(live.clone()),
        ..Setup::default()
    })
    .await;
    let dir = tempfile::tempdir().expect("tempdir");
    t.runtime
        .open_ticket_session(ticket("ann", "T-1", dir.path(), "go"))
        .await
        .expect("slot free");
    let full = t
        .runtime
        .open_ticket_session(ticket("bob", "T-2", dir.path(), "go"))
        .await;
    assert!(full.is_err_and(|e| e.contains("max_concurrent_agents (1)")));
    live.store(3, Ordering::Relaxed);
    t.runtime
        .open_ticket_session(ticket("bob", "T-2", dir.path(), "go"))
        .await
        .expect("raised limit has room");
    live.store(1, Ordering::Relaxed);
    assert_eq!(t.runtime.ticket_session_count(), 2);
}

#[tokio::test]
async fn a_chat_wrapped_up_after_work_closes_and_clears_the_transcript() {
    let mut t = team(Setup::default()).await;
    chat(&t, "ann", "hello").await;
    wait_for(&mut t.rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;
    assert!(t.hub.snapshot().await.chats.contains_key("ann"));
    t.runtime.start_fresh_next("ann");
    let closed = t.runtime.close_idle_chats(Duration::from_secs(600)).await;
    assert_eq!(closed, vec![SessionKey::chat("ann")]);
    assert_eq!(t.runtime.session_count(), 0);
    wait_for(&mut t.rx, |k| {
        matches!(k, EventKind::EmployeeContextCleared { .. })
    })
    .await;
    assert!(!t.hub.snapshot().await.chats.contains_key("ann"));
}

#[tokio::test]
async fn a_wrapped_up_chat_with_a_ticket_session_open_keeps_its_transcript() {
    let mut t = team(Setup::default()).await;
    let dir = tempfile::tempdir().expect("tempdir");
    chat(&t, "bob", "hello").await;
    wait_for(&mut t.rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;
    t.runtime
        .open_ticket_session(ticket("bob", "T-1", dir.path(), "go"))
        .await
        .expect("opens");
    wait_for(&mut t.rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;
    t.runtime.start_fresh_next("bob");
    t.runtime.close_idle_chats(Duration::from_secs(600)).await;
    assert_eq!(t.runtime.session_count(), 1);
    assert!(t.hub.snapshot().await.chats.contains_key("bob"));
}
