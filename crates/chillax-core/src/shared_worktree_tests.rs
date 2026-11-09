//! Shared mode (ADR 0025): one ticket at a time holds the dev worktree.

use crate::command::Command;
use crate::dev_loop_tests::{dev, engine, hire, run, start, story};
use crate::dispatch::{Action, Context, SessionKind, next_actions};
use crate::engine::Engine;
use crate::event::Actor;

fn ctx(shared: bool) -> Context {
    Context {
        max_concurrent: 8,
        shared_dev_worktree: shared,
        ..Context::default()
    }
}

/// Two tickets, each with its own developer, both ready to start.
fn two_ready() -> (Engine, String, String) {
    let mut e = engine();
    hire(&mut e, "project-manager", "Pam");
    hire(&mut e, "fullstack-developer", "Dev");
    hire(&mut e, "fullstack-developer", "Eve");
    hire(&mut e, "tech-lead", "Lead");
    let a = story(&mut e, "First");
    let b = story(&mut e, "Second");
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
    for (id, who) in [(&a, "dev"), (&b, "eve")] {
        run(
            &mut e,
            dev("pam"),
            Command::AssignTicket {
                id: id.clone(),
                assignee: Some(who.into()),
            },
        );
    }
    (e, a, b)
}

fn starts(actions: &[Action]) -> Vec<&str> {
    actions
        .iter()
        .filter_map(|a| match a {
            Action::StartWork { ticket, .. } => Some(ticket.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn per_ticket_mode_starts_both() {
    let (e, a, b) = two_ready();
    let actions = next_actions(&e, &ctx(false));
    assert_eq!(starts(&actions), [a.as_str(), b.as_str()]);
}

#[test]
fn shared_mode_starts_one_ticket_per_pass() {
    let (e, a, _) = two_ready();
    let actions = next_actions(&e, &ctx(true));
    assert_eq!(starts(&actions), [a.as_str()]);
}

#[test]
fn shared_mode_waits_while_a_start_is_in_flight() {
    let (e, a, _) = two_ready();
    let mut c = ctx(true);
    c.busy.insert(a);
    assert!(starts(&next_actions(&e, &c)).is_empty());
}

#[test]
fn shared_mode_holds_the_second_ticket_until_the_first_is_merged() {
    let (mut e, a, b) = two_ready();
    start(&mut e, &a);
    let actions = next_actions(&e, &ctx(true));
    assert!(starts(&actions).is_empty(), "{actions:?}");
    // The holder still gets its own session; the other ticket gets nothing.
    assert!(actions.iter().any(|x| matches!(x,
        Action::OpenSession { ticket, kind: SessionKind::Work, .. } if *ticket == a)));
    assert!(actions.iter().all(|x| x.ticket() != b));
    // Per-ticket mode would have started the second one.
    assert_eq!(starts(&next_actions(&e, &ctx(false))), [b.as_str()]);
}

#[test]
fn shared_mode_frees_the_worktree_when_the_holder_is_dropped() {
    let (mut e, a, b) = two_ready();
    start(&mut e, &a);
    run(
        &mut e,
        Actor::Stakeholder,
        Command::DropTicket {
            id: a,
            reason: "no".into(),
        },
    );
    assert_eq!(starts(&next_actions(&e, &ctx(true))), [b.as_str()]);
}
