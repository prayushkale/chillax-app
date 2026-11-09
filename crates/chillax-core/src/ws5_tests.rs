//! Dropping tickets, closing sprints and planning the next one.

use crate::command::Command;
use crate::dev_loop_tests::{
    dev, engine, hire, in_review, ready_ticket, run, start, state_of, story, try_run,
};
use crate::dispatch::{Action, Context, next_actions};
use crate::engine::Engine;
use crate::error::CommandError;
use crate::event::{Actor, EventKind};
use crate::model::*;
use crate::qa_release_tests::{approve, gated};

fn drop_cmd(id: &str) -> Command {
    Command::DropTicket {
        id: id.into(),
        reason: "not needed".into(),
    }
}

fn team() -> Engine {
    let mut e = engine();
    hire(&mut e, "project-manager", "Pam");
    hire(&mut e, "fullstack-developer", "Dev");
    hire(&mut e, "tech-lead", "Lead");
    e
}

fn ctx() -> Context {
    Context {
        max_concurrent: 3,
        ..Context::default()
    }
}

#[test]
fn dropping_hides_a_ticket_and_unassigns_it() {
    let (mut e, id) = ready_ticket();
    let kinds = run(&mut e, Actor::Stakeholder, drop_cmd(&id));
    assert!(matches!(
        kinds.as_slice(),
        [
            EventKind::TicketAssigned { assignee: None, .. },
            EventKind::TicketDropped { .. }
        ]
    ));
    let t = e.state.ticket(&id).expect("ticket");
    assert_eq!(t.dropped.as_deref(), Some("not needed"));
    assert_eq!(t.assignee, None);
    // The only ticket of an active sprint was dropped: nothing left to ship.
    assert_eq!(
        e.state.sprint("SP-1").expect("sprint").status,
        SprintStatus::Closed
    );
    assert!(next_actions(&e, &ctx()).is_empty());
}

#[test]
fn only_the_stakeholder_drops_and_a_reason_is_required() {
    let (mut e, id) = ready_ticket();
    assert!(try_run(&mut e, dev("dev"), drop_cmd(&id)).is_err());
    assert!(
        try_run(
            &mut e,
            Actor::Stakeholder,
            Command::DropTicket {
                id: id.clone(),
                reason: "  ".into()
            }
        )
        .is_err()
    );
    assert!(try_run(&mut e, Actor::Stakeholder, drop_cmd("T-99")).is_err());
    run(&mut e, Actor::Stakeholder, drop_cmd(&id));
    assert!(try_run(&mut e, Actor::Stakeholder, drop_cmd(&id)).is_err());
}

#[test]
fn work_in_progress_can_be_dropped_and_the_dispatcher_forgets_it() {
    let (mut e, id) = ready_ticket();
    start(&mut e, &id);
    run(&mut e, Actor::Stakeholder, drop_cmd(&id));
    assert_eq!(e.state.ticket(&id).expect("ticket").assignee, None);
    assert!(next_actions(&e, &ctx()).is_empty());
}

#[test]
fn a_ticket_in_review_can_be_dropped() {
    let (mut e, id) = in_review();
    run(&mut e, Actor::Stakeholder, drop_cmd(&id));
    let t = e.state.ticket(&id).expect("ticket");
    assert!(t.dropped.is_some() && t.reviewer.is_none());
    assert!(next_actions(&e, &ctx()).is_empty());
}

#[test]
fn merged_and_finished_tickets_cannot_be_dropped() {
    let (mut e, id) = gated();
    // QA signed it off: it is done.
    assert_eq!(state_of(&e, &id), "done");
    assert!(matches!(
        try_run(&mut e, Actor::Stakeholder, drop_cmd(&id)),
        Err(CommandError::Invalid(m)) if m.contains("file a bug")
    ));
    // Merged into dev but still waiting for QA.
    e.state.tickets[0].state = "in_qa".into();
    assert!(e.state.tickets[0].checks.contains("merged_to_dev"));
    assert!(try_run(&mut e, Actor::Stakeholder, drop_cmd(&id)).is_err());
}

#[test]
fn a_dropped_ticket_takes_no_more_commands() {
    let mut e = team();
    let id = story(&mut e, "Login");
    run(&mut e, Actor::Stakeholder, drop_cmd(&id));
    for cmd in [
        Command::AssignTicket {
            id: id.clone(),
            assignee: Some("dev".into()),
        },
        Command::TransitionTicket {
            id: id.clone(),
            to: "ready".into(),
        },
        Command::ProposeSprint {
            name: "S".into(),
            goal: String::new(),
            tickets: vec![id.clone()],
        },
    ] {
        assert!(try_run(&mut e, dev("pam"), cmd).is_err());
    }
    // Comments stay possible, so the history can say why.
    run(
        &mut e,
        Actor::Stakeholder,
        Command::CommentTicket {
            id,
            body: "see ADR".into(),
            external_id: None,
            attachments: Vec::new(),
        },
    );
}

