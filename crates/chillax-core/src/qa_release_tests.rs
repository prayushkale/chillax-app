//! QA rounds: promotion, the environment steps, and QA's verdicts.

use crate::command::Command;
use crate::dev_loop_tests::{dev, engine, hire, in_review, run, state_of, try_run};
use crate::engine::Engine;
use crate::error::CommandError;
use crate::event::{Actor, EventKind};
use crate::model::*;

/// Dev's ticket T-1 (criterion "works") merged into `dev`, so it waits in `in_qa`.
fn in_qa() -> (Engine, String) {
    let (mut e, id) = in_review();
    run(
        &mut e,
        dev("lead"),
        Command::SubmitReview {
            id: id.clone(),
            verdict: ReviewVerdict::Approve,
            body: String::new(),
        },
    );
    run(
        &mut e,
        Actor::System,
        Command::RecordMerge {
            id: id.clone(),
            commit: "abc1234".into(),
        },
    );
    hire(&mut e, "qa-engineer", "Quinn");
    (e, id)
}

fn promote(e: &mut Engine) {
    run(
        e,
        Actor::System,
        Command::RecordPromotion {
            commit: "def5678".into(),
        },
    );
}

fn env_run(e: &mut Engine, step: EnvStep, ok: bool) {
    run(
        e,
        Actor::System,
        Command::RecordEnvRun {
            step,
            ok,
            skipped: false,
            output_tail: "output".into(),
        },
    );
}

/// A round with the environment up and tested, ready for QA's verdicts.
fn testing() -> (Engine, String) {
    let (mut e, id) = in_qa();
    promote(&mut e);
    env_run(&mut e, EnvStep::Up, true);
    env_run(&mut e, EnvStep::Test, true);
    (e, id)
}

fn passed(criteria: &[&str]) -> Vec<CriterionResult> {
    criteria
        .iter()
        .map(|c| CriterionResult {
            criterion: (*c).into(),
            passed: true,
            evidence: "ran it, saw it work".into(),
        })
        .collect()
}

fn verdict(id: &str, verdict: QaVerdict, criteria: Vec<CriterionResult>) -> Command {
    Command::SubmitQaVerdict {
        id: id.into(),
        verdict,
        criteria,
        notes: "notes".into(),
        bug: None,
    }
}

/// A failing verdict on `id` with the bug QA filed.
fn failure(id: &str) -> Command {
    let mut criteria = passed(&["works"]);
    criteria[0].passed = false;
    Command::SubmitQaVerdict {
        id: id.into(),
        verdict: QaVerdict::Fail,
        criteria,
        notes: "notes".into(),
        bug: Some(NewBug {
            title: "Login 500s".into(),
            description: "Steps, expected, actual".into(),
        }),
    }
}

fn round(e: &Engine) -> &QaRound {
    e.state.qa.as_ref().expect("a round")
}

#[test]
fn promotion_starts_a_round_over_the_tickets_waiting_in_qa() {
    let (mut e, id) = in_qa();
    promote(&mut e);
    let r = round(&e);
    assert_eq!(r.number, 1);
    assert_eq!(r.commit, "def5678");
    assert_eq!(r.status, QaRoundStatus::Starting);
    assert_eq!(r.tickets, [id]);
    assert_eq!(r.sprint.as_deref(), Some("SP-1"));
}

#[test]
fn promotion_needs_tickets_and_is_for_chillax_alone() {
    let (mut e, _) = in_qa();
    let cmd = || Command::RecordPromotion {
        commit: "def5678".into(),
    };
    assert!(try_run(&mut e, dev("quinn"), cmd()).is_err());
    assert!(try_run(&mut e, Actor::Stakeholder, cmd()).is_err());
    promote(&mut e);
    // Only one round at a time.
    assert!(try_run(&mut e, Actor::System, cmd()).is_err());

    let mut empty = engine();
    assert!(try_run(&mut empty, Actor::System, cmd()).is_err());
}

#[test]
fn a_promotion_request_is_recorded_for_the_stakeholder_and_chillax() {
    let (mut e, _) = in_qa();
    assert!(try_run(&mut e, dev("quinn"), Command::PromoteToQa).is_err());
    run(&mut e, Actor::Stakeholder, Command::PromoteToQa);
    assert!(e.state.qa_requested);
    promote(&mut e);
    assert!(!e.state.qa_requested);
}

