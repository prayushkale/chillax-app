//! Token and cost recording against the scripted `fake-acp-agent` (no model, no credits).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use chillax_agents::{AgentRuntime, RuntimeConfig, SessionKey, TicketSession, shared_backends};
use chillax_core::config::{AgentConfig, PermissionMode};
use chillax_core::dispatch::SessionKind;
use chillax_core::{
    Actor, Command, CompanyHandle, Engine, Event, EventKind, Hub, MemoryLog, ModelPrice,
    PriceTable, Role, RoleCatalog, Workflow,
};
use tokio::sync::broadcast;

struct Team {
    hub: Arc<Hub>,
    runtime: Arc<AgentRuntime>,
    rx: broadcast::Receiver<Event>,
}

async fn team(env: &[(&str, &str)], cumulative: bool, priced_model: Option<&str>) -> Team {
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
    let pricing = priced_model
        .map(|m| {
            PriceTable::new(&BTreeMap::from([(
                m.to_string(),
                ModelPrice {
                    input_micros_per_mtok: 1_000_000,
                    output_micros_per_mtok: 2_000_000,
                    cached_micros_per_mtok: 100_000,
                    cache_write_micros_per_mtok: 1_000_000,
                },
            )]))
        })
        .unwrap_or_default();
    let runtime = AgentRuntime::new(
        Arc::clone(&hub),
        RuntimeConfig {
            company: "Acme".into(),
            cwd: std::env::temp_dir(),
            backends: shared_backends(BTreeMap::from([(
                "fake".to_string(),
                AgentConfig {
                    command: env!("CARGO_BIN_EXE_fake-acp-agent").into(),
                    env: env
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                    tokens_cumulative: cumulative,
                    ..AgentConfig::default()
                },
            )])),
            max_concurrent: 3,
            permissions: PermissionMode::Policy,
            pricing: Arc::new(pricing),
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
            model: priced_model.map(str::to_string),
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

fn ticket_session(dir: &std::path::Path, briefing: &str) -> TicketSession {
    TicketSession {
        employee: "dev".into(),
        ticket: "T-1".into(),
        kind: SessionKind::Work,
        cwd: dir.to_path_buf(),
        scratch: None,
        briefing: briefing.into(),
    }
}

fn is_usage(k: &EventKind) -> bool {
    matches!(k, EventKind::UsageRecorded { .. })
}

fn comments(events: &[EventKind]) -> Vec<String> {
    events
        .iter()
        .filter_map(|k| match k {
            EventKind::TicketCommented { body, .. } => Some(body.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn reported_cumulative_cost_is_recorded_per_turn_and_commented_once_on_close() {
    let mut t = team(&[("FAKE_USAGE_TOKENS", "1000,200,300")], false, None).await;
    let dir = tempfile::tempdir().expect("tempdir");
    t.runtime
        .open_ticket_session(ticket_session(dir.path(), "report usage"))
        .await
        .expect("opens");
    let first = wait_for(&mut t.rx, is_usage).await;
    let EventKind::UsageRecorded {
        input_tokens,
        output_tokens,
        cached_tokens,
        cost_micros,
        estimated,
        ticket,
        context_size,
        ..
    } = first.kind
    else {
        unreachable!()
    };
    assert_eq!(
        (input_tokens, output_tokens, cached_tokens),
        (1000, 200, 300)
    );
    assert_eq!(cost_micros, Some(250_000));
    assert!(!estimated);
    assert_eq!(ticket.as_deref(), Some("T-1"));
    assert_eq!(context_size, Some(200_000));

    wait_for(&mut t.rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;
    assert!(t.runtime.send_to(
        &SessionKey::ticket("dev", "T-1"),
        "report usage again".into()
    ));
    let second = wait_for(&mut t.rx, is_usage).await;
    // The agent says 0.50 USD cumulative; the second sample carries only the difference.
    assert!(matches!(
        second.kind,
        EventKind::UsageRecorded {
            cost_micros: Some(250_000),
            ..
        }
    ));

    assert!(t.runtime.close(&SessionKey::ticket("dev", "T-1")).await);
    t.runtime.close(&SessionKey::ticket("dev", "T-1")).await;
    let after = drain(&mut t.rx, Duration::from_millis(500)).await;
    let comments = comments(&after);
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert!(
        comments[0].starts_with("Usage: 2.0k in / 400 out (600 cached)"),
        "{}",
        comments[0]
    );
    assert!(comments[0].contains("$0.50"), "{}", comments[0]);

    let state = t.hub.snapshot().await;
    assert_eq!(state.usage.by_ticket["T-1"].cost_micros, 500_000);
    assert_eq!(state.usage.total.turns, 2);
    assert!(
        state.usage.context.is_empty(),
        "stopped session clears context"
    );
}

#[tokio::test]
async fn a_backend_that_reports_nothing_records_and_comments_nothing() {
    let mut t = team(&[], false, None).await;
    let dir = tempfile::tempdir().expect("tempdir");
    t.runtime
        .open_ticket_session(ticket_session(dir.path(), "hello"))
        .await
        .expect("opens");
    wait_for(&mut t.rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;
    t.runtime.close(&SessionKey::ticket("dev", "T-1")).await;
    let after = drain(&mut t.rx, Duration::from_millis(500)).await;
    assert!(!after.iter().any(is_usage));
    assert!(comments(&after).is_empty());
    let state = t.hub.snapshot().await;
    assert_eq!(state.usage.by_employee["dev"].turns_total, 1);
    assert_eq!(state.usage.by_employee["dev"].sampled_turns, 0);
}

#[tokio::test]
async fn tokens_only_turns_are_estimated_from_the_price_table() {
    let mut t = team(
        &[("FAKE_USAGE_TOKENS", "1000000,500000,0")],
        false,
        Some("fast"),
    )
    .await;
    let dir = tempfile::tempdir().expect("tempdir");
    t.runtime
        .open_ticket_session(ticket_session(dir.path(), "hello"))
        .await
        .expect("opens");
    let event = wait_for(&mut t.rx, is_usage).await;
    let EventKind::UsageRecorded {
        cost_micros,
        estimated,
        model,
        ..
    } = event.kind
    else {
        unreachable!()
    };
    // 1M input at $1 plus 0.5M output at $2.
    assert_eq!(cost_micros, Some(2_000_000));
    assert!(estimated);
    assert_eq!(model.as_deref(), Some("fast"));
    t.runtime.close(&SessionKey::ticket("dev", "T-1")).await;
    let after = drain(&mut t.rx, Duration::from_millis(500)).await;
    let comments = comments(&after);
    assert!(comments[0].contains("~$2.00 (estimated)"), "{comments:?}");
}

#[tokio::test]
async fn unpriced_tokens_are_recorded_without_a_cost() {
    let mut t = team(&[("FAKE_USAGE_TOKENS", "10,5,0")], false, None).await;
    let dir = tempfile::tempdir().expect("tempdir");
    t.runtime
        .open_ticket_session(ticket_session(dir.path(), "hello"))
        .await
        .expect("opens");
    let event = wait_for(&mut t.rx, is_usage).await;
    assert!(matches!(
        event.kind,
        EventKind::UsageRecorded {
            cost_micros: None,
            input_tokens: 10,
            ..
        }
    ));
    t.runtime.close(&SessionKey::ticket("dev", "T-1")).await;
    let after = drain(&mut t.rx, Duration::from_millis(500)).await;
    assert!(comments(&after)[0].contains("cost unknown"));
    assert_eq!(t.hub.snapshot().await.usage.total.unpriced_samples, 1);
}

#[tokio::test]
async fn running_totals_are_turned_into_per_turn_tokens_when_configured() {
    let mut t = team(
        &[
            ("FAKE_USAGE_TOKENS", "100,10,0"),
            ("FAKE_USAGE_CUMULATIVE", "1"),
        ],
        true,
        None,
    )
    .await;
    let dir = tempfile::tempdir().expect("tempdir");
    t.runtime
        .open_ticket_session(ticket_session(dir.path(), "hello"))
        .await
        .expect("opens");
    wait_for(&mut t.rx, is_usage).await;
    wait_for(&mut t.rx, |k| matches!(k, EventKind::AgentTurnEnded { .. })).await;
    assert!(
        t.runtime
            .send_to(&SessionKey::ticket("dev", "T-1"), "again".into())
    );
    let second = wait_for(&mut t.rx, is_usage).await;
    // The agent now reports 200 in total; only the 100 new tokens count.
    assert!(matches!(
        second.kind,
        EventKind::UsageRecorded {
            input_tokens: 100,
            ..
        }
    ));
}

#[tokio::test]
async fn closing_a_session_mid_turn_still_records_the_turns_tokens() {
    let mut t = team(&[("FAKE_USAGE_TOKENS", "1000,200,300")], false, None).await;
    let dir = tempfile::tempdir().expect("tempdir");
    t.runtime
        .open_ticket_session(ticket_session(dir.path(), "stall until cancel"))
        .await
        .expect("opens");
    // The turn is running: the agent has said it is working and reported cost.
    wait_for(&mut t.rx, |k| {
        matches!(k, EventKind::AgentMessageChunk { .. })
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert!(t.runtime.close(&SessionKey::ticket("dev", "T-1")).await);
    let after = drain(&mut t.rx, Duration::from_millis(500)).await;
    let samples: Vec<_> = after
        .iter()
        .filter_map(|k| match k {
            EventKind::UsageRecorded {
                input_tokens,
                output_tokens,
                cached_tokens,
                cost_micros,
                ..
            } => Some((*input_tokens, *output_tokens, *cached_tokens, *cost_micros)),
            _ => None,
        })
        .collect();
    assert_eq!(samples, vec![(1000, 200, 300, Some(250_000))], "{after:?}");
    let stops = after
        .iter()
        .filter(|k| matches!(k, EventKind::AgentStopped { .. }))
        .count();
    assert_eq!(stops, 1, "stop announced once: {after:?}");
    assert_eq!(t.runtime.session_count(), 0);
}

#[tokio::test]
async fn flush_usage_records_cost_of_a_running_turn_once() {
    let mut t = team(&[("FAKE_USAGE_TOKENS", "10,2,0")], false, None).await;
    let dir = tempfile::tempdir().expect("tempdir");
    t.runtime
        .open_ticket_session(ticket_session(dir.path(), "stall until cancel"))
        .await
        .expect("opens");
    wait_for(&mut t.rx, |k| {
        matches!(k, EventKind::AgentMessageChunk { .. })
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    t.runtime.flush_usage().await;
    t.runtime.flush_usage().await;
    let flushed = drain(&mut t.rx, Duration::from_millis(300)).await;
    let costs: Vec<_> = flushed
        .iter()
        .filter_map(|k| match k {
            EventKind::UsageRecorded { cost_micros, .. } => Some(*cost_micros),
            _ => None,
        })
        .collect();
    assert_eq!(costs, vec![Some(250_000)], "second flush adds nothing");
    assert_eq!(t.runtime.session_count(), 1, "the session stays open");

    // Closing records the turn's tokens with no cost counted a second time.
    t.runtime.close(&SessionKey::ticket("dev", "T-1")).await;
    let state = t.hub.snapshot().await;
    assert_eq!(state.usage.total.cost_micros, 250_000);
    assert_eq!(state.usage.total.input_tokens, 10);
}

#[tokio::test]
async fn a_refresh_event_flushes_and_is_acknowledged() {
    let mut t = team(&[], false, None).await;
    t.hub
        .send(Actor::Stakeholder, Command::RefreshUsage)
        .await
        .expect("sent");
    wait_for(&mut t.rx, |k| matches!(k, EventKind::UsageRefreshed)).await;
}