#[test]
fn a_dropped_blocker_no_longer_blocks() {
    let mut e = team();
    let a = story(&mut e, "A");
    let b = story(&mut e, "B");
    run(
        &mut e,
        dev("pam"),
        Command::SetBlockers {
            id: b.clone(),
            blocked_by: vec![a.clone()],
        },
    );
    assert!(!e.state.blockers_clear(e.state.ticket(&b).expect("b")));
    run(&mut e, Actor::Stakeholder, drop_cmd(&a));
    assert!(e.state.blockers_clear(e.state.ticket(&b).expect("b")));
}

#[test]
fn updating_a_ticket_can_change_its_epic() {
    let mut e = team();
    let id = story(&mut e, "A");
    run(
        &mut e,
        dev("pam"),
        Command::CreateEpic {
            title: "Auth".into(),
            description: String::new(),
        },
    );
    let update = |epic: &str| Command::UpdateTicket {
        id: id.clone(),
        title: None,
        description: None,
        acceptance_criteria: None,
        estimate: None,
        epic: Some(epic.into()),
    };
    assert!(matches!(
        try_run(&mut e, Actor::Stakeholder, update("EP-9")),
        Err(CommandError::UnknownEpic(_))
    ));
    run(&mut e, Actor::Stakeholder, update("EP-1"));
    assert_eq!(
        e.state.ticket(&id).expect("t").epic.as_deref(),
        Some("EP-1")
    );
}

/// SP-1 active with T-1 (ready) and T-2 (in progress), T-3 in the backlog.
fn stuck() -> Engine {
    let mut e = team();
    let t1 = story(&mut e, "One");
    let t2 = story(&mut e, "Two");
    story(&mut e, "Three");
    run(
        &mut e,
        dev("pam"),
        Command::ProposeSprint {
            name: "S1".into(),
            goal: String::new(),
            tickets: vec![t1.clone(), t2.clone()],
        },
    );
    run(
        &mut e,
        Actor::Stakeholder,
        Command::ApproveSprint { id: "SP-1".into() },
    );
    e.state.tickets[1].state = "in_progress".into();
    e.state.tickets[1].branch = Some("feat/t-2".into());
    e
}

#[test]
fn closing_a_sprint_frees_its_unfinished_tickets() {
    let mut e = stuck();
    let close = || Command::CloseSprint { id: "SP-1".into() };
    assert!(try_run(&mut e, dev("pam"), close()).is_err());
    let kinds = run(&mut e, Actor::Stakeholder, close());
    // T-1 was only planned, so it goes back to the backlog.
    assert!(kinds.iter().any(|k| matches!(
        k,
        EventKind::TicketTransitioned { id, to, .. } if id == "T-1" && to == "backlog"
    )));
    assert!(matches!(
        kinds.last(),
        Some(EventKind::SprintClosed { carried, .. }) if carried == &["T-1", "T-2"]
    ));
    let sp = e.state.sprint("SP-1").expect("sprint");
    assert_eq!(sp.status, SprintStatus::Closed);
    assert!(sp.tickets.is_empty());
    assert_eq!(state_of(&e, "T-1"), "backlog");
    assert_eq!(state_of(&e, "T-2"), "in_progress");
    assert!(e.state.tickets.iter().all(|t| t.sprint.is_none()));
    // Only an active sprint closes.
    assert!(try_run(&mut e, Actor::Stakeholder, close()).is_err());
    assert!(
        try_run(
            &mut e,
            Actor::Stakeholder,
            Command::CloseSprint { id: "SP-7".into() }
        )
        .is_err()
    );
}

#[test]
fn leftovers_of_a_closed_sprint_join_the_next_proposal() {
    let mut e = stuck();
    // A sprint in flight still holds its tickets.
    assert!(
        try_run(
            &mut e,
            dev("pam"),
            Command::ProposeSprint {
                name: "S2".into(),
                goal: String::new(),
                tickets: vec!["T-2".into()],
            }
        )
        .is_err()
    );
    run(
        &mut e,
        Actor::Stakeholder,
        Command::CloseSprint { id: "SP-1".into() },
    );
    run(
        &mut e,
        dev("pam"),
        Command::ProposeSprint {
            name: "S2".into(),
            goal: String::new(),
            tickets: vec!["T-2".into(), "T-3".into()],
        },
    );
    assert_eq!(
        e.state.ticket("T-2").expect("t").sprint.as_deref(),
        Some("SP-2")
    );
}