#[test]
fn the_environment_comes_up_then_is_tested_before_agents_start() {
    let (mut e, _) = in_qa();
    promote(&mut e);
    env_run(&mut e, EnvStep::Up, true);
    assert_eq!(round(&e).status, QaRoundStatus::Starting);
    env_run(&mut e, EnvStep::Test, true);
    assert_eq!(round(&e).status, QaRoundStatus::Testing);
}

#[test]
fn a_failed_environment_step_alerts_the_stakeholder_and_files_no_bugs() {
    let (mut e, _) = in_qa();
    promote(&mut e);
    env_run(&mut e, EnvStep::Up, false);
    assert_eq!(round(&e).status, QaRoundStatus::EnvFailed);
    assert!(matches!(
        e.state.inbox.last(),
        Some(InboxItem::EnvAlert {
            step: EnvStep::Up,
            ..
        })
    ));
    assert_eq!(e.state.tickets.len(), 1, "no bug was filed");
}

#[test]
fn a_pass_with_every_criterion_proven_signs_the_ticket_off() {
    let (mut e, id) = testing();
    run(
        &mut e,
        dev("quinn"),
        verdict(&id, QaVerdict::Pass, passed(&["works"])),
    );
    let t = e.state.ticket(&id).expect("ticket");
    assert_eq!(t.state, "done");
    assert!(t.checks.contains("qa_passed"));
    assert_eq!(round(&e).verdicts[&id], QaVerdict::Pass);
}

#[test]
fn a_pass_cannot_be_a_rubber_stamp() {
    let (mut e, id) = testing();
    let refused = |e: &mut Engine, criteria| {
        try_run(e, dev("quinn"), verdict(&id, QaVerdict::Pass, criteria)).is_err()
    };
    assert!(refused(&mut e, Vec::new()), "no criteria covered");
    let mut failing = passed(&["works"]);
    failing[0].passed = false;
    assert!(refused(&mut e, failing), "a failed criterion");
    let mut no_evidence = passed(&["works"]);
    no_evidence[0].evidence = "  ".into();
    assert!(refused(&mut e, no_evidence), "no evidence");
    assert_eq!(state_of(&e, &id), "in_qa");
}

#[test]
fn only_the_qa_role_gives_verdicts_and_only_while_testing() {
    let (mut e, id) = in_qa();
    promote(&mut e);
    let pass = || verdict(&id, QaVerdict::Pass, passed(&["works"]));
    // The round is still coming up.
    assert!(try_run(&mut e, dev("quinn"), pass()).is_err());
    env_run(&mut e, EnvStep::Up, true);
    env_run(&mut e, EnvStep::Test, true);
    assert!(try_run(&mut e, dev("dev"), pass()).is_err(), "a developer");
    assert!(try_run(&mut e, Actor::Stakeholder, pass()).is_err());
    run(&mut e, dev("quinn"), pass());
    // One verdict per ticket per round.
    assert!(try_run(&mut e, dev("quinn"), pass()).is_err());
}

#[test]
fn a_failure_files_a_bug_for_the_developer_and_the_story_waits() {
    let (mut e, id) = testing();
    // A failing verdict has to say what is wrong.
    let mut no_bug = failure(&id);
    if let Command::SubmitQaVerdict { bug, .. } = &mut no_bug {
        *bug = None;
    }
    assert!(try_run(&mut e, dev("quinn"), no_bug).is_err());

    let kinds = run(&mut e, dev("quinn"), failure(&id));
    assert!(
        kinds.iter().any(
            |k| matches!(k, EventKind::BugFiled { story, bug } if story == &id && bug == "T-2")
        )
    );

    let story = e.state.ticket(&id).expect("story");
    assert_eq!(story.state, "in_qa");
    assert!(story.checks.contains("qa_failed"));
    assert!(!story.checks.contains("qa_passed"));

    let bug = e.state.ticket("T-2").expect("bug");
    assert_eq!(bug.kind, TicketKind::Bug);
    assert_eq!(bug.found_in.as_deref(), Some(id.as_str()));
    assert_eq!(bug.state, "ready");
    assert_eq!(bug.assignee.as_deref(), Some("dev"));
    assert_eq!(bug.sprint.as_deref(), Some("SP-1"));
    let sprint = e.state.sprint("SP-1").expect("sprint");
    assert!(sprint.tickets.contains(&"T-2".to_string()));
}

