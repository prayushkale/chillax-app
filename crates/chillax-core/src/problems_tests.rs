//! The pure rules of problem briefings (ADR 0032).

use crate::command::Command;
use crate::dev_loop_tests::{dev, engine, hire, run, story};
use crate::dispatch::{
    Action, CheckinCtx, Context, KEY_CHECKIN, KEY_PROBLEM, ProblemCtx, next_actions,
};
use crate::engine::Engine;
use crate::event::{Event, EventKind};
use crate::problems::*;

fn apply(e: &mut Engine, kind: EventKind) {
    let event = Event {
        seq: e.state.last_seq + 1,
        ts: chrono::DateTime::from_timestamp(0, 0).expect("epoch is valid"),
        actor: crate::event::Actor::System,
        kind,
    };
    e.apply(&event);
}

fn team() -> Engine {
    let mut e = engine();
    hire(&mut e, "project-manager", "Pam");
    hire(&mut e, "fullstack-developer", "Dev");
    e
}

fn fail(e: &mut Engine, who: &str) {
    apply(
        e,
        EventKind::AgentFailed {
            employee: who.into(),
            error: "boom".into(),
            ticket: None,
        },
    );
}

/// The stakeholder stops `who` mid-turn, which holds them.
pub(crate) fn hold(e: &mut Engine, who: &str) {
    apply(
        e,
        EventKind::AgentMessageChunk {
            employee: who.into(),
            text: "x".into(),
        },
    );
    run(
        e,
        crate::event::Actor::Stakeholder,
        Command::CancelTurn {
            employee: who.into(),
        },
    );
}

fn note(key: &str, kind: ProblemKind) -> ProblemNote {
    ProblemNote {
        key: key.into(),
        kind,
        subject: "thing".into(),
        detail: "it broke".into(),
        employee: None,
        first_seen_secs: 0,
        attempts: 1,
    }
}

#[test]
fn ordered_by_kind_then_key_and_capped() {
    let e = team();
    let notes = [
        note("b", ProblemKind::AppCrashing),
        note("z", ProblemKind::ActionFailed),
        note("a", ProblemKind::AppCrashing),
    ];
    let keys: Vec<String> = problems(&e, &notes, 10)
        .into_iter()
        .map(|p| p.key)
        .collect();
    assert_eq!(keys, ["z", "a", "b"]);
    assert_eq!(problems(&e, &notes, 2).len(), 2);
    assert!(problems(&e, &[], 5).is_empty());
}

#[test]
fn a_failed_developer_is_listed_but_the_pm_and_held_ones_are_not() {
    let mut e = team();
    fail(&mut e, "dev");
    fail(&mut e, "pam");
    let found = problems(&e, &[], 5);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].key, "agent-dev");
    // A held employee is never briefed about, whatever else is true of them.
    hold(&mut e, "dev");
    assert!(e.state.employee("dev").expect("dev").stopped);
    assert!(problems(&e, &[], 5).is_empty());
}

#[test]
fn a_failed_developer_on_a_limited_backend_is_not_a_problem_for_the_pm() {
    let mut e = team();
    fail(&mut e, "dev");
    let backend = e
        .state
        .employee("dev")
        .expect("dev")
        .employee
        .backend
        .clone();
    apply(
        &mut e,
        EventKind::BackendLimited {
            backend,
            until: None,
            message: "limit".into(),
        },
    );
    assert!(problems(&e, &[], 5).is_empty());
}

#[test]
fn a_note_about_a_held_employee_is_dropped() {
    let mut e = team();
    hold(&mut e, "dev");
    let mut n = note("x", ProblemKind::AgentStuck);
    n.employee = Some("dev".into());
    assert!(problems(&e, &[n], 5).is_empty());
    assert!(dev("dev").employee().is_some());
}