#[test]
fn a_release_releases_every_sprint_whose_work_shipped() {
    let (mut e, _) = gated();
    // SP-9 stayed active after an earlier release: its work is shipped or dropped.
    let mut shipped = e.state.tickets[0].clone();
    shipped.id = "T-8".into();
    shipped.state = "done".into();
    shipped.checks.insert("released".into());
    let mut dropped = shipped.clone();
    dropped.id = "T-9".into();
    dropped.state = "backlog".into();
    dropped.checks.clear();
    dropped.dropped = Some("no".into());
    e.state.tickets.extend([shipped, dropped]);
    e.state.sprints.push(Sprint {
        id: "SP-9".into(),
        name: "old".into(),
        goal: String::new(),
        tickets: vec!["T-8".into(), "T-9".into()],
        status: SprintStatus::Active,
        external: None,
    });
    // And one with unfinished work stays open.
    let mut open = e.state.tickets[0].clone();
    open.id = "T-7".into();
    open.state = "in_progress".into();
    open.checks.clear();
    e.state.tickets.push(open);
    e.state.sprints.push(Sprint {
        id: "SP-8".into(),
        name: "open".into(),
        goal: String::new(),
        tickets: vec!["T-7".into()],
        status: SprintStatus::Active,
        external: None,
    });
    approve(&mut e, "v0.1.0").expect("approved");
    run(
        &mut e,
        Actor::System,
        Command::RecordRelease {
            id: "R-1".into(),
            tag: "v0.1.0".into(),
            commit: "abc1234".into(),
        },
    );
    let status = |id| e.state.sprint(id).expect("sprint").status;
    assert_eq!(status("SP-1"), SprintStatus::Released);
    assert_eq!(status("SP-9"), SprintStatus::Released);
    assert_eq!(status("SP-8"), SprintStatus::Active);
}

#[test]
fn planning_waits_for_the_sprint_to_end() {
    let mut e = stuck();
    let plan = |note: Option<&str>| Command::RequestSprintPlanning {
        note: note.map(String::from),
    };
    assert!(matches!(
        try_run(&mut e, Actor::Stakeholder, plan(None)),
        Err(CommandError::Invalid(m)) if m.contains("SP-1")
    ));
    run(
        &mut e,
        Actor::Stakeholder,
        Command::CloseSprint { id: "SP-1".into() },
    );
    assert!(try_run(&mut e, dev("pam"), plan(None)).is_err());
    run(&mut e, Actor::Stakeholder, plan(Some("  ")));
    assert_eq!(
        e.state.planning,
        Some(PlanningRequest {
            note: None,
            briefed: false
        })
    );
}

#[test]
fn planning_allowed_with_empty_backlog_after_release() {
    let mut e = team();
    run(
        &mut e,
        Actor::Stakeholder,
        Command::RequestSprintPlanning { note: None },
    );
    assert!(e.state.planning.as_ref().is_some_and(|p| !p.briefed));
}

#[test]
fn the_planner_is_briefed_once_and_a_proposal_ends_the_request() {
    let mut e = stuck();
    run(
        &mut e,
        Actor::Stakeholder,
        Command::CloseSprint { id: "SP-1".into() },
    );
    assert!(
        next_actions(&e, &ctx())
            .iter()
            .all(|a| !matches!(a, Action::PlanSprint { .. }))
    );
    run(
        &mut e,
        Actor::Stakeholder,
        Command::RequestSprintPlanning { note: None },
    );
    assert!(next_actions(&e, &ctx()).contains(&Action::PlanSprint { pm: "pam".into() }));
    // A busy planner waits; so does a claimed briefing.
    let mut busy = ctx();
    busy.busy.insert(crate::dispatch::KEY_PLAN.into());
    assert!(
        next_actions(&e, &busy)
            .iter()
            .all(|a| !matches!(a, Action::PlanSprint { .. }))
    );
    e.state.employees[0].status = EmployeeStatus::Working;
    assert!(
        next_actions(&e, &ctx())
            .iter()
            .all(|a| !matches!(a, Action::PlanSprint { .. }))
    );
    e.state.employees[0].status = EmployeeStatus::Idle;

    assert!(try_run(&mut e, Actor::Stakeholder, Command::RecordPlanningBriefed).is_err());
    run(&mut e, Actor::System, Command::RecordPlanningBriefed);
    assert!(e.state.planning.as_ref().is_some_and(|p| p.briefed));
    assert!(
        next_actions(&e, &ctx())
            .iter()
            .all(|a| !matches!(a, Action::PlanSprint { .. }))
    );
    // Asking again briefs again.
    run(
        &mut e,
        Actor::Stakeholder,
        Command::RequestSprintPlanning { note: None },
    );
    assert!(e.state.planning.as_ref().is_some_and(|p| !p.briefed));

    run(
        &mut e,
        dev("pam"),
        Command::ProposeSprint {
            name: "S2".into(),
            goal: String::new(),
            tickets: vec!["T-3".into()],
        },
    );
    assert_eq!(e.state.planning, None);
}
