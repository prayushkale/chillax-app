//! Reported apps: what an employee says they started, kept apart from the
//! supervised apps.

use crate::apps::{AppInfo, AppSource, normalize_reported_url};
use crate::command::Command;
use crate::dev_loop_tests::{dev, ready_ticket, run, start, try_run};
use crate::engine::{Engine, State};
use crate::error::CommandError;
use crate::event::{Actor, EventKind};

fn report(url: Option<&str>, port: Option<u16>, env: Option<&str>) -> Command {
    Command::ReportApp {
        env: env.map(Into::into),
        url: url.map(Into::into),
        port,
        ticket: None,
        note: None,
        down: false,
    }
}

fn invalid(e: &mut Engine, who: Actor, cmd: Command) -> String {
    match try_run(e, who, cmd) {
        Err(CommandError::Invalid(m)) => m,
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn a_reported_app_appears_and_a_rereport_replaces_it() {
    let (mut e, _) = ready_ticket();
    run(
        &mut e,
        dev("dev"),
        report(Some("http://localhost:3001"), None, Some("dev")),
    );
    assert_eq!(e.state.reported_apps.len(), 1);
    assert_eq!(e.state.reported_apps[0].employee, "dev");
    run(
        &mut e,
        dev("dev"),
        report(Some("http://localhost:3002/x"), None, Some("DEV")),
    );
    assert_eq!(e.state.reported_apps.len(), 1);
    assert_eq!(e.state.reported_apps[0].url, "http://localhost:3002/x");
    assert_eq!(e.state.reported_apps[0].port, Some(3002));
    run(
        &mut e,
        dev("dev"),
        report(None, Some(4000), Some("preview")),
    );
    assert_eq!(e.state.reported_apps.len(), 2);
    assert_eq!(e.state.reported_apps[1].url, "http://localhost:4000");
}

#[test]
fn down_removes_and_is_quiet_when_nothing_matches() {
    let (mut e, _) = ready_ticket();
    let down = Command::ReportApp {
        env: Some("dev".into()),
        url: None,
        port: None,
        ticket: None,
        note: None,
        down: true,
    };
    assert!(run(&mut e, dev("dev"), down.clone()).is_empty());
    run(&mut e, dev("dev"), report(None, Some(3001), Some("dev")));
    let kinds = run(&mut e, dev("dev"), down);
    assert_eq!(kinds.len(), 1);
    assert!(e.state.reported_apps.is_empty());
}

#[test]
fn the_label_defaults_to_the_ticket_and_the_worktree_is_filled_in() {
    let (mut e, id) = ready_ticket();
    start(&mut e, &id);
    run(&mut e, dev("dev"), report(None, Some(3001), None));
    let app = &e.state.reported_apps[0];
    assert_eq!(app.label, id);
    assert_eq!(app.ticket.as_deref(), Some(id.as_str()));
    assert_eq!(
        app.worktree.as_deref(),
        Some(format!(".chillax/worktrees/{id}").as_str())
    );
}

#[test]
fn the_label_is_dev_without_a_ticket() {
    let (mut e, _) = ready_ticket();
    run(&mut e, dev("dev"), report(None, Some(3001), None));
    assert_eq!(e.state.reported_apps[0].label, "dev");
}

#[test]
fn reported_apps_survive_a_stopped_session_and_a_let_go_until_reported_down() {
    let (mut e, _) = ready_ticket();
    run(&mut e, dev("dev"), report(None, Some(3001), Some("dev")));
    run(&mut e, dev("lead"), report(None, Some(3002), Some("dev")));
    let stop = |e: &mut Engine, who: &str| {
        let event = crate::event::Event {
            seq: e.state.last_seq + 1,
            ts: chrono::DateTime::from_timestamp(0, 0).expect("epoch"),
            actor: Actor::System,
            kind: EventKind::AgentStopped {
                employee: who.into(),
            },
        };
        e.apply(&event);
    };
    stop(&mut e, "dev");
    assert_eq!(e.state.reported_apps.len(), 2);
    run(
        &mut e,
        Actor::Stakeholder,
        Command::LetGo {
            employee: "lead".into(),
        },
    );
    assert_eq!(e.state.reported_apps.len(), 2);
    let down = EventKind::AppReported {
        env: "dev".into(),
        url: "http://localhost:3002".into(),
        port: Some(3002),
        ticket: None,
        employee: "lead".into(),
        worktree: None,
        note: None,
        down: true,
    };
    let event = crate::event::Event {
        seq: e.state.last_seq + 1,
        ts: chrono::DateTime::from_timestamp(0, 0).expect("epoch"),
        actor: Actor::System,
        kind: down,
    };
    e.apply(&event);
    assert_eq!(e.state.reported_apps.len(), 1);
    assert_eq!(e.state.reported_apps[0].employee, "dev");
}

#[test]
fn the_system_can_report_a_detected_app() {
    let (mut e, _) = ready_ticket();
    for p in 0..7u16 {
        run(
            &mut e,
            Actor::System,
            report(None, Some(4000 + p), Some(&format!("_qa :{}", 4000 + p))),
        );
    }
    assert_eq!(e.state.reported_apps.len(), 7);
    assert_eq!(e.state.reported_apps[0].employee, "chillax");
}

#[test]
fn running_apps_merges_supervised_and_reported_rows() {
    let (mut e, _) = ready_ticket();
    run(
        &mut e,
        Actor::System,
        Command::ConfigureApps {
            apps: vec![
                AppInfo {
                    env: "dev".into(),
                    port: 3000,
                    url: "http://localhost:3000".into(),
                },
                AppInfo {
                    env: "qa".into(),
                    port: 3001,
                    url: "http://localhost:3001".into(),
                },
            ],
        },
    );
    run(
        &mut e,
        dev("dev"),
        report(None, Some(3100), Some("preview")),
    );
    run(&mut e, dev("dev"), report(None, Some(3200), Some("dev")));
    let rows = e.state.running_apps();
    let shape: Vec<_> = rows
        .iter()
        .map(|r| (r.label.as_str(), r.source.clone()))
        .collect();
    assert_eq!(
        shape,
        [
            ("dev", AppSource::Supervised),
            ("dev", AppSource::Reported("dev".into())),
            ("qa", AppSource::Supervised),
            ("preview", AppSource::Reported("dev".into())),
        ]
    );
    // Reported apps never feed the supervisor's view of the world.
    assert_eq!(e.state.apps.len(), 2);
}

#[test]
fn only_local_http_urls_are_accepted() {
    for ok in [
        "http://localhost:3000",
        "https://LOCALHOST/x?y=1",
        "http://127.0.0.1:8080",
        "http://127.1.2.3",
        "http://[::1]:3000/",
        "http://app.localhost:3000",
    ] {
        assert!(normalize_reported_url(ok).is_ok(), "{ok}");
    }
    for bad in [
        "http://evil.com",
        "https://localhost.evil.com",
        "http://localhost@evil.com",
        "http://user:pw@localhost",
        "javascript:alert(1)",
        "file:///etc/passwd",
        "http://127.0.0.1.evil.com",
        "http://localhost:0",
        "http://localhost:99999",
        "http://localhost:abc",
        "http://localhost\n:3000",
        "http://local host",
        "http://localhost\\@evil.com",
        "//localhost:3000",
        "",
    ] {
        assert!(normalize_reported_url(bad).is_err(), "{bad}");
    }
    assert_eq!(
        normalize_reported_url("HTTP://LocalHost:3000/Path").expect("ok"),
        ("http://localhost:3000/Path".to_string(), Some(3000))
    );
}

#[test]
fn bad_reports_are_rejected() {
    let (mut e, _) = ready_ticket();
    assert!(
        invalid(
            &mut e,
            dev("dev"),
            report(Some("http://evil.com"), None, None)
        )
        .contains("this machine")
    );
    invalid(&mut e, dev("dev"), report(None, None, None));
    invalid(
        &mut e,
        dev("dev"),
        report(Some("http://localhost:1"), Some(1), None),
    );
    invalid(&mut e, dev("dev"), report(None, Some(0), None));
    invalid(&mut e, dev("dev"), report(None, Some(80), Some("a\nb")));
    invalid(
        &mut e,
        dev("dev"),
        report(None, Some(80), Some(&"x".repeat(41))),
    );
    let long_note = Command::ReportApp {
        env: None,
        url: None,
        port: Some(80),
        ticket: None,
        note: Some("n".repeat(201)),
        down: false,
    };
    invalid(&mut e, dev("dev"), long_note);
    assert!(matches!(
        try_run(&mut e, Actor::Stakeholder, report(None, Some(80), None)),
        Err(CommandError::EmployeeOnly(_))
    ));
    assert!(matches!(
        try_run(&mut e, dev("ghost"), report(None, Some(80), None)),
        Err(CommandError::UnknownEmployee(_))
    ));
    assert!(e.state.reported_apps.is_empty());
}

#[test]
fn an_employee_keeps_at_most_five_reported_apps() {
    let (mut e, _) = ready_ticket();
    for i in 0..5 {
        run(
            &mut e,
            dev("dev"),
            report(None, Some(3000 + i), Some(&format!("app{i}"))),
        );
    }
    invalid(&mut e, dev("dev"), report(None, Some(3100), Some("app5")));
    // Replacing one of the five is fine, and another employee has their own five.
    run(&mut e, dev("dev"), report(None, Some(3101), Some("app0")));
    run(&mut e, dev("lead"), report(None, Some(3102), Some("app5")));
    assert_eq!(e.state.reported_apps.len(), 6);
}

#[test]
fn old_logs_and_state_without_reported_apps_still_read() {
    let (e, _) = ready_ticket();
    let json = serde_json::to_string(&e.state).expect("serializes");
    assert!(!json.contains("reported_apps"));
    let state: State = serde_json::from_str(&json).expect("a state without the field reads");
    assert!(state.reported_apps.is_empty());
    let kind = EventKind::AppReported {
        env: "dev".into(),
        url: "http://localhost:3000".into(),
        port: Some(3000),
        ticket: None,
        employee: "pat".into(),
        worktree: None,
        note: None,
        down: false,
    };
    let json = serde_json::to_string(&kind).expect("serializes");
    assert!(json.contains(r#""type":"app_reported""#));
    assert_eq!(
        serde_json::from_str::<EventKind>(&json).expect("reads"),
        kind
    );
    assert_eq!(kind.name(), "app_reported");
    assert!(
        kind.summary()
            .contains("pat reports dev at http://localhost:3000")
    );
}

#[test]
fn report_app_round_trips_as_a_command() {
    let cmd = Command::ReportApp {
        env: Some("dev".into()),
        url: Some("http://localhost:3000".into()),
        port: None,
        ticket: Some("T-1".into()),
        note: Some("login page".into()),
        down: false,
    };
    let json = serde_json::to_string(&cmd).expect("serializes");
    assert_eq!(serde_json::from_str::<Command>(&json).expect("reads"), cmd);
    let bare: Command =
        serde_json::from_str(r#"{"type":"report_app","port":3000}"#).expect("reads");
    assert_eq!(bare, report(None, Some(3000), None));
}
