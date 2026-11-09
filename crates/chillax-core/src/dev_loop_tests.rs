//! The dev loop's rules: requirement checks, review hand-offs, blockers and pausing.

use crate::command::Command;
use crate::engine::Engine;
use crate::error::CommandError;
use crate::event::{Actor, Event, EventKind};
use crate::model::*;
use crate::role::{Role, RoleCatalog};
use crate::workflow::Workflow;

pub(crate) fn engine() -> Engine {
    let roles = [
        ("project-manager", "manager"),
        ("fullstack-developer", "developer"),
        ("tech-lead", "reviewer"),
        ("qa-engineer", "qa"),
    ]
    .map(|(id, tag)| {
        Role::parse(&format!(
            "---\nid: {id}\ntitle: {id}\ntags: [{tag}]\n---\nprompt\n"
        ))
        .expect("valid role")
    });
    Engine::new("Acme", RoleCatalog::new(roles), Workflow::default())
}

pub(crate) fn try_run(
    e: &mut Engine,
    actor: Actor,
    cmd: Command,
) -> Result<Vec<EventKind>, CommandError> {
    let kinds = e.decide(&actor, cmd)?;
    for kind in &kinds {
        let event = Event {
            seq: e.state.last_seq + 1,
            ts: chrono::DateTime::from_timestamp(0, 0).expect("epoch is valid"),
            actor: actor.clone(),
            kind: kind.clone(),
        };
        e.apply(&event);
    }
    Ok(kinds)
}

pub(crate) fn run(e: &mut Engine, actor: Actor, cmd: Command) -> Vec<EventKind> {
    try_run(e, actor, cmd).expect("command accepted")
}

pub(crate) fn dev(id: &str) -> Actor {
    Actor::Employee(id.into())
}

pub(crate) fn hire(e: &mut Engine, role: &str, name: &str) {
    run(
        e,
        Actor::Stakeholder,
        Command::HireEmployee {
            role: role.into(),
            name: Some(name.into()),
            backend: None,
            model: None,
            effort: None,
            keep_defaults: false,
        },
    );
}

pub(crate) fn story(e: &mut Engine, title: &str) -> String {
    let kinds = run(
        e,
        dev("pam"),
        Command::CreateTicket {
            kind: TicketKind::Story,
            title: title.into(),
            description: String::new(),
            acceptance_criteria: vec!["works".into()],
            estimate: None,
            epic: None,
            external: None,
        },
    );
    match &kinds[0] {
        EventKind::TicketCreated { ticket } => ticket.id.clone(),
        other => panic!("expected a ticket, got {other:?}"),
    }
}

/// A team (Pam the PM, Dev, Lead), one approved-sprint ticket assigned to Dev, `ready`.
pub(crate) fn ready_ticket() -> (Engine, String) {
    let mut e = engine();
    hire(&mut e, "project-manager", "Pam");
    hire(&mut e, "fullstack-developer", "Dev");
    hire(&mut e, "tech-lead", "Lead");
    let id = story(&mut e, "Login");
    run(
        &mut e,
        dev("pam"),
        Command::ProposeSprint {
            name: "S1".into(),
            goal: String::new(),
            tickets: vec![id.clone()],
        },
    );
    run(
        &mut e,
        Actor::Stakeholder,
        Command::ApproveSprint { id: "SP-1".into() },
    );
    run(
        &mut e,
        dev("pam"),
        Command::AssignTicket {
            id: id.clone(),
            assignee: Some("dev".into()),
        },
    );
    (e, id)
}

pub(crate) fn start(e: &mut Engine, id: &str) {
    run(
        e,
        Actor::System,
        Command::StartWork {
            id: id.into(),
            branch: format!("feat/{id}-login"),
            worktree: format!(".chillax/worktrees/{id}"),
        },
    );
}

pub(crate) fn tests(e: &mut Engine, id: &str, passed: bool) {
    run(
        e,
        Actor::System,
        Command::RecordTests {
            id: id.into(),
            passed,
            skipped: false,
            output_tail: "output".into(),
        },
    );
}

pub(crate) fn state_of(e: &Engine, id: &str) -> String {
    e.state.ticket(id).expect("ticket").state.clone()
}