#[test]
fn an_environment_verdict_is_retried_once_then_escalated() {
    let (mut e, id) = testing();
    let env = || verdict(&id, QaVerdict::Env, Vec::new());
    run(&mut e, dev("quinn"), env());
    assert!(round(&e).verdicts.is_empty(), "the first one is a retry");
    assert_eq!(round(&e).status, QaRoundStatus::Testing);
    run(&mut e, dev("quinn"), env());
    assert_eq!(round(&e).status, QaRoundStatus::EnvFailed);
    assert!(matches!(
        e.state.inbox.last(),
        Some(InboxItem::EnvAlert { .. })
    ));
    assert_eq!(state_of(&e, &id), "in_qa");
}

#[test]
fn sending_a_ticket_back_clears_what_qa_recorded() {
    let (mut e, id) = testing();
    run(&mut e, dev("quinn"), failure(&id));
    run(
        &mut e,
        Actor::Stakeholder,
        Command::TransitionTicket {
            id: id.clone(),
            to: "in_progress".into(),
        },
    );
    let t = e.state.ticket(&id).expect("ticket");
    assert!(!t.checks.contains("qa_failed"));
}

#[test]
fn nobody_marks_a_ticket_done_by_hand() {
    let (mut e, id) = testing();
    let result = try_run(
        &mut e,
        Actor::Stakeholder,
        Command::TransitionTicket {
            id,
            to: "done".into(),
        },
    );
    assert!(matches!(result, Err(CommandError::Invalid(_))));
}

// ---- closing a round and the release gate ----

fn close(e: &mut Engine) -> Vec<EventKind> {
    run(e, Actor::System, Command::CloseQaRound)
}

fn pass(e: &mut Engine, id: &str) {
    run(
        e,
        dev("quinn"),
        verdict(id, QaVerdict::Pass, passed(&["works"])),
    );
}

/// A closed, clean round with a release proposed at the gate.
pub(crate) fn gated() -> (Engine, String) {
    let (mut e, id) = testing();
    pass(&mut e, &id);
    close(&mut e);
    (e, id)
}

#[test]
fn a_clean_round_mid_sprint_proposes_no_release_until_the_sprint_is_done() {
    let (mut e, id) = testing();
    // Another ticket of the sprint is still being built.
    let mut other = e.state.ticket(&id).expect("ticket").clone();
    other.id = "T-9".into();
    other.state = "in_progress".into();
    other.checks.clear();
    e.state.tickets.push(other);
    e.state
        .sprints
        .iter_mut()
        .find(|s| s.id == "SP-1")
        .expect("sprint")
        .tickets
        .push("T-9".into());
    pass(&mut e, &id);
    close(&mut e);
    assert!(e.state.releases.is_empty(), "the sprint is still open");
    assert_eq!(state_of(&e, &id), "done");
    // The manual gate still ships what is done.
    run(
        &mut e,
        Actor::Stakeholder,
        Command::OpenReleaseGate { force: false },
    );
    assert_eq!(release_of(&e).tickets, [id]);
}

pub(crate) fn approve(e: &mut Engine, version: &str) -> Result<Vec<EventKind>, CommandError> {
    try_run(
        e,
        Actor::Stakeholder,
        Command::ApproveRelease {
            id: "R-1".into(),
            version: version.into(),
            notes: "notes".into(),
        },
    )
}

fn release_of(e: &Engine) -> &Release {
    e.state.releases.last().expect("a release")
}

#[test]
fn a_round_closes_only_when_every_ticket_has_a_verdict() {
    let (mut e, id) = testing();
    assert!(try_run(&mut e, Actor::System, Command::CloseQaRound).is_err());
    assert!(try_run(&mut e, Actor::Stakeholder, Command::CloseQaRound).is_err());
    pass(&mut e, &id);
    close(&mut e);
    assert_eq!(round(&e).status, QaRoundStatus::Closed);
    // A closed round no longer takes verdicts, and the next can start.
    assert!(e.state.open_round().is_none());
}

#[test]
fn a_clean_round_proposes_a_release_at_the_gate() {
    let (e, id) = gated();
    let r = release_of(&e);
    assert_eq!(r.id, "R-1");
    assert_eq!(r.version, "v0.1.0");
    assert_eq!(r.status, ReleaseStatus::Gated);
    assert_eq!(r.tickets, [id]);
    assert!(r.known_issues.is_empty());
    assert!(r.notes.contains("T-1"));
    assert!(matches!(
        e.state.inbox.last(),
        Some(InboxItem::ReleaseGate { release, .. }) if release == "R-1"
    ));
}

