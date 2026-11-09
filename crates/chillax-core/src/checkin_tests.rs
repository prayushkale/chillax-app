//! The project manager's board check-in: the digest, the fingerprint and
//! when the dispatcher sends one.

use std::collections::BTreeMap;

use crate::checkin::{self, digest, fingerprint};
use crate::command::Command;
use crate::dev_loop_tests::{dev, engine, hire, run, story};
use crate::dispatch::{Action, CheckinCtx, Context, KEY_CHECKIN, next_actions};
use crate::engine::Engine;
use crate::event::{Actor, Event, EventKind};

/// Pam the PM and Dev, with `n` unplanned, unassigned stories.
fn board(n: usize) -> Engine {
    let mut e = engine();
    hire(&mut e, "project-manager", "Pam");
    hire(&mut e, "fullstack-developer", "Dev");
    for i in 1..=n {
        story(&mut e, &format!("Story {i}"));
    }
    e
}

fn apply(e: &mut Engine, kind: EventKind) {
    let event = Event {
        seq: e.state.last_seq + 1,
        ts: chrono::DateTime::from_timestamp(0, 0).expect("epoch is valid"),
        actor: Actor::System,
        kind,
    };
    e.apply(&event);
}

fn text(e: &Engine) -> String {
    digest(e, &BTreeMap::new(), Some(3)).expect("open work")
}

fn due() -> Context {
    Context {
        max_concurrent: 3,
        checkin: Some(CheckinCtx {
            due: true,
            ..CheckinCtx::default()
        }),
        ..Context::default()
    }
}

fn checkins(e: &Engine, ctx: &Context) -> Vec<Action> {
    next_actions(e, ctx)
        .into_iter()
        .filter(|a| matches!(a, Action::PmCheckin { .. }))
        .collect()
}

fn pam() -> Vec<Action> {
    vec![Action::PmCheckin { pm: "pam".into() }]
}

#[test]
fn no_open_work_means_no_digest() {
    let e = board(0);
    assert!(digest(&e, &BTreeMap::new(), Some(3)).is_none());
    let mut e = board(1);
    run(
        &mut e,
        Actor::Stakeholder,
        Command::DropTicket {
            id: "T-1".into(),
            reason: "no".into(),
        },
    );
    assert!(digest(&e, &BTreeMap::new(), Some(3)).is_none());
}

#[test]
fn the_digest_is_deterministic_ascii_and_lists_the_team_and_counts() {
    let e = board(2);
    let a = text(&e);
    assert_eq!(a, text(&e));
    assert!(a.starts_with("Team 2/8. "), "{a}");
    assert!(a.contains("Pam offline · Dev offline"), "{a}");
    assert!(a.contains("Open: 2 (backlog 2, ready 0"), "{a}");
    assert!(a.contains("Parallel limit 3."), "{a}");
    assert!(a.contains("Unassigned: T-1 T-2."), "{a}");
}

#[test]
fn urgent_tickets_come_first_and_get_their_own_line() {
    let mut e = board(3);
    run(
        &mut e,
        Actor::Stakeholder,
        Command::PrioritizeTicket {
            id: "T-3".into(),
            urgent: true,
        },
    );
    let a = text(&e);
    assert!(a.contains("URGENT: T-3 backlog unassigned"), "{a}");
    assert!(a.contains("Unassigned: T-3 T-1 T-2."), "{a}");
}

#[test]
fn long_lists_are_cut_with_a_count_of_the_rest() {
    let e = board(30);
    let a = text(&e);
    assert!(a.contains("T-1 T-2"), "{a}");
    assert!(a.contains("+18"), "{a}");
    assert!(!a.contains("T-30"), "{a}");
    assert!(a.chars().count() <= 1500);
}

#[test]
fn independent_startable_work_skips_blocked_tickets_and_includes_unassigned_ones() {
    let mut e = board(3);
    run(
        &mut e,
        dev("pam"),
        Command::SetBlockers {
            id: "T-2".into(),
            blocked_by: vec!["T-1".into()],
        },
    );
    let a = text(&e);
    assert!(a.contains("Waiting on blockers: T-2<-T-1."), "{a}");
    let line = a
        .lines()
        .find(|l| l.starts_with("Independent"))
        .expect("independent line");
    assert!(line.contains("T-1 T-3"), "{line}");
    assert!(!line.contains("T-2"), "{line}");
    assert!(
        line.contains("Free developers: 1. Room to hire: 6."),
        "{line}"
    );
}

#[test]
fn idle_developers_with_no_ticket_are_named() {
    let e = board(1);
    let a = text(&e);
    assert!(
        a.contains("Idle developers with no ticket: Dev (fullstack-developer)."),
        "{a}"
    );
}

#[test]
fn only_tickets_stalled_long_enough_are_named() {
    let e = board(2);
    let stalled = BTreeMap::from([("T-1".to_string(), 1), ("T-2".to_string(), 2)]);
    let a = digest(&e, &stalled, Some(3)).expect("open work");
    assert!(a.contains("Stalled"), "{a}");
    assert!(a.contains("T-2 backlog unassigned x2"), "{a}");
    assert!(!a.contains("T-1 backlog unassigned x"), "{a}");
    assert!(!text(&e).contains("Stalled"));
}