#[test]
fn work_starts_on_a_planned_ticket_with_a_branch() {
    let (mut e, id) = ready_ticket();
    assert_eq!(state_of(&e, &id), "ready");
    start(&mut e, &id);
    let t = e.state.ticket(&id).expect("ticket");
    assert_eq!(t.state, "in_progress");
    assert_eq!(t.branch.as_deref(), Some("feat/T-1-login"));
    assert_eq!(t.worktree.as_deref(), Some(".chillax/worktrees/T-1"));
}

#[test]
fn only_chillax_starts_work_and_unplanned_tickets_do_not_start() {
    let (mut e, id) = ready_ticket();
    let cmd = || Command::StartWork {
        id: id.clone(),
        branch: "b".into(),
        worktree: "w".into(),
    };
    assert!(try_run(&mut e, dev("dev"), cmd()).is_err());
    start(&mut e, &id);
    assert!(try_run(&mut e, Actor::System, cmd()).is_err());
}

#[test]
fn review_needs_passing_tests() {
    let (mut e, id) = ready_ticket();
    start(&mut e, &id);
    let start_review = || Command::StartReview {
        id: id.clone(),
        reviewer: "lead".into(),
    };
    assert!(matches!(
        try_run(&mut e, Actor::System, start_review()),
        Err(CommandError::RequirementsUnmet { missing, .. }) if missing == ["tests_pass"]
    ));
    tests(&mut e, &id, false);
    assert!(try_run(&mut e, Actor::System, start_review()).is_err());
    tests(&mut e, &id, true);
    run(&mut e, Actor::System, start_review());
    let t = e.state.ticket(&id).expect("ticket");
    assert_eq!(t.state, "in_review");
    assert_eq!(t.reviewer.as_deref(), Some("lead"));
}

#[test]
fn nobody_skips_the_loop_with_transition() {
    let (mut e, id) = ready_ticket();
    let to = |state: &str| Command::TransitionTicket {
        id: id.clone(),
        to: state.into(),
    };
    // Work starts through Chillax, so a hand-made move to in_progress is refused.
    for actor in [dev("dev"), Actor::Stakeholder] {
        assert!(matches!(
            try_run(&mut e, actor, to("in_progress")),
            Err(CommandError::Invalid(why)) if why.contains("starts work itself")
        ));
    }
    start(&mut e, &id);
    for actor in [dev("dev"), Actor::Stakeholder] {
        assert!(matches!(
            try_run(&mut e, actor, to("in_review")),
            Err(CommandError::Invalid(why)) if why.contains("request_review")
        ));
    }
}

#[test]
fn sending_a_ticket_back_by_hand_clears_the_stale_facts() {
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
        Actor::Stakeholder,
        Command::TransitionTicket {
            id: id.clone(),
            to: "in_progress".into(),
        },
    );
    let t = e.state.ticket(&id).expect("ticket");
    assert!(!t.checks.contains("tests_pass") && !t.checks.contains("review_approved"));
}

#[test]
fn no_one_reviews_their_own_work() {
    let (mut e, id) = ready_ticket();
    start(&mut e, &id);
    tests(&mut e, &id, true);
    let result = try_run(
        &mut e,
        Actor::System,
        Command::StartReview {
            id,
            reviewer: "dev".into(),
        },
    );
    assert!(result.is_err());
}

pub(crate) fn in_review() -> (Engine, String) {
    let (mut e, id) = ready_ticket();
    start(&mut e, &id);
    tests(&mut e, &id, true);
    run(
        &mut e,
        Actor::System,
        Command::StartReview {
            id: id.clone(),
            reviewer: "lead".into(),
        },
    );
    (e, id)
}

#[test]
fn approval_then_merge_lands_the_ticket_in_qa() {
    let (mut e, id) = in_review();
    run(
        &mut e,
        dev("lead"),
        Command::SubmitReview {
            id: id.clone(),
            verdict: ReviewVerdict::Approve,
            body: "lgtm".into(),
        },
    );
    assert_eq!(state_of(&e, &id), "in_review");
    run(
        &mut e,
        Actor::System,
        Command::RecordMerge {
            id: id.clone(),
            commit: "abcdef0123".into(),
        },
    );
    let t = e.state.ticket(&id).expect("ticket");
    assert_eq!(t.state, "in_qa");
    assert!(t.checks.contains("merged_to_dev"));
}

#[test]
fn a_merge_needs_an_approval() {
    let (mut e, id) = in_review();
    let result = try_run(
        &mut e,
        Actor::System,
        Command::RecordMerge {
            id,
            commit: "abc".into(),
        },
    );
    assert!(matches!(
        result,
        Err(CommandError::RequirementsUnmet { missing, .. }) if missing == ["review_approved"]
    ));
}

