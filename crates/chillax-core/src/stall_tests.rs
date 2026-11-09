//! The stall watchdog: when it acts, what it closes, who it briefs, and why
//! each open ticket is idle.

use std::collections::BTreeSet;
use std::time::Duration;

use crate::command::Command;
use crate::dev_loop_tests::{dev, ready_ticket, run, start};
use crate::dispatch::{Action, Context, KEY_STALL, hung_key, next_actions, why_idle};
use crate::engine::Engine;
use crate::event::{Actor, Event, EventKind};
use crate::stall::{self, BusySession, StallCtx};

const WINDOW: Duration = Duration::from_secs(20 * 60);

fn apply(e: &mut Engine, kind: EventKind) {
    let event = Event {
        seq: e.state.last_seq + 1,
        ts: chrono::DateTime::from_timestamp(0, 0).expect("epoch is valid"),
        actor: Actor::System,
        kind,
    };
    e.apply(&event);
}

fn ctx(stall: StallCtx) -> Context {
    Context {
        max_concurrent: 3,
        stall: Some(stall),
        ..Context::default()
    }
}

fn idle(for_: Duration) -> StallCtx {
    StallCtx {
        threshold: WINDOW,
        idle_for: for_,
        ..StallCtx::default()
    }
}

fn stall_actions(e: &Engine, ctx: &Context) -> Vec<Action> {
    next_actions(e, ctx)
        .into_iter()
        .filter(|a| {
            matches!(
                a,
                Action::PmUnstick { .. } | Action::StallAlert | Action::CloseHungSession { .. }
            )
        })
        .collect()
}

/// Two approved tickets, both assigned to Dev.
fn two_ready() -> (Engine, String, String) {
    let mut e = crate::dev_loop_tests::engine();
    crate::dev_loop_tests::hire(&mut e, "project-manager", "Pam");
    crate::dev_loop_tests::hire(&mut e, "fullstack-developer", "Dev");
    let a = crate::dev_loop_tests::story(&mut e, "Login");
    let b = crate::dev_loop_tests::story(&mut e, "Profile");
    run(
        &mut e,
        dev("pam"),
        Command::ProposeSprint {
            name: "S1".into(),
            goal: String::new(),
            tickets: vec![a.clone(), b.clone()],
        },
    );
    run(
        &mut e,
        Actor::Stakeholder,
        Command::ApproveSprint { id: "SP-1".into() },
    );
    for id in [&a, &b] {
        run(
            &mut e,
            dev("pam"),
            Command::AssignTicket {
                id: id.clone(),
                assignee: Some("dev".into()),
            },
        );
    }
    (e, a, b)
}

fn unstick() -> Vec<Action> {
    vec![Action::PmUnstick { pm: "pam".into() }]
}

/// A ticket in progress whose developer's session is open (so nothing else would act).
fn in_progress() -> (Engine, String, BTreeSet<(String, String)>) {
    let (mut e, id) = ready_ticket();
    start(&mut e, &id);
    let sessions = BTreeSet::from([("dev".to_string(), id.clone())]);
    (e, id, sessions)
}

#[test]
fn nothing_happens_before_the_board_has_been_still_for_the_threshold() {
    let (e, _, sessions) = in_progress();
    let mut c = ctx(idle(WINDOW - Duration::from_secs(1)));
    c.ticket_sessions = sessions;
    assert!(stall_actions(&e, &c).is_empty());
}

#[test]
fn a_stalled_board_is_handed_to_the_project_manager() {
    let (e, _, sessions) = in_progress();
    let mut c = ctx(idle(WINDOW));
    c.ticket_sessions = sessions;
    assert_eq!(stall_actions(&e, &c), unstick());
    // One briefing at a time.
    c.busy.insert(KEY_STALL.into());
    assert!(stall_actions(&e, &c).is_empty());
}