#[test]
fn the_briefing_is_deterministic_and_capped() {
    let p = Problem {
        key: "@promote".into(),
        kind: ProblemKind::ActionFailed,
        subject: "promote".into(),
        detail: "merge\nfailed  badly".into(),
        attempts: 1,
    };
    let text = briefing(std::slice::from_ref(&p), 2).expect("some");
    assert!(
        text.ends_with("P-@promote | action-failed | promote | merge failed badly | tried 1/2")
    );
    assert_eq!(briefing(&[], 2), None);
    let many: Vec<Problem> = (0..60)
        .map(|i| Problem {
            key: format!("k{i:02}"),
            ..p.clone()
        })
        .collect();
    let text = briefing(&many, 2).expect("some");
    assert!(text.chars().count() <= 1500);
    assert!(text.contains("more"));
    let long = Problem {
        detail: "x".repeat(500),
        ..p
    };
    let text = briefing(&[long], 2).expect("some");
    assert!(text.lines().last().expect("line").chars().count() < 300);
}

fn ctx_with(notes: Vec<ProblemNote>, due: bool) -> Context {
    Context {
        max_concurrent: 3,
        problem: Some(ProblemCtx {
            due,
            notes,
            escalated: Default::default(),
        }),
        ..Context::default()
    }
}

fn handles(e: &Engine, ctx: &Context) -> Vec<Action> {
    next_actions(e, ctx)
        .into_iter()
        .filter(|a| matches!(a, Action::PmHandleProblem { .. }))
        .collect()
}

#[test]
fn a_due_problem_briefs_the_free_pm_and_claims_its_key() {
    let e = team();
    let ctx = ctx_with(vec![note("@promote", ProblemKind::ActionFailed)], true);
    let found = handles(&e, &ctx);
    assert_eq!(found, [Action::PmHandleProblem { pm: "pam".into() }]);
    assert_eq!(found[0].ticket(), KEY_PROBLEM);
}

#[test]
fn nothing_is_sent_when_off_not_due_empty_busy_or_the_pm_is_held() {
    let mut e = team();
    let n = || vec![note("@promote", ProblemKind::ActionFailed)];
    assert!(handles(&e, &Context::default()).is_empty());
    assert!(handles(&e, &ctx_with(n(), false)).is_empty());
    assert!(handles(&e, &ctx_with(vec![], true)).is_empty());
    let mut busy = ctx_with(n(), true);
    busy.busy.insert(KEY_PROBLEM.into());
    assert!(handles(&e, &busy).is_empty());
    let mut done = ctx_with(n(), true);
    done.problem
        .as_mut()
        .expect("some")
        .escalated
        .insert("@promote".into());
    assert!(handles(&e, &done).is_empty());
    hold(&mut e, "pam");
    assert!(handles(&e, &ctx_with(n(), true)).is_empty());
}

#[test]
fn a_note_about_a_held_developer_does_not_brief_the_pm() {
    let mut e = team();
    hold(&mut e, "dev");
    let mut n = note("t", ProblemKind::AgentStuck);
    n.employee = Some("dev".into());
    assert!(handles(&e, &ctx_with(vec![n], true)).is_empty());
}

#[test]
fn problems_wait_for_a_request_and_the_checkin_yields_to_them() {
    let mut e = team();
    story(&mut e, "Story");
    let mut ctx = ctx_with(vec![note("@promote", ProblemKind::ActionFailed)], true);
    ctx.checkin = Some(CheckinCtx {
        due: true,
        ..CheckinCtx::default()
    });
    let actions = next_actions(&e, &ctx);
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, Action::PmHandleProblem { .. }))
    );
    assert!(
        !actions
            .iter()
            .any(|a| matches!(a, Action::PmCheckin { .. }))
    );
    // Once the problem is handled the check-in goes out.
    ctx.problem = None;
    let actions = next_actions(&e, &ctx);
    assert!(actions.iter().any(|a| a.ticket() == KEY_CHECKIN));
    // A request owed to the PM goes first.
    run(
        &mut e,
        crate::event::Actor::Stakeholder,
        Command::SubmitRequest {
            text: "do it".into(),
            start_now: false,
        },
    );
    let ctx = ctx_with(vec![note("@promote", ProblemKind::ActionFailed)], true);
    let actions = next_actions(&e, &ctx);
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, Action::TriageRequest { pm: Some(_), .. }))
    );
    assert!(
        !actions
            .iter()
            .any(|a| matches!(a, Action::PmHandleProblem { .. }))
    );
}

