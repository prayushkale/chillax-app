//! Employees suggest work to the project manager.

use crate::command::Command;
use crate::dev_loop_tests::{dev, engine, hire, run, try_run};
use crate::dispatch::{Action, Context, next_actions};
use crate::engine::Engine;
use crate::event::Actor;
use crate::intake;
use crate::model::*;

fn team() -> Engine {
    let mut e = engine();
    hire(&mut e, "project-manager", "Pam");
    hire(&mut e, "fullstack-developer", "Dev");
    e
}

fn suggest(e: &mut Engine, who: &str, kind: SuggestionKind, text: &str) {
    run(
        e,
        dev(who),
        Command::SuggestWork {
            kind,
            text: text.into(),
            ticket: None,
        },
    );
}

/// What the PM's `create_story` does: a ticket of `kind`, linked to the request.
fn file(e: &mut Engine, request: &str, kind: TicketKind) -> String {
    let kinds = run(
        e,
        dev("pam"),
        Command::CreateTicket {
            kind,
            title: "Filed".into(),
            description: String::new(),
            acceptance_criteria: vec!["works".into()],
            estimate: None,
            epic: None,
            external: None,
        },
    );
    let id = kinds
        .iter()
        .find_map(|k| match k {
            crate::event::EventKind::TicketCreated { ticket } => Some(ticket.id.clone()),
            _ => None,
        })
        .expect("ticket created");
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

#[test]
fn an_employee_suggestion_becomes_a_request_from_them() {
    let mut e = team();
    suggest(
        &mut e,
        "dev",
        SuggestionKind::Bug,
        "Login 500s on empty email",
    );
    let r = e.state.request("R-1").expect("request");
    assert_eq!(r.from.as_deref(), Some("dev"));
    assert_eq!(r.kind, Some(SuggestionKind::Bug));
    assert!(!r.start_now);
    let brief = intake::briefing(&e, r);
    assert!(brief.contains("Suggestion R-1 from Dev"), "{brief}");
}

#[test]
fn the_stakeholder_cannot_suggest_and_text_is_required() {
    let mut e = team();
    let cmd = Command::SuggestWork {
        kind: SuggestionKind::Bug,
        text: "x".into(),
        ticket: None,
    };
    assert!(try_run(&mut e, Actor::Stakeholder, cmd).is_err());
    let blank = Command::SuggestWork {
        kind: SuggestionKind::Bug,
        text: "  ".into(),
        ticket: None,
    };
    assert!(try_run(&mut e, dev("dev"), blank).is_err());
}

#[test]
fn an_employee_may_have_only_so_many_suggestions_open() {
    let mut e = team();
    for n in 0..e.team.max_open_suggestions {
        suggest(
            &mut e,
            "dev",
            SuggestionKind::Improvement,
            &format!("idea {n}"),
        );
    }
    let cmd = Command::SuggestWork {
        kind: SuggestionKind::Improvement,
        text: "one more".into(),
        ticket: None,
    };
    assert!(try_run(&mut e, dev("dev"), cmd).is_err());
}

#[test]
fn a_bug_suggestion_starts_now_and_an_improvement_waits() {
    let (mut e, _) = crate::dev_loop_tests::ready_ticket();
    suggest(&mut e, "dev", SuggestionKind::Bug, "Crash on save");
    suggest(&mut e, "dev", SuggestionKind::Improvement, "Cache the list");
    let bug = file(&mut e, "R-1", TicketKind::Bug);
    let idea = file(&mut e, "R-2", TicketKind::Story);
    let bug = e.state.ticket(&bug).expect("bug");
    assert_eq!(
        (bug.state.as_str(), bug.sprint.as_deref()),
        ("ready", Some("SP-1"))
    );
    let idea = e.state.ticket(&idea).expect("idea");
    assert_eq!(
        (idea.state.as_str(), idea.sprint.as_deref()),
        ("backlog", None)
    );
}

#[test]
fn stakeholder_requests_are_briefed_before_older_suggestions() {
    let mut e = team();
    suggest(&mut e, "dev", SuggestionKind::Bug, "Crash on save");
    run(
        &mut e,
        Actor::Stakeholder,
        Command::SubmitRequest {
            text: "Add dark mode".into(),
            start_now: false,
        },
    );
    let ctx = Context {
        max_concurrent: 3,
        ..Context::default()
    };
    let ids: Vec<_> = next_actions(&e, &ctx)
        .into_iter()
        .filter_map(|a| match a {
            Action::TriageRequest { id, .. } => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(ids, ["R-2"]);
}