#[test]
fn a_round_with_a_failure_proposes_nothing_until_forced() {
    let (mut e, id) = testing();
    run(&mut e, dev("quinn"), failure(&id));
    close(&mut e);
    assert!(e.state.releases.is_empty());
    let open = |force| Command::OpenReleaseGate { force };
    assert!(try_run(&mut e, Actor::Stakeholder, open(false)).is_err());
    // Nothing has passed, but forcing ships the failure as a known issue.
    run(&mut e, Actor::Stakeholder, open(true));
    let r = release_of(&e);
    assert!(r.tickets.is_empty());
    assert_eq!(r.known_issues, [id]);
}

#[test]
fn a_forced_gate_with_nothing_done_and_no_known_issues_is_refused() {
    let (mut empty, _) = in_qa();
    // The ticket waits in QA: nothing is done and nothing failed.
    assert!(
        try_run(
            &mut empty,
            Actor::Stakeholder,
            Command::OpenReleaseGate { force: true }
        )
        .is_err()
    );
}

/// Dev builds `id` (assigned to Dev in an approved sprint) through review and merge.
fn merge_through(e: &mut Engine, id: &str) {
    run(
        e,
        Actor::System,
        Command::StartWork {
            id: id.into(),
            branch: format!("feat/{id}"),
            worktree: format!(".chillax/worktrees/{id}"),
        },
    );
    run(
        e,
        Actor::System,
        Command::RecordTests {
            id: id.into(),
            passed: true,
            skipped: false,
            output_tail: String::new(),
        },
    );
    run(
        e,
        Actor::System,
        Command::StartReview {
            id: id.into(),
            reviewer: "lead".into(),
        },
    );
    run(
        e,
        dev("lead"),
        Command::SubmitReview {
            id: id.into(),
            verdict: ReviewVerdict::Approve,
            body: String::new(),
        },
    );
    run(
        e,
        Actor::System,
        Command::RecordMerge {
            id: id.into(),
            commit: "bbb2222".into(),
        },
    );
}

#[test]
fn a_forced_gate_ships_what_passed_and_lists_what_failed() {
    let (mut e, first) = in_qa();
    let second = crate::dev_loop_tests::story(&mut e, "Logout");
    run(
        &mut e,
        dev("pam"),
        Command::ProposeSprint {
            name: "S2".into(),
            goal: String::new(),
            tickets: vec![second.clone()],
        },
    );
    run(
        &mut e,
        Actor::Stakeholder,
        Command::ApproveSprint { id: "SP-2".into() },
    );
    run(
        &mut e,
        dev("pam"),
        Command::AssignTicket {
            id: second.clone(),
            assignee: Some("dev".into()),
        },
    );
    merge_through(&mut e, &second);

    promote(&mut e);
    assert_eq!(round(&e).tickets.len(), 2);
    env_run(&mut e, EnvStep::Up, true);
    env_run(&mut e, EnvStep::Test, true);
    pass(&mut e, &first);
    run(&mut e, dev("quinn"), failure(&second));
    close(&mut e);

    // A failure holds the release back...
    assert!(e.state.releases.is_empty());
    let open = |force| Command::OpenReleaseGate { force };
    assert!(try_run(&mut e, Actor::Stakeholder, open(false)).is_err());
    // ...unless the stakeholder ships what passed, with the failure as a known issue.
    run(&mut e, Actor::Stakeholder, open(true));
    let r = release_of(&e);
    assert_eq!(r.tickets, [first]);
    assert_eq!(r.known_issues, [second]);
    assert!(r.notes.contains("### Known issues"));
}

#[test]
fn the_gate_is_the_stakeholders_and_waits_for_a_closed_round() {
    let (mut e, id) = testing();
    pass(&mut e, &id);
    let open = || Command::OpenReleaseGate { force: false };
    // The round is still open.
    assert!(try_run(&mut e, Actor::Stakeholder, open()).is_err());
    close(&mut e);
    // The release is already at the gate.
    assert!(try_run(&mut e, Actor::Stakeholder, open()).is_err());
    assert!(try_run(&mut e, dev("quinn"), open()).is_err());
}