#[test]
fn requested_changes_send_the_ticket_back_and_clear_the_facts() {
    let (mut e, id) = in_review();
    run(
        &mut e,
        dev("lead"),
        Command::SubmitReview {
            id: id.clone(),
            verdict: ReviewVerdict::RequestChanges,
            body: "handle the empty case".into(),
        },
    );
    let t = e.state.ticket(&id).expect("ticket");
    assert_eq!(t.state, "in_progress");
    assert!(t.checks.is_empty());
    assert!(
        t.comments
            .last()
            .is_some_and(|c| c.body.contains("handle the empty case"))
    );
}

#[test]
fn only_the_reviewer_reviews_and_changes_need_feedback() {
    let (mut e, id) = in_review();
    let review = |verdict, body: &str| Command::SubmitReview {
        id: id.clone(),
        verdict,
        body: body.into(),
    };
    assert!(try_run(&mut e, dev("dev"), review(ReviewVerdict::Approve, "")).is_err());
    assert!(
        try_run(
            &mut e,
            dev("lead"),
            review(ReviewVerdict::RequestChanges, " ")
        )
        .is_err()
    );
}

#[test]
fn a_failed_merge_returns_the_ticket_to_the_developer() {
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
        Command::RecordMergeFailed {
            id: id.clone(),
            reason: "conflict in src/lib.rs".into(),
        },
    );
    let t = e.state.ticket(&id).expect("ticket");
    assert_eq!(t.state, "in_progress");
    assert!(!t.checks.contains("review_approved"));
}

#[test]
fn qa_stays_closed_until_qa_passes() {
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
            commit: "abc".into(),
        },
    );
    let result = try_run(
        &mut e,
        Actor::Stakeholder,
        Command::TransitionTicket {
            id,
            to: "done".into(),
        },
    );
    assert!(matches!(
        result,
        Err(CommandError::Invalid(why)) if why.contains("submit_qa")
    ));
}

#[test]
fn only_the_assignee_requests_a_review() {
    let (mut e, id) = ready_ticket();
    start(&mut e, &id);
    let request = || Command::RequestReview {
        id: id.clone(),
        summary: "done".into(),
    };
    assert!(try_run(&mut e, dev("lead"), request()).is_err());
    assert!(try_run(&mut e, Actor::Stakeholder, request()).is_err());
    assert_eq!(
        run(&mut e, dev("dev"), request()),
        [EventKind::ReviewRequested {
            id: id.clone(),
            employee: "dev".into(),
            summary: "done".into()
        }]
    );
}

#[test]
fn blockers_hold_work_back_until_they_are_merged() {
    let (mut e, first) = ready_ticket();
    let second = story(&mut e, "Logout");
    run(
        &mut e,
        dev("pam"),
        Command::SetBlockers {
            id: second.clone(),
            blocked_by: vec![first.clone()],
        },
    );
    let t = e.state.ticket(&second).expect("ticket").clone();
    assert!(!e.state.blockers_clear(&t));
    start(&mut e, &first);
    assert!(!e.state.blockers_clear(&t));
    tests(&mut e, &first, true);
    run(
        &mut e,
        Actor::System,
        Command::StartReview {
            id: first.clone(),
            reviewer: "lead".into(),
        },
    );
    run(
        &mut e,
        dev("lead"),
        Command::SubmitReview {
            id: first.clone(),
            verdict: ReviewVerdict::Approve,
            body: String::new(),
        },
    );
    run(
        &mut e,
        Actor::System,
        Command::RecordMerge {
            id: first,
            commit: "abc".into(),
        },
    );
    assert!(e.state.blockers_clear(&t));
}

#[test]
fn blockers_cannot_form_a_cycle() {
    let (mut e, first) = ready_ticket();
    let second = story(&mut e, "Logout");
    let block = |id: &str, by: &str| Command::SetBlockers {
        id: id.into(),
        blocked_by: vec![by.into()],
    };
    run(&mut e, dev("pam"), block(&second, &first));
    assert!(try_run(&mut e, dev("pam"), block(&first, &second)).is_err());
    assert!(try_run(&mut e, dev("pam"), block(&first, &first)).is_err());
    assert!(try_run(&mut e, dev("pam"), block(&first, "T-99")).is_err());
}