#[test]
fn the_watchdog_is_quiet_when_paused_off_or_without_active_sprint_work() {
    let (mut e, _, sessions) = in_progress();
    let mut c = ctx(idle(WINDOW * 3));
    c.ticket_sessions = sessions.clone();
    // Off.
    let off = Context {
        stall: None,
        ..c.clone()
    };
    assert!(stall_actions(&e, &off).is_empty());
    // A zero threshold is off too.
    let zero = ctx(StallCtx {
        threshold: Duration::ZERO,
        ..idle(WINDOW)
    });
    assert!(stall_actions(&e, &zero).is_empty());
    // Paused.
    run(&mut e, Actor::Stakeholder, Command::PauseDispatch);
    assert!(stall_actions(&e, &c).is_empty());

    // Only backlog work, or a sprint still at the gate: nothing to keep moving.
    let mut e = crate::dev_loop_tests::engine();
    crate::dev_loop_tests::hire(&mut e, "project-manager", "Pam");
    let id = crate::dev_loop_tests::story(&mut e, "Login");
    assert!(!stall::watching(&e));
    assert!(stall_actions(&e, &c).is_empty());
    run(
        &mut e,
        dev("pam"),
        Command::ProposeSprint {
            name: "S1".into(),
            goal: String::new(),
            tickets: vec![id],
        },
    );
    assert!(stall_actions(&e, &c).is_empty());
}

#[test]
fn a_hung_turn_is_closed_before_anyone_is_briefed() {
    let (e, id, sessions) = in_progress();
    let busy = |employee: &str, ticket: Option<&str>, quiet| BusySession {
        employee: employee.into(),
        ticket: ticket.map(Into::into),
        quiet,
    };
    let mut c = ctx(StallCtx {
        busy_sessions: vec![
            busy("dev", Some(&id), WINDOW),
            busy("pam", None, WINDOW + Duration::from_secs(60)),
            // Busy but recently active: left alone.
            busy("lead", None, Duration::from_secs(30)),
        ],
        ..idle(WINDOW)
    });
    c.ticket_sessions = sessions;
    assert_eq!(
        stall_actions(&e, &c),
        vec![
            Action::CloseHungSession {
                key: hung_key("dev", Some(&id)),
                employee: "dev".into(),
                ticket: Some(id.clone()),
            },
            Action::CloseHungSession {
                key: hung_key("pam", None),
                employee: "pam".into(),
                ticket: None,
            },
        ]
    );
    // While the closes run, the project manager is not briefed yet.
    c.stall.as_mut().expect("stall").busy_sessions.clear();
    c.busy.insert(hung_key("pam", None));
    assert!(stall_actions(&e, &c).is_empty());
    c.busy.clear();
    assert_eq!(stall_actions(&e, &c), unstick());
}

#[test]
fn a_turn_waiting_on_the_stakeholder_is_not_hung() {
    let (mut e, id, sessions) = in_progress();
    run(
        &mut e,
        dev("dev"),
        Command::AskStakeholder {
            question: "Which colour?".into(),
        },
    );
    let mut c = ctx(StallCtx {
        busy_sessions: vec![BusySession {
            employee: "dev".into(),
            ticket: Some(id.clone()),
            quiet: WINDOW * 2,
        }],
        ..idle(WINDOW)
    });
    c.ticket_sessions = sessions;
    assert_eq!(stall_actions(&e, &c), unstick());
    assert!(why_idle(&e, &c, e.state.ticket(&id).expect("ticket")).contains("stakeholder"));
}

#[test]
fn one_briefing_per_window_then_one_alert_then_quiet() {
    let (e, _, sessions) = in_progress();
    let mut c = ctx(StallCtx {
        briefed_for: Some(WINDOW - Duration::from_secs(1)),
        ..idle(WINDOW * 2)
    });
    c.ticket_sessions = sessions;
    assert!(stall_actions(&e, &c).is_empty(), "the PM is on it");
    c.stall.as_mut().expect("stall").briefed_for = Some(WINDOW);
    assert_eq!(stall_actions(&e, &c), vec![Action::StallAlert]);
    c.stall.as_mut().expect("stall").alerted = true;
    c.stall.as_mut().expect("stall").briefed_for = Some(WINDOW * 5);
    assert!(stall_actions(&e, &c).is_empty(), "told once, then quiet");
}

#[test]
fn without_a_free_project_manager_the_stakeholder_is_told_after_two_windows() {
    let (mut e, _, sessions) = in_progress();
    run(
        &mut e,
        Actor::Stakeholder,
        Command::LetGo {
            employee: "pam".into(),
        },
    );
    let mut c = ctx(idle(WINDOW));
    c.ticket_sessions = sessions;
    assert!(stall_actions(&e, &c).is_empty());
    c.stall.as_mut().expect("stall").idle_for = WINDOW * 2;
    assert_eq!(stall_actions(&e, &c), vec![Action::StallAlert]);
}

