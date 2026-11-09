//! Request intake.

use crate::command::Command;
use crate::dev_loop_tests::{dev, engine, hire, run, try_run};
use crate::dispatch::{Action, Context, next_actions};
use crate::engine::Engine;
use crate::event::{Actor, EventKind};
use crate::intake;
use crate::model::*;

fn team() -> Engine {
    let mut e = engine();
    hire(&mut e, "project-manager", "Pam");
    hire(&mut e, "fullstack-developer", "Dev");
    e
}

fn ctx() -> Context {
    Context {
        max_concurrent: 3,
        ..Context::default()
    }
}

fn submit(e: &mut Engine, text: &str, start_now: bool) {
    run(
        e,
        Actor::Stakeholder,
        Command::SubmitRequest {
            text: text.into(),
            start_now,
        },
    );
}

/// What the PM's `create_story` does: a ticket, an assignee, and the link.
fn file(e: &mut Engine, request: &str, title: &str) -> String {
    let id = crate::dev_loop_tests::story(e, title);
    run(
        e,
        dev("pam"),
        Command::AssignTicket {
            id: id.clone(),
            assignee: Some("dev".into()),
        },
    );
    run(
        e,
        dev("pam"),
        Command::LinkRequestTicket {
            request: request.into(),
            ticket: id.clone(),
        },
    );
    id
}

fn triage_pm(e: &Engine, ctx: &Context) -> Vec<Action> {
    next_actions(e, ctx)
        .into_iter()
        .filter(|a| matches!(a, Action::TriageRequest { .. }))
        .collect()
}

#[test]
fn only_the_stakeholder_submits_and_text_is_required() {
    let mut e = team();
    let cmd = Command::SubmitRequest {
        text: "Add dark mode".into(),
        start_now: false,
    };
    assert!(try_run(&mut e, dev("pam"), cmd.clone()).is_err());
    assert!(
        try_run(
            &mut e,
            Actor::Stakeholder,
            Command::SubmitRequest {
                text: "  ".into(),
                start_now: false
            }
        )
        .is_err()
    );
    let long = "x".repeat(intake::MAX_TEXT + 1);
    assert!(
        try_run(
            &mut e,
            Actor::Stakeholder,
            Command::SubmitRequest {
                text: long,
                start_now: false
            }
        )
        .is_err()
    );
    run(&mut e, Actor::Stakeholder, cmd);
    let r = e.state.request("R-1").expect("request");
    assert_eq!(r.status, RequestStatus::Submitted);
    assert!(!r.start_now && r.tickets.is_empty());
}

#[test]
fn the_pm_is_briefed_with_the_roster_one_request_at_a_time() {
    let mut e = team();
    submit(&mut e, "Add dark mode", false);
    submit(&mut e, "Fix the typo", false);
    let actions = triage_pm(&e, &ctx());
    assert_eq!(actions.len(), 1);
    let Action::TriageRequest { id, pm, key } = &actions[0] else {
        unreachable!()
    };
    assert_eq!((id.as_str(), pm.as_deref()), ("R-1", Some("pam")));
    assert_eq!(key, "@request-R-1");

    let text = intake::briefing(&e, e.state.request("R-1").expect("r"));
    assert!(text.contains("Add dark mode"));
    assert!(text.contains("Dev (id `dev`)"));
    assert!(text.contains("tags developer"));
    assert!(text.contains("no open tickets"));
    assert!(text.contains("wait in the backlog"));

    // While the oldest request is cooling (busy) the next one waits.
    let mut busy = ctx();
    busy.busy.insert("@request-R-1".into());
    assert!(triage_pm(&e, &busy).is_empty());
}

#[test]
fn a_busy_or_limited_or_missing_pm_leaves_the_request_queued() {
    let mut e = engine();
    submit(&mut e, "Add dark mode", false);
    assert!(triage_pm(&e, &ctx()).is_empty());
    let r = e.state.request("R-1").expect("r").clone();
    let plans = |role: &str| role == "project-manager";
    assert!(
        intake::wait_reason(&e.state, &r, plans)
            .expect("reason")
            .contains("hire")
    );

    hire(&mut e, "project-manager", "Pam");
    assert_eq!(
        intake::wait_reason(&e.state, &r, plans).as_deref(),
        Some("waiting for the project manager to be free")
    );
    assert_eq!(triage_pm(&e, &ctx()).len(), 1);
    let mut halted = ctx();
    halted.halted_backends.insert("claude".into());
    assert!(triage_pm(&e, &halted).is_empty());
    e.state.employees[0].status = EmployeeStatus::Working;
    assert!(triage_pm(&e, &ctx()).is_empty());
}

#[test]
fn briefing_counts_attempts_and_gives_up_after_the_limit() {
    let mut e = team();
    submit(&mut e, "Add dark mode", false);
    for n in 1..=intake::MAX_ATTEMPTS {
        run(
            &mut e,
            Actor::System,
            Command::RecordRequestBriefed { id: "R-1".into() },
        );
        let r = e.state.request("R-1").expect("r");
        assert_eq!((r.attempts, r.status), (n, RequestStatus::Briefed));
    }
    let actions = triage_pm(&e, &ctx());
    assert!(matches!(
        actions.as_slice(),
        [Action::TriageRequest { pm: None, .. }]
    ));
    run(
        &mut e,
        Actor::System,
        Command::AbandonRequest { id: "R-1".into() },
    );
    assert_eq!(
        e.state.request("R-1").expect("r").status,
        RequestStatus::Abandoned
    );
    assert!(triage_pm(&e, &ctx()).is_empty());
    // Only Chillax records these.
    assert!(
        try_run(
            &mut e,
            Actor::Stakeholder,
            Command::RecordRequestBriefed { id: "R-1".into() }
        )
        .is_err()
    );
}