#[test]
fn handoff_goes_to_the_least_loaded_colleague_in_that_role() {
    let (mut e, id) = ready_ticket();
    hire(&mut e, "tech-lead", "Second Lead");
    let other = story(&mut e, "Other");
    run(
        &mut e,
        Actor::Stakeholder,
        Command::AssignTicket {
            id: other,
            assignee: Some("lead".into()),
        },
    );
    run(
        &mut e,
        dev("dev"),
        Command::Handoff {
            id: id.clone(),
            role: "tech-lead".into(),
            note: "please take over".into(),
        },
    );
    assert_eq!(
        e.state.ticket(&id).expect("ticket").assignee.as_deref(),
        Some("second-lead")
    );
}

#[test]
fn handoff_needs_the_current_owner_and_someone_in_the_role() {
    let (mut e, id) = ready_ticket();
    let handoff = |role: &str| Command::Handoff {
        id: id.clone(),
        role: role.into(),
        note: String::new(),
    };
    assert!(try_run(&mut e, dev("lead"), handoff("tech-lead")).is_err());
    assert!(try_run(&mut e, dev("dev"), handoff("qa-engineer")).is_err());
}

#[test]
fn pausing_is_the_stakeholders_call_and_idempotent() {
    let mut e = engine();
    assert!(try_run(&mut e, dev("pam"), Command::PauseDispatch).is_err());
    assert_eq!(
        run(&mut e, Actor::Stakeholder, Command::PauseDispatch),
        [EventKind::DispatchPaused]
    );
    assert!(e.state.dispatch_paused);
    assert!(run(&mut e, Actor::Stakeholder, Command::PauseDispatch).is_empty());
    run(&mut e, Actor::Stakeholder, Command::ResumeDispatch);
    assert!(!e.state.dispatch_paused);
}

#[test]
fn rebase_outcomes_mark_branches_that_fell_behind() {
    let (mut e, id) = ready_ticket();
    start(&mut e, &id);
    let rebase = |outcome| Command::RecordRebase {
        id: id.clone(),
        outcome,
        note: String::new(),
    };
    run(&mut e, Actor::System, rebase(RebaseOutcome::Deferred));
    assert!(e.state.ticket(&id).expect("ticket").needs_rebase);
    run(&mut e, Actor::System, rebase(RebaseOutcome::Rebased));
    assert!(!e.state.ticket(&id).expect("ticket").needs_rebase);
}

#[test]
fn the_live_view_follows_tool_calls_and_plans() {
    let mut e = engine();
    hire(&mut e, "fullstack-developer", "Dev");
    let events = [
        EventKind::AgentToolCall {
            employee: "dev".into(),
            tool_call_id: "c1".into(),
            title: "Edit src/lib.rs".into(),
            tool_kind: "edit".into(),
            status: "in_progress".into(),
        },
        EventKind::AgentToolCallUpdated {
            employee: "dev".into(),
            tool_call_id: "c1".into(),
            title: None,
            status: Some("completed".into()),
        },
        EventKind::AgentToolCallContent {
            employee: "dev".into(),
            tool_call_id: "c1".into(),
            content: vec![ToolContent::Text { text: "ok".into() }],
        },
        EventKind::AgentPlanUpdated {
            employee: "dev".into(),
            entries: vec![PlanEntry {
                content: "write it".into(),
                status: "pending".into(),
            }],
        },
    ];
    for kind in events {
        let event = Event {
            seq: e.state.last_seq + 1,
            ts: chrono::DateTime::from_timestamp(0, 0).expect("epoch is valid"),
            actor: Actor::System,
            kind,
        };
        e.apply(&event);
    }
    let live = &e.state.live["dev"];
    assert_eq!(live.tools[0].status, "completed");
    assert_eq!(live.tools[0].content.len(), 1);
    assert_eq!(live.plan[0].content, "write it");
}

#[test]
fn the_live_view_keeps_only_recent_tool_calls() {
    let mut e = engine();
    hire(&mut e, "fullstack-developer", "Dev");
    for n in 0..(LIVE_TOOLS_KEPT + 5) {
        let event = Event {
            seq: e.state.last_seq + 1,
            ts: chrono::DateTime::from_timestamp(0, 0).expect("epoch is valid"),
            actor: Actor::System,
            kind: EventKind::AgentToolCall {
                employee: "dev".into(),
                tool_call_id: format!("c{n}"),
                title: "Read".into(),
                tool_kind: "read".into(),
                status: String::new(),
            },
        };
        e.apply(&event);
    }
    let live = &e.state.live["dev"];
    assert_eq!(live.tools.len(), LIVE_TOOLS_KEPT);
    assert_eq!(live.tools[0].tool_call_id, "c5");
}