#[test]
fn the_briefing_lists_each_open_ticket_with_its_reason_and_the_closed_turns() {
    let (e, id, sessions) = in_progress();
    let mut c = ctx(idle(WINDOW));
    c.ticket_sessions = sessions;
    let text = stall::briefing(&e, &c, WINDOW, &["Dev on T-1".to_string()]);
    assert!(text.contains("20 minutes"), "{text}");
    assert!(
        text.contains(&format!(
            "- {id} \"Login\" in_progress, Dev: Dev's session is open but idle"
        )),
        "{text}"
    );
    assert!(text.contains("Dev on T-1"), "{text}");
    for tool in [
        "retry_action",
        "restart_teammate",
        "assign",
        "report_progress",
    ] {
        assert!(text.contains(tool), "{tool}: {text}");
    }
    let alert = stall::alert_detail(&e, &c, WINDOW * 2);
    assert!(
        alert.contains("40 minutes") && alert.contains(&id),
        "{alert}"
    );
}

#[test]
fn why_idle_names_the_guard_that_holds_a_ticket() {
    let (mut e, id) = ready_ticket();
    let c = Context {
        max_concurrent: 3,
        ..Context::default()
    };
    let why = |e: &Engine, c: &Context| why_idle(e, c, e.state.ticket(&id).expect("ticket"));
    assert_eq!(why(&e, &c), "ready to start on the next pass");

    let full = Context {
        sessions_open: 3,
        ..c.clone()
    };
    assert!(
        why(&e, &full).contains("all 3 agent slots"),
        "{}",
        why(&e, &full)
    );

    let cooling = Context {
        cooling_tickets: BTreeSet::from([id.clone()]),
        busy: BTreeSet::from([id.clone()]),
        ..c.clone()
    };
    assert!(why(&e, &cooling).contains("retry_action"));

    let other = crate::dev_loop_tests::story(&mut e, "Signup");
    run(
        &mut e,
        dev("pam"),
        Command::SetBlockers {
            id: id.clone(),
            blocked_by: vec![other.clone()],
        },
    );
    assert_eq!(why(&e, &c), format!("blocked by {other}"));
    run(
        &mut e,
        dev("pam"),
        Command::SetBlockers {
            id: id.clone(),
            blocked_by: vec![],
        },
    );

    // The developer already builds another ticket at their parallel limit.
    let (mut e, id, second) = two_ready();
    let why = |e: &Engine, c: &Context| why_idle(e, c, e.state.ticket(&id).expect("ticket"));
    start(&mut e, &second);
    let reason = why(&e, &c);
    assert!(
        reason.contains("max_parallel_tasks") && reason.contains(&second),
        "{reason}"
    );

    // Stopped by the stakeholder.
    apply(
        &mut e,
        EventKind::AgentMessageChunk {
            employee: "dev".into(),
            text: "x".into(),
        },
    );
    run(
        &mut e,
        Actor::Stakeholder,
        Command::CancelTurn {
            employee: "dev".into(),
        },
    );
    assert!(why(&e, &c).contains("stopped by the stakeholder"));
}

#[test]
fn why_idle_explains_a_conflicted_ticket_and_a_working_developer() {
    let (mut e, id, sessions) = in_progress();
    let c = Context {
        max_concurrent: 3,
        ticket_sessions: sessions,
        ..Context::default()
    };
    apply(
        &mut e,
        EventKind::AgentMessageChunk {
            employee: "dev".into(),
            text: "x".into(),
        },
    );
    let why = |e: &Engine| why_idle(e, &c, e.state.ticket(&id).expect("ticket"));
    assert_eq!(why(&e), "Dev is working on it");
    run(
        &mut e,
        Actor::System,
        Command::RecordSync {
            id: id.clone(),
            outcome: crate::SyncOutcome::Conflict,
            files: vec!["a.rs".into()],
            note: "CONFLICT in a.rs".into(),
        },
    );
    assert_eq!(
        why(&e),
        "conflicts with dev; Dev is resolving it (attempt 1/2)"
    );
}