#[test]
fn without_start_now_tickets_wait_in_the_backlog() {
    let mut e = team();
    submit(&mut e, "Add dark mode", false);
    let t = file(&mut e, "R-1", "Dark theme");
    let ticket = e.state.ticket(&t).expect("t");
    assert_eq!(
        (ticket.state.as_str(), ticket.sprint.as_deref()),
        ("backlog", None)
    );
    assert_eq!(e.state.request("R-1").expect("r").tickets, [t]);
    assert!(e.state.sprints.is_empty());
}

#[test]
fn start_now_opens_and_approves_a_sprint_when_none_is_running() {
    let mut e = team();
    submit(&mut e, "Add dark mode", true);
    let first = file(&mut e, "R-1", "Dark theme");
    let second = file(&mut e, "R-1", "Theme toggle");
    assert_eq!(e.state.sprints.len(), 1);
    let sp = &e.state.sprints[0];
    assert_eq!(sp.status, SprintStatus::Active);
    assert_eq!(sp.name, "Request R-1");
    assert_eq!(sp.tickets, [first.clone(), second.clone()]);
    for id in [&first, &second] {
        let t = e.state.ticket(id).expect("t");
        assert_eq!(
            (t.state.as_str(), t.sprint.as_deref()),
            ("ready", Some("SP-1"))
        );
    }
    // No stakeholder gate is left waiting.
    assert!(e.state.inbox.is_empty());
    // The dispatcher can start the work at once.
    assert!(
        next_actions(&e, &ctx())
            .iter()
            .any(|a| matches!(a, Action::StartWork { ticket, .. } if *ticket == first))
    );
}

#[test]
fn start_now_joins_the_active_sprint() {
    let (mut e, _) = crate::dev_loop_tests::ready_ticket();
    submit(&mut e, "Add dark mode", true);
    let t = file(&mut e, "R-1", "Dark theme");
    let ticket = e.state.ticket(&t).expect("t");
    assert_eq!(
        (ticket.state.as_str(), ticket.sprint.as_deref()),
        ("ready", Some("SP-1"))
    );
    assert!(e.state.sprint("SP-1").expect("sp").tickets.contains(&t));
    assert_eq!(e.state.sprints.len(), 1);
}

#[test]
fn start_now_leaves_tickets_alone_while_a_sprint_awaits_approval() {
    let mut e = team();
    let planned = crate::dev_loop_tests::story(&mut e, "Planned");
    run(
        &mut e,
        dev("pam"),
        Command::ProposeSprint {
            name: "S1".into(),
            goal: String::new(),
            tickets: vec![planned],
        },
    );
    submit(&mut e, "Add dark mode", true);
    let t = file(&mut e, "R-1", "Dark theme");
    let ticket = e.state.ticket(&t).expect("t");
    assert_eq!(
        (ticket.state.as_str(), ticket.sprint.as_deref()),
        ("backlog", None)
    );
    assert_eq!(e.state.sprints.len(), 1);
}

#[test]
fn completing_files_a_note_and_closes_the_request() {
    let mut e = team();
    submit(&mut e, "Add dark mode", false);
    let t = file(&mut e, "R-1", "Dark theme");
    // Linking twice is a no-op.
    assert!(
        run(
            &mut e,
            dev("pam"),
            Command::LinkRequestTicket {
                request: "R-1".into(),
                ticket: t.clone()
            }
        )
        .is_empty()
    );
    // The stakeholder cannot play the PM.
    assert!(
        try_run(
            &mut e,
            Actor::Stakeholder,
            Command::CompleteRequest {
                id: "R-1".into(),
                summary: "x".into()
            }
        )
        .is_err()
    );
    let kinds = run(
        &mut e,
        dev("pam"),
        Command::CompleteRequest {
            id: "R-1".into(),
            summary: "Dev takes the theme".into(),
        },
    );
    assert!(matches!(
        kinds.as_slice(),
        [
            EventKind::RequestTriaged { .. },
            EventKind::AlertRaised { .. }
        ]
    ));
    let r = e.state.request("R-1").expect("r");
    assert_eq!(r.status, RequestStatus::Triaged);
    assert_eq!(r.summary.as_deref(), Some("Dev takes the theme"));
    let Some(InboxItem::Alert { title, detail, .. }) = e.state.inbox.first() else {
        panic!("no alert");
    };
    assert_eq!(title, "Request R-1 filed");
    assert!(detail.contains(&format!("{t} (story, backlog): Dark theme -> Dev")));
    assert!(detail.contains("Dev takes the theme"));
    // Done: no more briefings, no more links.
    assert!(triage_pm(&e, &ctx()).is_empty());
    assert!(
        try_run(
            &mut e,
            dev("pam"),
            Command::LinkRequestTicket {
                request: "R-1".into(),
                ticket: t
            }
        )
        .is_err()
    );
}

#[test]
fn a_request_needing_nothing_can_still_be_completed() {
    let mut e = team();
    submit(&mut e, "Is dark mode planned?", false);
    run(
        &mut e,
        dev("pam"),
        Command::CompleteRequest {
            id: "R-1".into(),
            summary: "Already T-4".into(),
        },
    );
    let Some(InboxItem::Alert { detail, .. }) = e.state.inbox.first() else {
        panic!("no alert");
    };
    assert!(detail.starts_with("No tickets were needed."));
}

#[test]
fn a_rebriefing_names_what_is_already_filed() {
    let mut e = team();
    submit(&mut e, "Add dark mode", false);
    let t = file(&mut e, "R-1", "Dark theme");
    let text = intake::briefing(&e, e.state.request("R-1").expect("r"));
    assert!(text.contains(&format!("do not file them again): {t}")));
}