#[test]
fn tool_kind_is_stored_and_old_output_is_trimmed_by_size() {
    let mut e = engine();
    hire(&mut e, "fullstack-developer", "Dev");
    let send = |e: &mut Engine, kind: EventKind| {
        let event = Event {
            seq: e.state.last_seq + 1,
            ts: chrono::DateTime::from_timestamp(0, 0).expect("epoch is valid"),
            actor: Actor::System,
            kind,
        };
        e.apply(&event);
    };
    for n in 0..3 {
        send(
            &mut e,
            EventKind::AgentToolCall {
                employee: "dev".into(),
                tool_call_id: format!("c{n}"),
                title: "Run".into(),
                tool_kind: "execute".into(),
                status: "pending".into(),
            },
        );
        send(
            &mut e,
            EventKind::AgentToolCallContent {
                employee: "dev".into(),
                tool_call_id: format!("c{n}"),
                content: vec![ToolContent::Text {
                    text: "x".repeat(LIVE_CONTENT_BYTES / 2 + 1),
                }],
            },
        );
    }
    let live = &e.state.live["dev"];
    assert_eq!(live.tools[0].tool_kind, "execute");
    assert_eq!(live.tools.len(), 3, "the calls stay");
    assert!(live.tools[0].content.is_empty(), "oldest output goes first");
    assert!(live.tools[1].content.is_empty());
    assert!(!live.tools[2].content.is_empty());
}

#[test]
fn an_urgent_ticket_takes_the_only_slot_and_the_other_waits_for_it() {
    use crate::dispatch::{Action, Context, next_actions};
    let mut e = engine();
    hire(&mut e, "project-manager", "Pam");
    hire(&mut e, "fullstack-developer", "Dev");
    hire(&mut e, "fullstack-developer", "Dee");
    let first = story(&mut e, "First");
    let second = story(&mut e, "Second");
    run(
        &mut e,
        dev("pam"),
        Command::ProposeSprint {
            name: "S1".into(),
            goal: String::new(),
            tickets: vec![first.clone(), second.clone()],
        },
    );
    run(
        &mut e,
        Actor::Stakeholder,
        Command::ApproveSprint { id: "SP-1".into() },
    );
    for (id, who) in [(&first, "dev"), (&second, "dee")] {
        run(
            &mut e,
            dev("pam"),
            Command::AssignTicket {
                id: id.clone(),
                assignee: Some(who.into()),
            },
        );
    }
    run(
        &mut e,
        Actor::Stakeholder,
        Command::PrioritizeTicket {
            id: second.clone(),
            urgent: true,
        },
    );
    let one_slot = Context {
        max_concurrent: 1,
        ..Context::default()
    };
    let actions = next_actions(&e, &one_slot);
    assert_eq!(
        actions,
        [Action::StartWork {
            ticket: second.clone(),
            employee: "dee".into()
        }]
    );
    start(&mut e, &second);
    // The slot is taken: the earlier ticket waits, and nothing is pre-empted.
    let taken = Context {
        sessions_open: 1,
        ..one_slot
    };
    assert!(
        next_actions(&e, &taken)
            .iter()
            .all(|a| !matches!(a, Action::StartWork { .. }))
    );
    assert_eq!(state_of(&e, &first), "ready");
}

// ---- Conflicts with dev ----

fn approved() -> (Engine, String) {
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
    (e, id)
}

fn sync(e: &mut Engine, id: &str, outcome: SyncOutcome) -> Vec<EventKind> {
    run(
        e,
        Actor::System,
        Command::RecordSync {
            id: id.into(),
            outcome,
            files: vec!["src/lib.rs".into()],
            note: "boom".into(),
        },
    )
}

fn briefed(e: &mut Engine, id: &str) {
    run(
        e,
        Actor::System,
        Command::RecordConflictBriefed { id: id.into() },
    );
}

fn with_session(id: &str) -> crate::dispatch::Context {
    crate::dispatch::Context {
        max_concurrent: 3,
        ticket_sessions: [("dev".to_string(), id.to_string())].into(),
        ..Default::default()
    }
}