#[test]
fn the_fingerprint_moves_with_the_board_and_with_the_team_only() {
    let mut e = board(2);
    let base = fingerprint(&e.state);
    assert_eq!(base, fingerprint(&e.state));
    run(
        &mut e,
        dev("pam"),
        Command::AssignTicket {
            id: "T-1".into(),
            assignee: Some("dev".into()),
        },
    );
    let assigned = fingerprint(&e.state);
    assert_ne!(base, assigned);
    apply(
        &mut e,
        EventKind::AgentMessageChunk {
            employee: "dev".into(),
            text: "x".into(),
        },
    );
    let working = fingerprint(&e.state);
    assert_ne!(assigned, working, "a hire's state counts");
    // Chat text and the like do not.
    run(
        &mut e,
        dev("pam"),
        Command::CommentTicket {
            id: "T-1".into(),
            body: "hello".into(),
            external_id: None,
            attachments: vec![],
        },
    );
    assert_eq!(working, fingerprint(&e.state));
    hire(&mut e, "tech-lead", "Lead");
    assert_ne!(working, fingerprint(&e.state));
}

#[test]
fn the_briefing_carries_the_digest() {
    let text = checkin::briefing("Team 1/8.");
    assert!(text.starts_with("[Chillax] Board check-in."));
    assert!(text.contains("hire_teammate"));
    assert!(text.ends_with("Team 1/8."));
}

#[test]
fn a_due_check_in_goes_to_an_idle_project_manager_when_work_is_open() {
    assert_eq!(checkins(&board(2), &due()), pam());
}

#[test]
fn nothing_is_sent_when_it_is_not_due_or_check_ins_are_off() {
    let e = board(2);
    let mut ctx = due();
    ctx.checkin.as_mut().expect("on").due = false;
    assert!(checkins(&e, &ctx).is_empty());
    ctx.checkin = None;
    assert!(checkins(&e, &ctx).is_empty());
}

#[test]
fn nothing_is_sent_without_open_work_or_while_paused() {
    assert!(checkins(&board(0), &due()).is_empty());
    let mut e = board(2);
    run(&mut e, Actor::Stakeholder, Command::PauseDispatch);
    assert!(checkins(&e, &due()).is_empty());
}

#[test]
fn nothing_is_sent_to_a_stopped_busy_or_missing_project_manager() {
    let mut e = board(2);
    apply(
        &mut e,
        EventKind::AgentMessageChunk {
            employee: "pam".into(),
            text: "x".into(),
        },
    );
    assert!(checkins(&e, &due()).is_empty(), "mid-turn");
    run(
        &mut e,
        Actor::Stakeholder,
        Command::CancelTurn {
            employee: "pam".into(),
        },
    );
    assert!(checkins(&e, &due()).is_empty(), "stopped");
    let mut no_pm = engine();
    hire(&mut no_pm, "fullstack-developer", "Dev");
    story(&mut no_pm, "Story");
    assert!(checkins(&no_pm, &due()).is_empty(), "no project manager");
}

#[test]
fn nothing_is_sent_while_a_check_in_runs() {
    let mut ctx = due();
    ctx.busy.insert(KEY_CHECKIN.to_string());
    assert!(checkins(&board(2), &ctx).is_empty());
}

#[test]
fn an_unchanged_board_is_skipped_until_it_has_been_quiet_for_three_intervals() {
    let e = board(2);
    let mut ctx = due();
    let c = ctx.checkin.as_mut().expect("on");
    c.last_fingerprint = Some(fingerprint(&e.state));
    c.unchanged_intervals = 2;
    assert!(checkins(&e, &ctx).is_empty());
    ctx.checkin.as_mut().expect("on").unchanged_intervals = 3;
    assert_eq!(checkins(&e, &ctx), pam(), "a stuck board is checked anyway");
    // A changed board is sent at once.
    ctx.checkin.as_mut().expect("on").unchanged_intervals = 0;
    ctx.checkin.as_mut().expect("on").last_fingerprint = Some("old".into());
    assert_eq!(checkins(&e, &ctx), pam());
}

#[test]
fn other_project_manager_briefings_go_first() {
    // A stakeholder request waits for triage.
    let mut e = board(2);
    run(
        &mut e,
        Actor::Stakeholder,
        Command::SubmitRequest {
            text: "Add dark mode".into(),
            start_now: false,
        },
    );
    let actions = next_actions(&e, &due());
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, Action::TriageRequest { .. }))
    );
    assert!(checkins(&e, &due()).is_empty());
    // A planning request does too.
    let mut e = board(2);
    run(
        &mut e,
        Actor::Stakeholder,
        Command::RequestSprintPlanning { note: None },
    );
    let actions = next_actions(&e, &due());
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, Action::PlanSprint { .. }))
    );
    assert!(checkins(&e, &due()).is_empty());
}