mod authority {
    use super::*;
    use crate::dev_loop_tests::try_run;
    use crate::event::Actor;

    fn all(e: &mut Engine, actor: Actor) -> Vec<Result<(), String>> {
        [
            Command::ResolveProblem {
                key: "@promote".into(),
                summary: "retried".into(),
            },
            Command::RetryAction {
                key: "@promote".into(),
            },
            Command::RestartEmployee {
                employee: "dev".into(),
            },
        ]
        .into_iter()
        .map(|c| {
            e.decide(&actor, c)
                .map(|_| ())
                .map_err(|err| err.to_string())
        })
        .collect()
    }

    #[test]
    fn the_pm_may_but_nobody_else_can() {
        let mut e = team();
        hire(&mut e, "tech-lead", "Lead");
        hire(&mut e, "qa-engineer", "Quinn");
        assert!(all(&mut e, dev("pam")).iter().all(Result::is_ok));
        for who in [
            Actor::Stakeholder,
            Actor::System,
            dev("dev"),
            dev("lead"),
            dev("quinn"),
            dev("ghost"),
        ] {
            assert!(
                all(&mut e, who.clone()).iter().all(Result::is_err),
                "{who:?}"
            );
        }
    }

    #[test]
    fn resolving_leaves_an_info_notice_with_the_summary() {
        let mut e = team();
        let kinds = run(
            &mut e,
            dev("pam"),
            Command::ResolveProblem {
                key: "@promote".into(),
                summary: "retried the promotion".into(),
            },
        );
        assert!(matches!(&kinds[0], EventKind::ProblemResolved { key, .. } if key == "@promote"));
        let note = e.state.inbox.iter().any(|i| {
            matches!(i, crate::model::InboxItem::Alert { detail, .. }
                if detail == "PM handled: retried the promotion")
        });
        assert!(note);
        assert!(
            try_run(
                &mut e,
                dev("pam"),
                Command::ResolveProblem {
                    key: " ".into(),
                    summary: "x".into()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn restart_refuses_a_held_employee_the_pm_and_strangers_and_clears_failure() {
        let mut e = team();
        let restart = |who: &str| Command::RestartEmployee {
            employee: who.into(),
        };
        assert!(try_run(&mut e, dev("pam"), restart("pam")).is_err());
        assert!(try_run(&mut e, dev("pam"), restart("nobody")).is_err());
        fail(&mut e, "dev");
        assert_eq!(
            e.state.employee("dev").expect("dev").status,
            crate::model::EmployeeStatus::Failed
        );
        run(&mut e, dev("pam"), restart("dev"));
        // The event applied by `run` clears the failure.
        assert_eq!(
            e.state.employee("dev").expect("dev").status,
            crate::model::EmployeeStatus::Offline
        );
        hold(&mut e, "dev");
        assert!(try_run(&mut e, dev("pam"), restart("dev")).is_err());
    }

    #[test]
    fn the_new_commands_and_events_round_trip() {
        for json in [
            r#"{"type":"resolve_problem","key":"k","summary":"s"}"#,
            r#"{"type":"retry_action","key":"k"}"#,
            r#"{"type":"restart_employee","employee":"dev"}"#,
        ] {
            let c: Command = serde_json::from_str(json).expect("reads");
            assert_eq!(serde_json::to_string(&c).expect("writes"), json);
        }
        for kind in [
            EventKind::ProblemResolved {
                key: "k".into(),
                summary: "s".into(),
            },
            EventKind::ActionRetryRequested { key: "k".into() },
            EventKind::EmployeeRestarted {
                employee: "dev".into(),
            },
        ] {
            let json = serde_json::to_string(&kind).expect("writes");
            assert_eq!(
                serde_json::from_str::<EventKind>(&json).expect("reads"),
                kind
            );
        }
    }
}