#[test]
fn a_merge_conflict_keeps_the_approval_and_the_ticket_in_review() {
    let (mut e, id) = approved();
    sync(&mut e, &id, SyncOutcome::Conflict);
    let t = e.state.ticket(&id).expect("ticket");
    assert_eq!(t.state, "in_review");
    assert!(t.checks.contains("review_approved"));
    assert!(!t.checks.contains("tests_pass"));
    assert_eq!(
        t.conflict,
        Some(MergeConflict {
            files: vec!["src/lib.rs".into()],
            attempts: 1,
            briefed: false,
        })
    );
    let last = &t.comments.last().expect("comment").body;
    assert!(
        last.contains("src/lib.rs") && last.contains("1/2"),
        "{last}"
    );
    assert!(last.contains("approval stands"), "{last}");
    assert!(e.state.failures.is_empty(), "no triage yet");
}

#[test]
fn a_resolved_conflict_is_tested_and_merged_without_a_new_review() {
    use crate::dispatch::{Action, next_actions};
    let (mut e, id) = approved();
    sync(&mut e, &id, SyncOutcome::Conflict);
    let ctx = crate::dispatch::Context {
        max_concurrent: 3,
        ..Default::default()
    };
    assert_eq!(
        next_actions(&e, &ctx),
        [Action::ResolveConflict {
            ticket: id.clone(),
            employee: "dev".into()
        }]
    );
    briefed(&mut e, &id);
    assert!(next_actions(&e, &with_session(&id)).is_empty());
    run(
        &mut e,
        dev("dev"),
        Command::RequestReview {
            id: id.clone(),
            summary: "merged dev".into(),
        },
    );
    assert_eq!(
        next_actions(&e, &with_session(&id)),
        [Action::RunTests { ticket: id.clone() }]
    );
    tests(&mut e, &id, true);
    let t = e.state.ticket(&id).expect("ticket");
    assert_eq!(t.state, "in_review");
    assert!(t.conflict.is_none());
    assert_eq!(
        next_actions(&e, &with_session(&id)),
        [Action::Merge { ticket: id.clone() }]
    );
    run(
        &mut e,
        Actor::System,
        Command::RecordMerge {
            id: id.clone(),
            commit: "abc1234".into(),
        },
    );
    assert_eq!(state_of(&e, &id), "in_qa");
}

#[test]
fn a_conflicted_ticket_gets_a_fresh_session_once_briefed_and_never_merges() {
    use crate::dispatch::{Action, SessionKind, next_actions};
    let (mut e, id) = approved();
    sync(&mut e, &id, SyncOutcome::Conflict);
    briefed(&mut e, &id);
    let ctx = crate::dispatch::Context {
        max_concurrent: 3,
        ..Default::default()
    };
    assert_eq!(
        next_actions(&e, &ctx),
        [Action::OpenSession {
            ticket: id.clone(),
            employee: "dev".into(),
            kind: SessionKind::Work,
        }]
    );
}

#[test]
fn failed_tests_after_a_resolution_send_the_ticket_back_for_rework() {
    let (mut e, id) = approved();
    sync(&mut e, &id, SyncOutcome::Conflict);
    briefed(&mut e, &id);
    tests(&mut e, &id, false);
    let t = e.state.ticket(&id).expect("ticket");
    assert_eq!(t.state, "in_progress");
    assert!(t.checks.is_empty());
    assert!(t.conflict.is_none());
}

#[test]
fn a_third_conflict_is_triaged_like_a_failed_merge() {
    let (mut e, id) = approved();
    sync(&mut e, &id, SyncOutcome::Conflict);
    briefed(&mut e, &id);
    sync(&mut e, &id, SyncOutcome::Unresolved);
    let t = e.state.ticket(&id).expect("ticket");
    assert_eq!(
        t.conflict.as_ref().map(|c| (c.attempts, c.briefed)),
        Some((2, false))
    );
    assert_eq!(t.state, "in_review");
    briefed(&mut e, &id);
    let kinds = sync(&mut e, &id, SyncOutcome::Conflict);
    assert!(
        kinds
            .iter()
            .any(|k| matches!(k, EventKind::MergeFailed { .. })),
        "{kinds:?}"
    );
    let t = e.state.ticket(&id).expect("ticket");
    assert_eq!(t.state, "in_progress");
    assert!(!t.checks.contains("review_approved"));
    assert!(t.conflict.is_none());
    assert_eq!(e.state.failures.len(), 1);
    assert_eq!(e.state.failures[0].class, crate::FailureClass::Merge);
}