#[test]
fn approving_takes_a_newer_valid_version_and_the_edited_notes() {
    let (mut e, _) = gated();
    assert!(approve(&mut e, "1.x").is_err(), "not a version");
    assert!(
        try_run(
            &mut e,
            dev("quinn"),
            Command::ApproveRelease {
                id: "R-1".into(),
                version: "v0.1.0".into(),
                notes: "n".into()
            }
        )
        .is_err(),
        "only the stakeholder"
    );
    assert!(
        try_run(
            &mut e,
            Actor::Stakeholder,
            Command::ApproveRelease {
                id: "R-1".into(),
                version: "v0.1.0".into(),
                notes: "  ".into()
            }
        )
        .is_err(),
        "notes are required"
    );
    approve(&mut e, "1.0.0").expect("approved");
    let r = release_of(&e);
    assert_eq!(r.version, "v1.0.0");
    assert_eq!(r.notes, "notes");
    assert_eq!(r.status, ReleaseStatus::Approved);
    assert!(
        !e.state
            .inbox
            .iter()
            .any(|i| matches!(i, InboxItem::ReleaseGate { .. })),
        "the gate item is resolved"
    );
    // Approved twice is refused.
    assert!(approve(&mut e, "1.0.1").is_err());
}

#[test]
fn a_rejected_release_can_be_proposed_again() {
    let (mut e, _) = gated();
    run(
        &mut e,
        Actor::Stakeholder,
        Command::RejectRelease {
            id: "R-1".into(),
            reason: "not yet".into(),
        },
    );
    assert_eq!(release_of(&e).status, ReleaseStatus::Rejected);
    run(
        &mut e,
        Actor::Stakeholder,
        Command::OpenReleaseGate { force: false },
    );
    assert_eq!(release_of(&e).id, "R-2");
    // The rejected version does not count against the new one.
    assert_eq!(release_of(&e).version, "v0.1.0");
}

#[test]
fn tagging_marks_the_work_released_and_the_sprint_with_it() {
    let (mut e, id) = gated();
    approve(&mut e, "v0.1.0").expect("approved");
    let record = |commit: &str| Command::RecordRelease {
        id: "R-1".into(),
        tag: "v0.1.0".into(),
        commit: commit.into(),
    };
    assert!(try_run(&mut e, Actor::Stakeholder, record("abc")).is_err());
    run(&mut e, Actor::System, record("abc1234"));
    let r = release_of(&e);
    assert_eq!(r.status, ReleaseStatus::Tagged);
    assert_eq!(r.tag.as_deref(), Some("v0.1.0"));
    assert!(
        e.state
            .ticket(&id)
            .expect("ticket")
            .checks
            .contains("released")
    );
    assert_eq!(
        e.state.sprint("SP-1").expect("sprint").status,
        SprintStatus::Released
    );
    // Tagged twice is refused.
    assert!(try_run(&mut e, Actor::System, record("abc1234")).is_err());
}

#[test]
fn a_failed_deploy_leaves_the_release_standing_and_can_be_retried() {
    let (mut e, _) = gated();
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
    let deploy = |ok| Command::RecordDeploy {
        id: "R-1".into(),
        ok,
        skipped: false,
        output_tail: "boom".into(),
    };
    let retry = || Command::RetryEnv {
        env: "prod".into(),
        step: EnvStep::Deploy,
    };
    // Nothing failed yet.
    assert!(try_run(&mut e, Actor::Stakeholder, retry()).is_err());
    run(&mut e, Actor::System, deploy(false));
    assert_eq!(release_of(&e).status, ReleaseStatus::DeployFailed);
    assert!(matches!(
        e.state.inbox.last(),
        Some(InboxItem::EnvAlert {
            step: EnvStep::Deploy,
            ..
        })
    ));

    run(&mut e, Actor::Stakeholder, retry());
    assert!(e.state.env_retry.is_some());
    assert!(e.state.inbox.is_empty(), "the alert it answers is resolved");
    run(&mut e, Actor::System, deploy(true));
    assert_eq!(release_of(&e).status, ReleaseStatus::Deployed);
    assert!(e.state.env_retry.is_none());
}

#[test]
fn a_failed_qa_environment_can_be_retried_by_the_stakeholder() {
    let (mut e, _) = in_qa();
    promote(&mut e);
    env_run(&mut e, EnvStep::Up, false);
    let retry = Command::RetryEnv {
        env: "qa".into(),
        step: EnvStep::Up,
    };
    assert!(try_run(&mut e, dev("quinn"), retry.clone()).is_err());
    run(&mut e, Actor::Stakeholder, retry);
    assert_eq!(
        e.state.env_retry,
        Some(EnvRetry {
            env: "qa".into(),
            step: EnvStep::Up
        })
    );
    env_run(&mut e, EnvStep::Up, true);
    assert!(e.state.env_retry.is_none());
    assert_eq!(round(&e).status, QaRoundStatus::Starting);
    assert!(round(&e).up_done);
}

#[test]
fn down_is_recorded_before_the_round_closes() {
    let (mut e, id) = testing();
    pass(&mut e, &id);
    env_run(&mut e, EnvStep::Down, true);
    close(&mut e);
    assert_eq!(round(&e).status, QaRoundStatus::Closed);
    // Up and test need an open round.
    assert!(
        try_run(
            &mut e,
            Actor::System,
            Command::RecordEnvRun {
                step: EnvStep::Up,
                ok: true,
                skipped: false,
                output_tail: String::new()
            }
        )
        .is_err()
    );
}

// ---- alerts ----

/// The id of the alert in the inbox, when there is exactly one.
fn alert_id(e: &Engine) -> String {
    match e.state.inbox.as_slice() {
        [InboxItem::Alert { id, .. }] => id.clone(),
        other => panic!("expected one alert, got {other:?}"),
    }
}

#[test]
fn chillax_raises_an_alert_once_and_the_stakeholder_dismisses_it() {
    let (mut e, _) = in_qa();
    let raise = || Command::RaiseAlert {
        title: "Could not promote dev to qa".into(),
        detail: "qa has diverged".into(),
    };
    assert!(try_run(&mut e, Actor::Stakeholder, raise()).is_err());
    assert!(try_run(&mut e, dev("quinn"), raise()).is_err());
    run(&mut e, Actor::System, raise());
    let id = alert_id(&e);
    // The same problem again adds nothing.
    assert!(run(&mut e, Actor::System, raise()).is_empty());
    assert_eq!(e.state.inbox.len(), 1);

    let dismiss = |id: &str| Command::DismissAlert { id: id.into() };
    assert!(try_run(&mut e, dev("quinn"), dismiss(&id)).is_err());
    assert!(try_run(&mut e, Actor::Stakeholder, dismiss("IN-99")).is_err());
    run(&mut e, Actor::Stakeholder, dismiss(&id));
    assert!(e.state.inbox.is_empty());
    // Once dismissed, the same problem can be raised again.
    run(&mut e, Actor::System, raise());
    assert_eq!(e.state.inbox.len(), 1);
}

#[test]
fn chillax_can_clear_an_alert_when_the_problem_goes_away() {
    let (mut e, _) = in_qa();
    run(
        &mut e,
        Actor::System,
        Command::RaiseAlert {
            title: "t".into(),
            detail: "d".into(),
        },
    );
    let id = alert_id(&e);
    run(&mut e, Actor::System, Command::DismissAlert { id });
    assert!(e.state.inbox.is_empty());
}

#[test]
fn an_approved_release_that_cannot_be_made_can_be_retried_or_rejected() {
    let (mut e, _) = gated();
    approve(&mut e, "v0.1.0").expect("approved");
    assert_eq!(release_of(&e).status, ReleaseStatus::Approved);
    let retry = Command::RetryEnv {
        env: "prod".into(),
        step: EnvStep::Deploy,
    };
    run(&mut e, Actor::Stakeholder, retry);
    assert!(e.state.env_retry.is_some());
    run(
        &mut e,
        Actor::Stakeholder,
        Command::RejectRelease {
            id: "R-1".into(),
            reason: "main has diverged".into(),
        },
    );
    assert_eq!(release_of(&e).status, ReleaseStatus::Rejected);
}

#[test]
fn a_tagged_release_can_no_longer_be_rejected() {
    let (mut e, _) = gated();
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
    let reject = Command::RejectRelease {
        id: "R-1".into(),
        reason: "too late".into(),
    };
    assert!(try_run(&mut e, Actor::Stakeholder, reject).is_err());
}

#[test]
fn a_pull_request_is_recorded_on_the_ticket_by_chillax_alone() {
    let (mut e, id) = in_qa();
    let record = Command::RecordPr {
        id: id.clone(),
        number: 7,
        url: "https://github.com/o/r/pull/7".into(),
    };
    assert!(try_run(&mut e, Actor::Stakeholder, record.clone()).is_err());
    run(&mut e, Actor::System, record);
    let ticket = e.state.ticket(&id).expect("ticket");
    assert_eq!(ticket.pr_number, Some(7));
    assert_eq!(
        ticket.pr_url.as_deref(),
        Some("https://github.com/o/r/pull/7")
    );
}