#[test]
fn tests_failing_after_a_sync_send_the_ticket_back_without_its_approval() {
    let (mut e, id) = approved();
    sync(&mut e, &id, SyncOutcome::TestsFailed);
    let t = e.state.ticket(&id).expect("ticket");
    assert_eq!(t.state, "in_progress");
    assert!(t.checks.is_empty());
    assert!(t.conflict.is_none());
    assert_eq!(e.state.test_streaks.get(&id), Some(&1));
}

#[test]
fn a_clean_sync_keeps_the_ticket_ready_to_merge() {
    let (mut e, id) = approved();
    sync(&mut e, &id, SyncOutcome::Merged);
    let t = e.state.ticket(&id).expect("ticket");
    assert_eq!(t.state, "in_review");
    assert!(t.checks.contains("review_approved") && t.checks.contains("tests_pass"));
}

#[test]
fn a_rebase_conflict_gets_a_resolve_turn_not_triage() {
    use crate::dispatch::{Action, next_actions};
    let (mut e, id) = ready_ticket();
    start(&mut e, &id);
    run(
        &mut e,
        Actor::System,
        Command::RecordRebase {
            id: id.clone(),
            outcome: RebaseOutcome::Conflict,
            note: "conflicts in src/lib.rs".into(),
        },
    );
    sync(&mut e, &id, SyncOutcome::Conflict);
    assert!(e.state.failures.is_empty());
    let t = e.state.ticket(&id).expect("ticket");
    assert_eq!(t.state, "in_progress");
    assert!(!t.needs_rebase);
    assert_eq!(
        next_actions(&e, &with_session(&id)),
        [Action::ResolveConflict {
            ticket: id.clone(),
            employee: "dev".into()
        }]
    );
    briefed(&mut e, &id);
    assert!(next_actions(&e, &with_session(&id)).is_empty());
    // Past the limit it is triaged as a rebase failure and the developer carries on.
    sync(&mut e, &id, SyncOutcome::Unresolved);
    sync(&mut e, &id, SyncOutcome::Unresolved);
    let t = e.state.ticket(&id).expect("ticket");
    assert!(t.conflict.is_none());
    assert_eq!(e.state.failures.len(), 1);
    assert_eq!(e.state.failures[0].class, crate::FailureClass::Rebase);
}

#[test]
fn a_conflicted_assignee_in_review_is_waiting_until_they_ask_for_tests() {
    use crate::dispatch::session_is_waiting;
    let (mut e, id) = approved();
    assert!(!session_is_waiting(&e, "dev", &id));
    sync(&mut e, &id, SyncOutcome::Conflict);
    assert!(session_is_waiting(&e, "dev", &id));
    run(
        &mut e,
        dev("dev"),
        Command::RequestReview {
            id: id.clone(),
            summary: String::new(),
        },
    );
    assert!(!session_is_waiting(&e, "dev", &id));
}

#[test]
fn an_unconflicted_ticket_in_review_takes_no_review_request_or_tests() {
    let (mut e, id) = approved();
    let request = Command::RequestReview {
        id: id.clone(),
        summary: String::new(),
    };
    assert!(try_run(&mut e, dev("dev"), request).is_err());
    let tested = Command::RecordTests {
        id,
        passed: true,
        skipped: false,
        output_tail: String::new(),
    };
    assert!(try_run(&mut e, Actor::System, tested).is_err());
}

#[test]
fn a_resolution_found_after_the_turn_requests_the_tests() {
    let (mut e, id) = approved();
    sync(&mut e, &id, SyncOutcome::Conflict);
    briefed(&mut e, &id);
    sync(&mut e, &id, SyncOutcome::Resolved);
    let t = e.state.ticket(&id).expect("ticket");
    assert!(t.review_requested);
    assert_eq!(t.conflict.as_ref().map(|c| c.attempts), Some(1));
}

#[test]
fn tickets_logged_before_conflicts_existed_load_without_one() {
    let (e, id) = approved();
    let mut json = serde_json::to_value(e.state.ticket(&id).expect("ticket")).expect("json");
    assert!(json.get("conflict").is_none());
    json.as_object_mut().expect("object").remove("conflict");
    let t: Ticket = serde_json::from_value(json).expect("ticket");
    assert!(t.conflict.is_none());
}
