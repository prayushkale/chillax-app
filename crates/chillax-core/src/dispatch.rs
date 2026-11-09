//! The dispatcher's brain: given the company state, which actions should
//! Chillax take next? Pure, so every rule is unit-tested without IO. The
//! `chillax-dispatch` crate performs the actions and records what happened.

use std::collections::{BTreeMap, BTreeSet};

use crate::engine::Engine;
use crate::model::*;
use crate::workflow::{IN_PROGRESS, IN_QA, IN_REVIEW};

/// Which kind of ticket session an employee needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SessionKind {
    /// Build the ticket in its worktree.
    Work,
    /// Review the ticket's branch.
    Review,
    /// Test the ticket against its acceptance criteria in the shared `qa` worktree.
    Qa,
}

impl SessionKind {
    /// The environment whose env files the session gets.
    pub fn stage(self) -> &'static str {
        match self {
            SessionKind::Qa => "qa",
            SessionKind::Work | SessionKind::Review => "dev",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Create the branch and worktree, then move the ticket to `in_progress`.
    StartWork {
        ticket: TicketId,
        employee: EmployeeId,
    },
    /// The assignee's role cannot build the ticket: give it to `to`, a developer with
    /// room. The next pass starts the work.
    Reassign { ticket: TicketId, to: EmployeeId },
    /// A ready ticket nobody can build; tell the stakeholder (deduplicated by the engine).
    RaiseAlert {
        ticket: TicketId,
        title: String,
        detail: String,
    },
    /// Make sure `employee` has an agent session in the ticket's worktree.
    OpenSession {
        ticket: TicketId,
        employee: EmployeeId,
        kind: SessionKind,
    },
    /// Commit the worktree and run the project's tests.
    RunTests { ticket: TicketId },
    /// Tests passed: hand the ticket to a reviewer.
    StartReview {
        ticket: TicketId,
        reviewer: EmployeeId,
    },
    /// The review approved: merge the branch into `dev`.
    Merge { ticket: TicketId },
    /// Rebase an open branch onto `dev`.
    Rebase { ticket: TicketId },
    /// The ticket's branch conflicts with `dev`: give its assignee a focused turn to merge
    /// `dev` and resolve it, opening their session if needed.
    ResolveConflict {
        ticket: TicketId,
        employee: EmployeeId,
    },
    /// Advance `qa` to `dev` and start a QA round.
    Promote,
    /// Run one command of the QA environment (`up` or `test`).
    RunEnv { step: EnvStep },
    /// Every ticket of the round has a verdict: run `down` and close it.
    CloseRound,
    /// The release was approved: merge `qa` into the prod branch, tag, back-merge.
    Release { id: String },
    /// Run the prod deploy command of a tagged release.
    Deploy { id: String },
    /// Brief the project manager `pm` about a failure so they file a bug for the Tech
    /// Lead. Without a `pm` the failure has been tried too often: give up.
    TriageFailure {
        /// The claim key, `@triage-<fingerprint>`.
        key: String,
        fingerprint: String,
        pm: Option<EmployeeId>,
    },
    /// Brief the project manager `pm` to propose the next sprint.
    PlanSprint { pm: EmployeeId },
    /// Brief the project manager `pm` to file and assign a stakeholder request, or
    /// (`pm: None`) give up on one briefed too often.
    TriageRequest {
        key: String,
        id: RequestId,
        pm: Option<EmployeeId>,
    },
    /// Brief the project manager `pm` with the board digest so they keep the sprint on
    /// track and staff it.
    PmCheckin { pm: EmployeeId },
    /// Brief the project manager `pm` with the problems the dispatcher deferred instead of
    /// alerting the stakeholder (ADR 0032).
    PmHandleProblem { pm: EmployeeId },
    /// A session's turn hangs while the board is stalled: close it so its slot and its
    /// employee are free again. A ticket session reopens with a fresh briefing.
    CloseHungSession {
        /// The claim key, from [`hung_key`].
        key: String,
        employee: EmployeeId,
        ticket: Option<TicketId>,
    },
    /// The board has not moved for `[team] stall_minutes`: brief the project manager `pm`
    /// with why each open ticket is idle so he gets it moving.
    PmUnstick { pm: EmployeeId },
    /// A stall the project manager's briefing did not end: tell the stakeholder once.
    StallAlert,
    /// Start the app of `env` on `commit`, or restart it when it runs something else
    /// (ADR 0015). `why` is the reason recorded with a restart.
    RunApp {
        env: String,
        commit: String,
        why: String,
    },
    /// Stop the app of `env`, recording `reason` as why it is not running.
    StopApp { env: String, reason: String },
}

/// Claim keys of the actions that are not about one ticket.
pub const KEY_PROMOTE: &str = "@promote";
pub const KEY_QA_ENV: &str = "@qa-env";
pub const KEY_QA_CLOSE: &str = "@qa-close";
pub const KEY_RELEASE: &str = "@release";
pub const KEY_DEPLOY: &str = "@deploy";
pub const KEY_PLAN: &str = "@plan";
pub const KEY_CHECKIN: &str = "@checkin";
pub const KEY_PROBLEM: &str = "@problem";
pub const KEY_STALL: &str = "@stall";
pub const KEY_STALL_ALERT: &str = "@stall-alert";

/// The claim key of closing one hung session.
pub fn hung_key(employee: &str, ticket: Option<&str>) -> String {
    format!("@hung-{employee}-{}", ticket.unwrap_or("chat"))
}

/// The claim key of an app's actions: one action per app at a time.
pub fn app_claim(env: &str) -> &'static str {
    match env {
        "dev" => "@app-dev",
        "qa" => "@app-qa",
        "prod" => "@app-prod",
        _ => "@app-other",
    }
}

impl Action {
    /// What the action works on: a ticket id, or one of the `KEY_*` claims. The
    /// dispatcher runs at most one action per key at a time.
    pub fn ticket(&self) -> &str {
        match self {
            Self::StartWork { ticket, .. }
            | Self::Reassign { ticket, .. }
            | Self::RaiseAlert { ticket, .. }
            | Self::OpenSession { ticket, .. }
            | Self::RunTests { ticket }
            | Self::StartReview { ticket, .. }
            | Self::Merge { ticket }
            | Self::Rebase { ticket }
            | Self::ResolveConflict { ticket, .. } => ticket,
            Self::Promote => KEY_PROMOTE,
            Self::RunEnv { .. } => KEY_QA_ENV,
            Self::CloseRound => KEY_QA_CLOSE,
            Self::Release { .. } => KEY_RELEASE,
            Self::Deploy { .. } => KEY_DEPLOY,
            Self::TriageFailure { key, .. } => key,
            Self::PlanSprint { .. } => KEY_PLAN,
            Self::PmCheckin { .. } => KEY_CHECKIN,
            Self::PmHandleProblem { .. } => KEY_PROBLEM,
            Self::CloseHungSession { key, .. } => key,
            Self::PmUnstick { .. } => KEY_STALL,
            Self::StallAlert => KEY_STALL_ALERT,
            Self::TriageRequest { key, .. } => key,
            Self::RunApp { env, .. } | Self::StopApp { env, .. } => app_claim(env),
        }
    }
}

/// The claim key of the triage of one failure.
pub fn triage_key(fingerprint: &str) -> String {
    format!("@triage-{fingerprint}")
}

/// What the runner remembers about the project manager's board check-ins.
/// The decision has no clock, so the runner says whether the interval has passed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckinCtx {
    /// The check-in interval has passed since the last one.
    pub due: bool,
    /// The board's fingerprint when the last check-in went out.
    pub last_fingerprint: Option<String>,
    /// Intervals in a row that came due with the board unchanged.
    pub unchanged_intervals: u32,
}

/// What the runner remembers about problems it deferred to the project manager
/// (ADR 0032). The decision has no clock, so the runner says whether a briefing is due.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProblemCtx {
    /// A briefing is due: a problem was never briefed, or its retry interval passed.
    pub due: bool,
    /// The deferred problems and how far each one got.
    pub notes: Vec<crate::problems::ProblemNote>,
    /// Problems already handed to the stakeholder; the PM hears no more of them.
    pub escalated: BTreeSet<String>,
}

/// What the runtime knows that the event log does not.
#[derive(Debug, Clone, Default)]
pub struct Context {
    /// Deferred problems; `None` when deferral is off (`[team] problem_grace_minutes = 0`).
    pub problem: Option<ProblemCtx>,
    /// The project manager's check-in memory; `None` when check-ins are off
    /// (`[team] checkin_minutes = 0`).
    pub checkin: Option<CheckinCtx>,
    /// `[budget] max_concurrent_agents`.
    pub max_concurrent: usize,
    /// Ticket sessions open now. Chat sessions are not counted: they have their own cap,
    /// so a conversation never holds back a developer.
    pub sessions_open: usize,
    /// Backends that failed with `auth_required`: nothing starts on them until the
    /// stakeholder signs in and refreshes. Usage limits come from `State::backend_limits`.
    pub halted_backends: BTreeSet<String>,
    /// Failed employees still in their failure cooldown. Once an employee leaves this set a
    /// `Failed` status no longer blocks new work: the next action opens a fresh session,
    /// which is what clears the status.
    pub cooling_employees: BTreeSet<EmployeeId>,
    /// Ticket sessions that are open, as (employee, ticket).
    pub ticket_sessions: BTreeSet<(EmployeeId, TicketId)>,
    /// Tickets with an action still running; they get no new action.
    pub busy: BTreeSet<TicketId>,
    /// What each configured app should run, as found in git (ADR 0015).
    pub app_targets: BTreeMap<String, crate::apps::AppTarget>,
    /// The commit each app's supervised process runs, for apps with a process (or a
    /// pending crash retry) right now.
    pub app_live: BTreeMap<String, String>,
    /// `[vcs] worktrees = "shared"` (ADR 0025): ticket work happens on a branch in the one
    /// dev worktree, so only one ticket may hold it at a time.
    pub shared_dev_worktree: bool,
    /// Tickets left alone after a failed action, until their cooldown ends (also in
    /// `busy`); only [`why_idle`] tells them apart from running actions.
    pub cooling_tickets: BTreeSet<TicketId>,
    /// The stall watchdog's window; `None` when it is off (`[team] stall_minutes = 0`).
    pub stall: Option<crate::stall::StallCtx>,
}

/// The ticket that holds the shared dev worktree: one being built or reviewed and not yet
/// merged, or a planned one whose start is running right now. Per-ticket mode has none.
fn dev_worktree_holder<'a>(engine: &'a Engine, ctx: &Context) -> Option<&'a TicketId> {
    if !ctx.shared_dev_worktree {
        return None;
    }
    let planned = engine.workflow.planned_state();
    engine
        .state
        .tickets
        .iter()
        .find(|t| {
            t.dropped.is_none()
                && match t.state.as_str() {
                    IN_PROGRESS => true,
                    IN_REVIEW => t.branch.is_some() && !t.checks.contains("merged_to_dev"),
                    s => Some(s) == planned && t.assignee.is_some() && ctx.busy.contains(&t.id),
                }
        })
        .map(|t| &t.id)
}

/// Whether `employee`'s backend can take new work: not signed out, not rate limited.
fn backend_usable(engine: &Engine, ctx: &Context, employee: &str) -> bool {
    engine.state.employee(employee).is_none_or(|e| {
        let backend = &e.employee.backend;
        !ctx.halted_backends.contains(backend) && !engine.state.backend_limits.contains_key(backend)
    })
}

/// Whether `employee` may be given new work: a usable backend, and the stakeholder has
/// not stopped them. A held employee keeps their tickets; they just get no
/// new session or briefing until the stakeholder writes to them again.
fn employee_free(engine: &Engine, ctx: &Context, employee: &str) -> bool {
    backend_usable(engine, ctx, employee) && !held(engine, employee)
}

/// The stakeholder stopped this employee's turn and has not messaged them since.
pub fn held(engine: &Engine, employee: &str) -> bool {
    engine.state.employee(employee).is_some_and(|e| e.stopped)
}

/// Whether a ticket still needs `employee`'s session to act, so a turn that ended
/// without it is a stuck turn: a developer who has not requested review (or whose tests
/// failed), a reviewer without a verdict, a tester without one. An employee who asked
/// the stakeholder something is waiting on a person, not stuck.
pub fn session_is_waiting(engine: &Engine, employee: &str, ticket: &str) -> bool {
    let state = &engine.state;
    let Some(t) = state.ticket(ticket) else {
        return false;
    };
    let asked = state.inbox.iter().any(|item| {
        matches!(item, InboxItem::Question { employee: e, .. }
            | InboxItem::Permission { employee: e, .. } if e == employee)
    });
    if asked {
        return false;
    }
    // Resolving a conflict, in progress or in review: done once they ask for
    // the tests.
    if t.assignee.as_deref() == Some(employee) && t.conflict.is_some() {
        return !t.review_requested;
    }
    if t.assignee.as_deref() == Some(employee) && t.state == IN_PROGRESS {
        return !t.review_requested && !t.checks.contains("tests_pass");
    }
    if t.reviewer.as_deref() == Some(employee) && t.state == IN_REVIEW {
        return !t.checks.contains("review_approved");
    }
    t.state == IN_QA
        && is_tester(engine, employee)
        && state.open_round().is_some_and(|round| {
            round.status == QaRoundStatus::Testing
                && round.tickets.contains(&t.id)
                && !round.verdicts.contains_key(&t.id)
        })
}

/// Why an open ticket is not moving right now, in a few words for the project manager.
/// It follows the same guards as [`next_actions`]; when none of them holds,
/// the ticket is simply waiting for the next pass or for its agent to finish.
pub fn why_idle(engine: &Engine, ctx: &Context, ticket: &Ticket) -> String {
    let state = &engine.state;
    let name = |id: &str| {
        state
            .employee(id)
            .map_or(id.to_string(), |e| e.employee.name.clone())
    };
    if ticket.dropped.is_some() {
        return "dropped".into();
    }
    if state.is_finished(ticket) {
        return "done".into();
    }
    if ctx.cooling_tickets.contains(&ticket.id) {
        return format!(
            "an action on it failed and it is cooling down (retry_action with key {} retries now)",
            ticket.id
        );
    }
    if ctx.busy.contains(&ticket.id) {
        return "Chillax is running an action on it".into();
    }
    match ticket.sprint.as_deref().and_then(|sp| state.sprint(sp)) {
        None => return "not in a sprint".into(),
        Some(sp) if sp.status != SprintStatus::Active => {
            let status = format!("{:?}", sp.status).to_lowercase();
            return format!("its sprint {} is {status}, not active", sp.id);
        }
        Some(_) => {}
    }
    let planned = engine.workflow.planned_state();
    let touches_dev = Some(ticket.state.as_str()) == planned
        || matches!(ticket.state.as_str(), IN_PROGRESS | IN_REVIEW);
    if touches_dev
        && let Some(holder) = dev_worktree_holder(engine, ctx).filter(|h| **h != ticket.id)
    {
        return format!("waiting for the shared dev worktree, held by {holder}");
    }
    let slots_full = ctx.sessions_open >= ctx.max_concurrent;
    let slots = || {
        format!(
            "all {} agent slots are in use (max_concurrent_agents)",
            ctx.max_concurrent
        )
    };
    let has_session = |who: &str| {
        ctx.ticket_sessions
            .contains(&(who.to_string(), ticket.id.clone()))
    };
    if Some(ticket.state.as_str()) == planned {
        let Some(who) = ticket.assignee.as_deref() else {
            return "no assignee".into();
        };
        let Some(e) = state.employee(who) else {
            return format!("its assignee {who} is no longer on the team");
        };
        if !state.blockers_clear(ticket) {
            let open: Vec<&str> = ticket
                .blocked_by
                .iter()
                .filter(|b| !state.blocker_closed(b))
                .map(String::as_str)
                .collect();
            return format!("blocked by {}", open.join(", "));
        }
        let Some(role) = engine.roles.get(&e.employee.role) else {
            return format!("{}'s role {} is unknown", e.employee.name, e.employee.role);
        };
        if !engine.workflow.role_owns(role, IN_PROGRESS) {
            return format!(
                "{} ({}) cannot build it and no developer has room",
                e.employee.name, role.id
            );
        }
        if let Some(reason) = employee_reason(engine, ctx, who) {
            return reason;
        }
        let building: Vec<&str> = state
            .tickets
            .iter()
            .filter(|t| t.state == IN_PROGRESS && t.assignee.as_deref() == Some(who))
            .map(|t| t.id.as_str())
            .collect();
        if building.len() >= role.max_parallel_tasks as usize {
            return format!(
                "{} is at max_parallel_tasks ({}), busy on {}",
                e.employee.name,
                role.max_parallel_tasks,
                building.join(", ")
            );
        }
        if slots_full {
            return slots();
        }
        return "ready to start on the next pass".into();
    }
    match ticket.state.as_str() {
        IN_PROGRESS | IN_REVIEW if ticket.conflict.is_some() || ticket.state == IN_PROGRESS => {
            let Some(who) = ticket.assignee.as_deref() else {
                return "no assignee".into();
            };
            if ticket.review_requested {
                return "its tests are about to run".into();
            }
            if let Some(conflict) = &ticket.conflict {
                let doing = format!(
                    "conflicts with dev; {} is resolving it (attempt {}/{})",
                    name(who),
                    conflict.attempts,
                    crate::engine::MAX_CONFLICT_ATTEMPTS
                );
                return match employee_reason(engine, ctx, who) {
                    Some(reason) => format!("{doing}, but {reason}"),
                    None if !has_session(who) && slots_full => format!("{doing}, but {}", slots()),
                    None => doing,
                };
            }
            if ticket.checks.contains("tests_pass") {
                return "tests passed; waiting for a free reviewer".into();
            }
            if ticket.branch.is_none() {
                return "in progress without a branch".into();
            }
            if ticket.needs_rebase && !is_quiet(state, ctx, who) {
                return format!("waits to rebase onto dev until {} is idle", name(who));
            }
            busy_or_blocked(engine, ctx, who, has_session(who), slots_full, &slots)
        }
        IN_REVIEW => {
            if ticket.checks.contains("review_approved") {
                return if ticket.checks.contains("merged_to_dev") {
                    "approved and merged to dev; waiting for a QA round".into()
                } else {
                    "approved; waiting its turn to merge into dev".into()
                };
            }
            let Some(who) = ticket.reviewer.as_deref() else {
                return "waiting for a reviewer".into();
            };
            busy_or_blocked(engine, ctx, who, has_session(who), slots_full, &slots)
        }
        IN_QA => match state.open_round() {
            Some(round) if round.tickets.contains(&ticket.id) => match round.status {
                QaRoundStatus::Starting => "the QA environment is starting".into(),
                QaRoundStatus::EnvFailed => "the QA environment failed; waiting for a retry".into(),
                _ if round.verdicts.contains_key(&ticket.id) => {
                    "tested; waiting for the QA round to close".into()
                }
                _ => "waiting for a QA verdict".into(),
            },
            Some(_) => "waiting for the running QA round to end".into(),
            None => "waiting for a QA round (or for the fix of its failed QA)".into(),
        },
        s if Some(s) == Some(engine.workflow.initial_state()) => {
            format!("in {s}: not planned for work yet")
        }
        s => format!("in {s}"),
    }
}

/// Why `employee` cannot be given new work now, if anything stops them.
fn employee_reason(engine: &Engine, ctx: &Context, employee: &str) -> Option<String> {
    let state = &engine.state;
    let e = state.employee(employee)?;
    let name = &e.employee.name;
    let backend = &e.employee.backend;
    if held(engine, employee) {
        return Some(format!("{name} was stopped by the stakeholder"));
    }
    if ctx.halted_backends.contains(backend) {
        return Some(format!("{name}'s backend {backend} is signed out"));
    }
    if state.backend_limits.contains_key(backend) {
        return Some(format!("{name}'s backend {backend} hit its usage limit"));
    }
    if state.inbox.iter().any(|item| {
        matches!(item, InboxItem::Question { employee: x, .. }
            | InboxItem::Permission { employee: x, .. } if x == employee)
    }) {
        return Some(format!("{name} waits on the stakeholder's answer"));
    }
    if e.status == EmployeeStatus::Failed && ctx.cooling_employees.contains(employee) {
        return Some(format!("{name}'s agent failed and is cooling down"));
    }
    None
}

/// The usual reason a ticket waits on `who`'s session: working, blocked, or not open.
fn busy_or_blocked(
    engine: &Engine,
    ctx: &Context,
    who: &str,
    has_session: bool,
    slots_full: bool,
    slots: &dyn Fn() -> String,
) -> String {
    let state = &engine.state;
    let name = state
        .employee(who)
        .map_or(who.to_string(), |e| e.employee.name.clone());
    if state.employee(who).is_none() {
        return format!("{who} is no longer on the team");
    }
    if let Some(reason) = employee_reason(engine, ctx, who) {
        return reason;
    }
    let working = state
        .employee(who)
        .is_some_and(|e| e.status == EmployeeStatus::Working || e.turn_open);
    match (has_session, working) {
        (true, true) => format!("{name} is working on it"),
        (true, false) => {
            format!("{name}'s session is open but idle (turn ended without finishing)")
        }
        (false, _) if slots_full => slots(),
        (false, true) => format!("{name} is busy elsewhere; a session opens once they are free"),
        (false, false) => "a session opens on the next pass".into(),
    }
}

/// The order the dispatcher walks tickets in: urgent ones first, then
/// creation order. The sort is stable, so urgent tickets keep their creation order among
/// themselves. It only decides who claims the free slots first; sprint approval and
/// blockers still gate every ticket.
pub(crate) fn ticket_order(state: &crate::engine::State) -> Vec<&Ticket> {
    let mut order: Vec<&Ticket> = state.tickets.iter().collect();
    order.sort_by_key(|t| !t.urgent);
    order
}

/// The actions to take now, urgent tickets first, then oldest first. Calling again after the actions ran
/// (or while they run, with `busy` set) never repeats work.
pub fn next_actions(engine: &Engine, ctx: &Context) -> Vec<Action> {
    let state = &engine.state;
    if state.dispatch_paused {
        // Apps are not agent work: they keep following the code while paused.
        let mut actions = Vec::new();
        crate::apps::app_actions(engine, ctx, &mut actions);
        return actions;
    }
    let planned = engine.workflow.planned_state();
    let mut actions = Vec::new();
    let mut sessions = ctx.sessions_open;
    let mut merging = false;
    let mut holder = dev_worktree_holder(engine, ctx).cloned();
    // Work and review load per employee, including what this call hands out.
    let mut building = load(state, |t| {
        (t.state == IN_PROGRESS)
            .then(|| t.assignee.clone())
            .flatten()
    });
    let mut reviewing = load(state, |t| {
        (t.state == IN_REVIEW).then(|| t.reviewer.clone()).flatten()
    });

    for ticket in ticket_order(state) {
        if ctx.busy.contains(&ticket.id) || ticket.dropped.is_some() {
            continue;
        }
        let id = ticket.id.clone();
        let sprint_active = ticket
            .sprint
            .as_deref()
            .and_then(|sp| state.sprint(sp))
            .is_some_and(|sp| sp.status == SprintStatus::Active);
        let has_room = |sessions: usize| sessions < ctx.max_concurrent;
        // Shared mode: everything that touches the dev worktree waits for its holder.
        if holder.as_ref().is_some_and(|h| *h != ticket.id)
            && matches!(ticket.state.as_str(), IN_PROGRESS | IN_REVIEW)
        {
            continue;
        }

        if Some(ticket.state.as_str()) == planned {
            let Some(employee) = &ticket.assignee else {
                continue;
            };
            let Some(role) = engine
                .state
                .employee(employee)
                .and_then(|e| engine.roles.get(&e.employee.role))
            else {
                continue;
            };
            if sprint_active
                && holder.is_none()
                && state.blockers_clear(ticket)
                && !engine.workflow.role_owns(role, IN_PROGRESS)
            {
                // Its assignee's role cannot build it, so it would sit here forever.
                match pick_builder(engine, ctx, &building, false) {
                    Some(to) => {
                        *building.entry(to.clone()).or_default() += 1;
                        actions.push(Action::Reassign { ticket: id, to });
                    }
                    // Developers exist but are full: the ticket waits for one.
                    None if engine.state.employees.iter().any(|e| {
                        engine
                            .roles
                            .get(&e.employee.role)
                            .is_some_and(|r| engine.workflow.role_owns(r, IN_PROGRESS))
                    }) => {}
                    None => {
                        let name = engine
                            .state
                            .employee(employee)
                            .map(|e| e.employee.name.as_str())
                            .unwrap_or(employee);
                        let title = format!(
                            "{id} can't start: {name} ({}) doesn't own {IN_PROGRESS}; hire a developer or reassign",
                            role.id
                        );
                        let known = state.inbox.iter().any(
                            |item| matches!(item, InboxItem::Alert { title: t, .. } if *t == title),
                        );
                        let queued = actions.iter().any(
                            |a| matches!(a, Action::RaiseAlert { title: t, .. } if *t == title),
                        );
                        if !known && !queued {
                            actions.push(Action::RaiseAlert {
                                ticket: id,
                                title,
                                detail: String::new(),
                            });
                        }
                    }
                }
                continue;
            }
            let busy_with = building.get(employee).copied().unwrap_or(0);
            if sprint_active
                && holder.is_none()
                && engine.workflow.role_owns(role, IN_PROGRESS)
                && state.blockers_clear(ticket)
                && busy_with < role.max_parallel_tasks as usize
                && has_room(sessions)
                && employee_free(engine, ctx, employee)
            {
                *building.entry(employee.clone()).or_default() += 1;
                sessions += 1;
                if ctx.shared_dev_worktree {
                    holder = Some(id.clone());
                }
                actions.push(Action::StartWork {
                    ticket: id,
                    employee: employee.clone(),
                });
            } else if sprint_active
                && holder.is_none()
                && engine.workflow.role_owns(role, IN_PROGRESS)
                && state.blockers_clear(ticket)
                && has_room(sessions)
                && (busy_with >= role.max_parallel_tasks as usize
                    || !employee_free(engine, ctx, employee))
            {
                // Work stealing (ADR 0038): a ready ticket does not wait for a full,
                // held or rate-limited assignee while a developer is free. Nobody
                // free: no action (the stakeholder's alert already asked for staffing).
                // Only truly idle developers steal: one with its own ready ticket
                // starts that first.
                if let Some(to) = pick_builder(engine, ctx, &building, true) {
                    *building.entry(to.clone()).or_default() += 1;
                    actions.push(Action::Reassign { ticket: id, to });
                }
            }
        } else if ticket.state == IN_PROGRESS {
            let Some(employee) = &ticket.assignee else {
                continue;
            };
            let has_session = ctx
                .ticket_sessions
                .contains(&(employee.clone(), id.clone()));
            if ticket.review_requested {
                actions.push(Action::RunTests { ticket: id });
            } else if ticket.conflict.as_ref().is_some_and(|c| !c.briefed) {
                if employee_free(engine, ctx, employee) && (has_session || has_room(sessions)) {
                    if !has_session {
                        sessions += 1;
                    }
                    actions.push(Action::ResolveConflict {
                        ticket: id,
                        employee: employee.clone(),
                    });
                }
            } else if ticket.checks.contains("tests_pass") {
                if let Some(reviewer) = pick_reviewer(engine, ctx, ticket, &reviewing) {
                    *reviewing.entry(reviewer.clone()).or_default() += 1;
                    actions.push(Action::StartReview {
                        ticket: id,
                        reviewer,
                    });
                }
            } else if ticket.needs_rebase
                && ticket.branch.is_some()
                && is_quiet(state, ctx, employee)
            {
                actions.push(Action::Rebase { ticket: id });
            } else if ticket.branch.is_some()
                && !has_session
                && has_room(sessions)
                && employee_free(engine, ctx, employee)
            {
                sessions += 1;
                actions.push(Action::OpenSession {
                    ticket: id,
                    employee: employee.clone(),
                    kind: SessionKind::Work,
                });
            }
        } else if ticket.state == IN_REVIEW
            && let Some(conflict) = &ticket.conflict
        {
            // Approved but conflicting with `dev`: its developer resolves it,
            // then the tests run and it merges, never while the conflict stands.
            let Some(employee) = &ticket.assignee else {
                continue;
            };
            let has_session = ctx
                .ticket_sessions
                .contains(&(employee.clone(), id.clone()));
            let can_open = !has_session && has_room(sessions);
            if ticket.review_requested {
                actions.push(Action::RunTests { ticket: id });
            } else if employee_free(engine, ctx, employee) {
                if !conflict.briefed && (has_session || can_open) {
                    if can_open {
                        sessions += 1;
                    }
                    actions.push(Action::ResolveConflict {
                        ticket: id,
                        employee: employee.clone(),
                    });
                } else if can_open && ticket.branch.is_some() {
                    sessions += 1;
                    actions.push(Action::OpenSession {
                        ticket: id,
                        employee: employee.clone(),
                        kind: SessionKind::Work,
                    });
                }
            }
        } else if ticket.state == IN_REVIEW {
            if ticket.checks.contains("review_approved") {
                // Merges take turns: each one changes what `dev` is.
                if !ticket.checks.contains("merged_to_dev") && !merging {
                    merging = true;
                    actions.push(Action::Merge { ticket: id });
                }
            } else if let Some(reviewer) = &ticket.reviewer
                && !ctx
                    .ticket_sessions
                    .contains(&(reviewer.clone(), id.clone()))
                && has_room(sessions)
                && employee_free(engine, ctx, reviewer)
            {
                sessions += 1;
                actions.push(Action::OpenSession {
                    ticket: id,
                    employee: reviewer.clone(),
                    kind: SessionKind::Review,
                });
            }
        }
    }
    qa_and_release_actions(engine, ctx, sessions, &mut actions);
    triage_actions(engine, ctx, &mut actions);
    planning_actions(engine, ctx, &mut actions);
    request_actions(engine, ctx, &mut actions);
    problem_actions(engine, ctx, &mut actions);
    checkin_actions(engine, ctx, &mut actions);
    stall_actions(engine, ctx, &mut actions);
    crate::apps::app_actions(engine, ctx, &mut actions);
    actions
}

/// The stall watchdog. Once the board has not moved for the threshold: close
/// turns that hang, then brief the project manager once per stall window, and if another
/// full window passes without movement tell the stakeholder once. It runs after every
/// other briefing, so the project manager hears about real work first.
fn stall_actions(engine: &Engine, ctx: &Context, actions: &mut Vec<Action>) {
    let Some(stall) = &ctx.stall else {
        return;
    };
    if !crate::stall::stalled(engine, stall) {
        return;
    }
    let hung = crate::stall::hung(engine, stall);
    for s in &hung {
        let key = hung_key(&s.employee, s.ticket.as_deref());
        if !ctx.busy.contains(&key) {
            actions.push(Action::CloseHungSession {
                key,
                employee: s.employee.clone(),
                ticket: s.ticket.clone(),
            });
        }
    }
    // Brief only once the hung turns are gone: the project manager may be one of them.
    if !hung.is_empty() || ctx.busy.iter().any(|k| k.starts_with("@hung-")) {
        return;
    }
    let window = stall.threshold;
    match stall.briefed_for {
        Some(since) if since >= window && !stall.alerted => {
            if !ctx.busy.contains(KEY_STALL_ALERT) {
                actions.push(Action::StallAlert);
            }
        }
        Some(_) => {}
        // Nobody could be briefed for a whole extra window (no free project manager).
        None if stall.idle_for >= window * 2 && !stall.alerted => {
            if !ctx.busy.contains(KEY_STALL_ALERT) {
                actions.push(Action::StallAlert);
            }
        }
        None => {
            let briefing_out = ctx.busy.contains(KEY_STALL)
                || actions.iter().any(|a| {
                    matches!(
                        a,
                        Action::TriageFailure { pm: Some(_), .. }
                            | Action::PlanSprint { .. }
                            | Action::TriageRequest { pm: Some(_), .. }
                            | Action::PmHandleProblem { .. }
                            | Action::PmCheckin { .. }
                    )
                });
            if !briefing_out && let Some(pm) = pick_triager(engine, ctx) {
                actions.push(Action::PmUnstick { pm });
            }
        }
    }
}

/// Whether a due check-in is worth sending: there is open work, and the board moved
/// since the last one. An unchanged board is still checked after
/// [`crate::checkin::FORCE_AFTER`] quiet intervals, because a stuck board is by
/// definition unchanged.
pub fn checkin_wanted(engine: &Engine, checkin: &CheckinCtx) -> bool {
    crate::checkin::has_open_work(engine)
        && (checkin.unchanged_intervals >= crate::checkin::FORCE_AFTER
            || checkin.last_fingerprint.as_deref()
                != Some(crate::checkin::fingerprint(&engine.state).as_str()))
}

/// The project manager's periodic board check-in. It comes last: any other
/// briefing the project manager is due, or already owes, goes first.
fn checkin_actions(engine: &Engine, ctx: &Context, actions: &mut Vec<Action>) {
    let Some(checkin) = ctx.checkin.as_ref().filter(|c| c.due) else {
        return;
    };
    let state = &engine.state;
    if ctx.busy.contains(KEY_CHECKIN)
        || actions.iter().any(|a| {
            matches!(
                a,
                Action::TriageFailure { pm: Some(_), .. }
                    | Action::PlanSprint { .. }
                    | Action::TriageRequest { pm: Some(_), .. }
                    | Action::PmHandleProblem { .. }
                    | Action::PmCheckin { .. }
            )
        })
        || state.planning.as_ref().is_some_and(|p| !p.briefed)
        || crate::intake::open_requests(state).next().is_some()
        || !checkin_wanted(engine, checkin)
    {
        return;
    }
    if let Some(pm) = pick_triager(engine, ctx) {
        actions.push(Action::PmCheckin { pm });
    }
}

/// The problems a briefing would carry now: the deferred ones plus failed agents, minus
/// any already handed to the stakeholder and any about a held employee.
pub fn live_problems(engine: &Engine, ctx: &Context) -> Vec<crate::problems::Problem> {
    let Some(problem) = &ctx.problem else {
        return Vec::new();
    };
    let mut found = crate::problems::problems(engine, &problem.notes, usize::MAX);
    found.retain(|p| !problem.escalated.contains(&p.key));
    found
}

/// Problems the runner deferred to the project manager (ADR 0032): one briefing, after
/// the earlier ones (a failure, a sprint, a request) and before the board check-in.
fn problem_actions(engine: &Engine, ctx: &Context, actions: &mut Vec<Action>) {
    if !ctx.problem.as_ref().is_some_and(|p| p.due)
        || ctx.busy.contains(KEY_PROBLEM)
        || actions.iter().any(|a| {
            matches!(
                a,
                Action::TriageFailure { pm: Some(_), .. }
                    | Action::PlanSprint { .. }
                    | Action::TriageRequest { pm: Some(_), .. }
                    | Action::PmHandleProblem { .. }
            )
        })
    {
        return;
    }
    if live_problems(engine, ctx).is_empty() {
        return;
    }
    if let Some(pm) = pick_triager(engine, ctx) {
        actions.push(Action::PmHandleProblem { pm });
    }
}

/// The oldest open stakeholder request, one at a time: a briefing to a
/// project manager who is free and whose backend works. A request briefed too often is
/// given up. Nothing happens without a planner; the request stays queued.
fn request_actions(engine: &Engine, ctx: &Context, actions: &mut Vec<Action>) {
    let Some(request) = crate::intake::next_request(&engine.state) else {
        return;
    };
    let key = crate::intake::request_key(&request.id);
    if ctx.busy.contains(&key) {
        return;
    }
    if request.attempts >= crate::intake::MAX_ATTEMPTS {
        actions.push(Action::TriageRequest {
            key,
            id: request.id.clone(),
            pm: None,
        });
        return;
    }
    // The project manager takes one briefing per tick.
    if actions.iter().any(|a| {
        matches!(
            a,
            Action::TriageFailure { pm: Some(_), .. } | Action::PlanSprint { .. }
        )
    }) {
        return;
    }
    if let Some(pm) = pick_triager(engine, ctx) {
        actions.push(Action::TriageRequest {
            key,
            id: request.id.clone(),
            pm: Some(pm),
        });
    }
}

/// A planning request waiting for the project manager: one briefing, to a
/// planner who is free and whose backend works. Nothing happens without one; the
/// request stays until a planner is available.
fn planning_actions(engine: &Engine, ctx: &Context, actions: &mut Vec<Action>) {
    if engine.state.planning.as_ref().is_none_or(|p| p.briefed) || ctx.busy.contains(KEY_PLAN) {
        return;
    }
    // The project manager takes one briefing per tick.
    if actions
        .iter()
        .any(|a| matches!(a, Action::TriageFailure { pm: Some(_), .. }))
    {
        return;
    }
    if let Some(pm) = pick_triager(engine, ctx) {
        actions.push(Action::PlanSprint { pm });
    }
}

/// Failures waiting for a bug: one briefing per tick, to a project
/// manager who is free and whose backend works. A failure briefed too often is given up.
fn triage_actions(engine: &Engine, ctx: &Context, actions: &mut Vec<Action>) {
    let state = &engine.state;
    let pending = state.failures.iter().filter(|f| {
        f.status == crate::triage::TriageStatus::Pending
            && !ctx.busy.contains(&triage_key(&f.fingerprint))
            && crate::triage::still_failing(state, f)
    });
    for failure in pending {
        let key = triage_key(&failure.fingerprint);
        if failure.attempts >= crate::triage::MAX_ATTEMPTS {
            actions.push(Action::TriageFailure {
                key,
                fingerprint: failure.fingerprint.clone(),
                pm: None,
            });
            continue;
        }
        let Some(pm) = pick_triager(engine, ctx) else {
            return;
        };
        actions.push(Action::TriageFailure {
            key,
            fingerprint: failure.fingerprint.clone(),
            pm: Some(pm),
        });
        // One at a time: the next failure waits until this briefing is out and the
        // project manager is busy with it.
        return;
    }
}

/// The planner (the role that owns the workflow's first state) who can take a briefing
/// now: backend usable and not mid-turn.
pub fn pick_triager(engine: &Engine, ctx: &Context) -> Option<EmployeeId> {
    let initial = engine.workflow.initial_state();
    engine
        .state
        .employees
        .iter()
        .filter(|e| {
            engine
                .roles
                .get(&e.employee.role)
                .is_some_and(|r| engine.workflow.role_owns(r, initial))
        })
        .filter(|e| employee_free(engine, ctx, &e.employee.id))
        .find(|e| is_quiet(&engine.state, ctx, &e.employee.id))
        .map(|e| e.employee.id.clone())
}

/// Whether a project manager could take a problem briefing at all: a planner whose
/// backend works and who is not held. Unlike [`pick_triager`] it ignores whether they are
/// busy this moment, so the runner can decide to defer a problem to them.
pub fn pm_available(engine: &Engine, ctx: &Context) -> bool {
    let initial = engine.workflow.initial_state();
    engine.state.employees.iter().any(|e| {
        engine
            .roles
            .get(&e.employee.role)
            .is_some_and(|r| engine.workflow.role_owns(r, initial))
            && employee_free(engine, ctx, &e.employee.id)
            && !(e.status == EmployeeStatus::Failed
                && ctx.cooling_employees.contains(&e.employee.id))
    })
}

/// QA rounds and releases: the round-wide actions, then
/// one QA session per untested ticket.
fn qa_and_release_actions(
    engine: &Engine,
    ctx: &Context,
    mut sessions: usize,
    actions: &mut Vec<Action>,
) {
    let state = &engine.state;
    let free = |key: &str| !ctx.busy.contains(key);
    match state.open_round() {
        Some(round) => match round.status {
            QaRoundStatus::Starting if free(KEY_QA_ENV) => actions.push(Action::RunEnv {
                step: if round.up_done {
                    EnvStep::Test
                } else {
                    EnvStep::Up
                },
            }),
            // A failed step waits for the stakeholder's retry.
            QaRoundStatus::EnvFailed if free(KEY_QA_ENV) => {
                if let Some(retry) = state.env_retry.as_ref().filter(|r| r.env == "qa") {
                    actions.push(Action::RunEnv { step: retry.step });
                }
            }
            QaRoundStatus::Testing => {
                let untested: Vec<&Ticket> = round
                    .tickets
                    .iter()
                    .filter_map(|id| state.ticket(id))
                    .filter(|t| t.state == IN_QA && !round.verdicts.contains_key(&t.id))
                    .collect();
                if untested.is_empty() {
                    if free(KEY_QA_CLOSE) {
                        actions.push(Action::CloseRound);
                    }
                } else {
                    // Sessions already open count against each tester's room.
                    let mut load: BTreeMap<EmployeeId, usize> = BTreeMap::new();
                    for (employee, ticket) in &ctx.ticket_sessions {
                        if is_tester(engine, employee)
                            && state.ticket(ticket).is_some_and(|t| t.state == IN_QA)
                        {
                            *load.entry(employee.clone()).or_default() += 1;
                        }
                    }
                    for t in untested {
                        let covered = ctx
                            .ticket_sessions
                            .iter()
                            .any(|(e, id)| id == &t.id && is_tester(engine, e));
                        if covered || ctx.busy.contains(&t.id) || sessions >= ctx.max_concurrent {
                            continue;
                        }
                        if let Some(employee) = pick_tester(engine, ctx, &load) {
                            *load.entry(employee.clone()).or_default() += 1;
                            sessions += 1;
                            actions.push(Action::OpenSession {
                                ticket: t.id.clone(),
                                employee,
                                kind: SessionKind::Qa,
                            });
                        }
                    }
                }
            }
            _ => {}
        },
        None if free(KEY_PROMOTE) && should_promote(state) => actions.push(Action::Promote),
        None => {}
    }
    for release in &state.releases {
        match release.status {
            ReleaseStatus::Approved if free(KEY_RELEASE) => actions.push(Action::Release {
                id: release.id.clone(),
            }),
            ReleaseStatus::Tagged if free(KEY_DEPLOY) => actions.push(Action::Deploy {
                id: release.id.clone(),
            }),
            ReleaseStatus::DeployFailed
                if free(KEY_DEPLOY)
                    && state
                        .env_retry
                        .as_ref()
                        .is_some_and(|r| r.env == "prod" && r.step == EnvStep::Deploy) =>
            {
                actions.push(Action::Deploy {
                    id: release.id.clone(),
                });
            }
            _ => {}
        }
    }
}

/// Promote as soon as anything is waiting for QA, so testing runs alongside development
/// instead of after it. A running round or an open release holds `qa` still; tickets
/// that arrive meanwhile join the next round. A request promotes even a failed story
/// that still waits for its fix.
fn should_promote(state: &crate::engine::State) -> bool {
    // A release ships what QA tested, so `qa` waits until it is out.
    if state.open_release().is_some() {
        return false;
    }
    if state.qa_requested {
        return !state.waiting_for_qa().is_empty();
    }
    !state.qa_ready().is_empty()
}

fn is_tester(engine: &Engine, employee: &str) -> bool {
    engine
        .state
        .employee(employee)
        .and_then(|e| engine.roles.get(&e.employee.role))
        .is_some_and(|role| engine.workflow.role_owns(role, IN_QA))
}

/// The tester with room and the fewest tickets under test.
fn pick_tester(
    engine: &Engine,
    ctx: &Context,
    load: &BTreeMap<EmployeeId, usize>,
) -> Option<EmployeeId> {
    engine
        .state
        .employees
        .iter()
        .filter(|e| employee_free(engine, ctx, &e.employee.id))
        .filter_map(|e| {
            let role = engine.roles.get(&e.employee.role)?;
            let busy = load.get(&e.employee.id).copied().unwrap_or(0);
            (engine.workflow.role_owns(role, IN_QA) && busy < role.max_parallel_tasks as usize)
                .then_some((busy, &e.employee.id))
        })
        .min_by_key(|(busy, _)| *busy)
        .map(|(_, id)| id.clone())
}

/// The developer to give an unbuildable ticket: a role that owns `in_progress`, a
/// usable backend, room under the role's parallel limit, and the lightest load.
fn pick_builder(
    engine: &Engine,
    ctx: &Context,
    building: &BTreeMap<EmployeeId, usize>,
    skip_with_own_work: bool,
) -> Option<EmployeeId> {
    let planned = engine.workflow.planned_state();
    let has_own_ready = |who: &str| {
        engine.state.tickets.iter().any(|t| {
            t.dropped.is_none()
                && Some(t.state.as_str()) == planned
                && t.assignee.as_deref() == Some(who)
                && !ctx.busy.contains(&t.id)
                && engine.state.blockers_clear(t)
                && t.sprint
                    .as_deref()
                    .and_then(|sp| engine.state.sprint(sp))
                    .is_some_and(|sp| sp.status == SprintStatus::Active)
        })
    };
    engine
        .state
        .employees
        .iter()
        .filter(|e| employee_free(engine, ctx, &e.employee.id))
        .filter(|e| !(skip_with_own_work && has_own_ready(&e.employee.id)))
        .filter_map(|e| {
            let role = engine.roles.get(&e.employee.role)?;
            let busy = building.get(&e.employee.id).copied().unwrap_or(0);
            (engine.workflow.role_owns(role, IN_PROGRESS)
                && busy < role.max_parallel_tasks as usize)
                .then_some((busy, &e.employee.id))
        })
        .min_by_key(|(busy, _)| *busy)
        .map(|(_, id)| id.clone())
}

fn load(
    state: &crate::engine::State,
    who: impl Fn(&Ticket) -> Option<EmployeeId>,
) -> BTreeMap<EmployeeId, usize> {
    let mut counts = BTreeMap::new();
    for ticket in &state.tickets {
        if let Some(employee) = who(ticket) {
            *counts.entry(employee).or_default() += 1;
        }
    }
    counts
}

/// An employee whose agent is not mid-turn, so their tree is theirs to leave alone. A
/// failed one counts once their cooldown is over: the session is gone and a new one opens.
fn is_quiet(state: &crate::engine::State, ctx: &Context, employee: &str) -> bool {
    state.employee(employee).is_some_and(|e| match e.status {
        EmployeeStatus::Idle | EmployeeStatus::Offline => true,
        EmployeeStatus::Failed => !ctx.cooling_employees.contains(employee),
        _ => false,
    })
}

/// The reviewer with room and the fewest reviews; the author only when they are the
/// only one who can review (see `Engine::sole_reviewer`).
fn pick_reviewer(
    engine: &Engine,
    ctx: &Context,
    ticket: &Ticket,
    reviewing: &BTreeMap<EmployeeId, usize>,
) -> Option<EmployeeId> {
    other_reviewer(engine, ctx, ticket, reviewing).or_else(|| {
        // The only reviewer built the fix (a Tech Lead working a bug): they review it
        // themselves rather than stall it. Only when no one else could ever review.
        let author = ticket.assignee.as_deref()?;
        (!held(engine, author) && engine.sole_reviewer(author)).then(|| author.to_string())
    })
}

fn other_reviewer(
    engine: &Engine,
    ctx: &Context,
    ticket: &Ticket,
    reviewing: &BTreeMap<EmployeeId, usize>,
) -> Option<EmployeeId> {
    engine
        .state
        .employees
        .iter()
        .filter(|e| ticket.assignee.as_deref() != Some(e.employee.id.as_str()))
        .filter(|e| employee_free(engine, ctx, &e.employee.id))
        .filter_map(|e| {
            let role = engine.roles.get(&e.employee.role)?;
            let load = reviewing.get(&e.employee.id).copied().unwrap_or(0);
            (engine.workflow.role_owns(role, IN_REVIEW) && load < role.max_parallel_tasks as usize)
                .then_some((load, &e.employee.id))
        })
        .min_by_key(|(load, _)| *load)
        .map(|(_, id)| id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::Command;
    use crate::event::{Actor, Event, EventKind};
    use crate::role::{Role, RoleCatalog};
    use crate::workflow::Workflow;

    fn engine() -> Engine {
        let roles = [
            ("project-manager", "manager", 1),
            ("fullstack-developer", "developer", 1),
            ("tech-lead", "reviewer", 2),
            ("qa-engineer", "qa", 1),
        ]
        .map(|(id, tag, max)| {
            Role::parse(&format!(
                "---\nid: {id}\ntitle: {id}\ntags: [{tag}]\nmax_parallel_tasks: {max}\n---\nprompt\n"
            ))
            .expect("valid role")
        });
        Engine::new("Acme", RoleCatalog::new(roles), Workflow::default())
    }

    fn run(e: &mut Engine, actor: Actor, cmd: Command) {
        for kind in e.decide(&actor, cmd).expect("command accepted") {
            apply_as(e, actor.clone(), kind);
        }
    }

    fn apply(e: &mut Engine, kind: EventKind) {
        apply_as(e, Actor::System, kind);
    }

    fn apply_as(e: &mut Engine, actor: Actor, kind: EventKind) {
        let event = Event {
            seq: e.state.last_seq + 1,
            ts: chrono::DateTime::from_timestamp(0, 0).expect("epoch is valid"),
            actor,
            kind,
        };
        e.apply(&event);
    }

    fn hire(e: &mut Engine, role: &str, name: &str) {
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

    fn story(e: &mut Engine, title: &str) {
        run(
            e,
            Actor::Employee("pam".into()),
            Command::CreateTicket {
                kind: TicketKind::Story,
                title: title.into(),
                description: String::new(),
                acceptance_criteria: vec![],
                estimate: None,
                epic: None,
                external: None,
            },
        );
    }

    /// Pam, Dev and Lead; tickets T-1..T-n in an approved sprint, all assigned to Dev.
    fn team(tickets: usize) -> Engine {
        let mut e = engine();
        hire(&mut e, "project-manager", "Pam");
        hire(&mut e, "fullstack-developer", "Dev");
        hire(&mut e, "tech-lead", "Lead");
        for n in 1..=tickets {
            story(&mut e, &format!("Story {n}"));
        }
        run(
            &mut e,
            Actor::Employee("pam".into()),
            Command::ProposeSprint {
                name: "S1".into(),
                goal: String::new(),
                tickets: (1..=tickets).map(|n| format!("T-{n}")).collect(),
            },
        );
        run(
            &mut e,
            Actor::Stakeholder,
            Command::ApproveSprint { id: "SP-1".into() },
        );
        for n in 1..=tickets {
            run(
                &mut e,
                Actor::Employee("pam".into()),
                Command::AssignTicket {
                    id: format!("T-{n}"),
                    assignee: Some("dev".into()),
                },
            );
        }
        e
    }

    fn ctx() -> Context {
        Context {
            max_concurrent: 3,
            ..Context::default()
        }
    }

    fn start_work(e: &mut Engine, id: &str) {
        run(
            e,
            Actor::System,
            Command::StartWork {
                id: id.into(),
                branch: format!("feat/{id}"),
                worktree: format!("w/{id}"),
            },
        );
    }

    fn start(ticket: &str) -> Action {
        Action::StartWork {
            ticket: ticket.into(),
            employee: "dev".into(),
        }
    }

    /// The stakeholder stops `who` mid-turn, which holds them.
    fn stop(e: &mut Engine, who: &str) {
        apply(
            e,
            EventKind::AgentMessageChunk {
                employee: who.into(),
                text: "x".into(),
            },
        );
        run(
            e,
            Actor::Stakeholder,
            Command::CancelTurn {
                employee: who.into(),
            },
        );
    }

    #[test]
    fn a_stopped_developer_gets_no_work_until_the_stakeholder_writes() {
        let mut e = team(1);
        stop(&mut e, "dev");
        assert!(held(&e, "dev"));
        assert!(next_actions(&e, &ctx()).is_empty());
        // A teammate's message does not lift the hold.
        run(
            &mut e,
            Actor::Employee("pam".into()),
            Command::SendChat {
                to: "dev".into(),
                text: "psst".into(),
            },
        );
        assert!(next_actions(&e, &ctx()).is_empty());
        run(
            &mut e,
            Actor::Stakeholder,
            Command::SendChat {
                to: "dev".into(),
                text: "go on".into(),
            },
        );
        assert_eq!(next_actions(&e, &ctx()), [start("T-1")]);
    }

    #[test]
    fn a_stopped_developer_is_not_given_a_session_or_a_reassigned_ticket() {
        let mut e = team(1);
        start_work(&mut e, "T-1");
        assert!(matches!(
            next_actions(&e, &ctx()).as_slice(),
            [Action::OpenSession { .. }]
        ));
        stop(&mut e, "dev");
        assert!(next_actions(&e, &ctx()).is_empty());
        // Their ticket stays theirs.
        assert_eq!(
            e.state
                .ticket("T-1")
                .and_then(|t| t.assignee.clone())
                .as_deref(),
            Some("dev")
        );
    }

    #[test]
    fn a_stopped_developer_is_not_picked_to_take_over_a_ticket() {
        let mut e = team(1);
        hire(&mut e, "fullstack-developer", "Dee");
        assign_to_lead(&mut e);
        stop(&mut e, "dev");
        assert_eq!(
            next_actions(&e, &ctx()),
            [Action::Reassign {
                ticket: "T-1".into(),
                to: "dee".into()
            }]
        );
    }

    #[test]
    fn a_stopped_project_manager_gets_no_briefing() {
        let mut e = team(1);
        assert!(pick_triager(&e, &ctx()).is_some());
        stop(&mut e, "pam");
        assert!(pick_triager(&e, &ctx()).is_none());
    }

    fn fail_agent(e: &mut Engine, who: &str) {
        apply(
            e,
            EventKind::AgentFailed {
                employee: who.into(),
                error: "3 chats are busy at once".into(),
                ticket: None,
            },
        );
    }

    #[test]
    fn a_failed_project_manager_is_picked_again_after_the_cooldown() {
        let mut e = team(1);
        fail_agent(&mut e, "pam");
        let cooling = Context {
            cooling_employees: ["pam".to_string()].into(),
            ..ctx()
        };
        assert!(pick_triager(&e, &cooling).is_none());
        assert!(!pm_available(&e, &cooling));
        assert_eq!(pick_triager(&e, &ctx()), Some("pam".to_string()));
        assert!(pm_available(&e, &ctx()));
    }

    #[test]
    fn a_failed_project_manager_gets_a_checkin_after_the_cooldown() {
        let mut e = team(1);
        fail_agent(&mut e, "pam");
        let due = Some(CheckinCtx {
            due: true,
            last_fingerprint: None,
            unchanged_intervals: 0,
        });
        let cooling = Context {
            checkin: due.clone(),
            cooling_employees: ["pam".to_string()].into(),
            ..ctx()
        };
        let has_checkin = |c: &Context| {
            next_actions(&e, c)
                .iter()
                .any(|a| matches!(a, Action::PmCheckin { .. }))
        };
        assert!(!has_checkin(&cooling));
        assert!(has_checkin(&Context {
            checkin: due,
            ..ctx()
        }));
    }

    #[test]
    fn a_failed_project_manager_on_a_halted_backend_stays_unpicked() {
        let mut e = team(1);
        fail_agent(&mut e, "pam");
        let backend = e
            .state
            .employee("pam")
            .expect("pam")
            .employee
            .backend
            .clone();
        let halted = Context {
            halted_backends: [backend].into(),
            ..ctx()
        };
        assert!(pick_triager(&e, &halted).is_none());
        assert!(!pm_available(&e, &halted));
    }

    #[test]
    fn a_planned_assigned_ticket_starts() {
        let e = team(1);
        assert_eq!(next_actions(&e, &ctx()), [start("T-1")]);
    }

    fn prioritize(e: &mut Engine, id: &str, urgent: bool) {
        run(
            e,
            Actor::Stakeholder,
            Command::PrioritizeTicket {
                id: id.into(),
                urgent,
            },
        );
    }

    fn one_slot() -> Context {
        Context {
            max_concurrent: 1,
            ..Context::default()
        }
    }

    #[test]
    fn the_same_board_starts_work_only_when_the_limit_leaves_room() {
        let e = team(2);
        let at = |max_concurrent| Context {
            max_concurrent,
            sessions_open: 1,
            ..Context::default()
        };
        assert!(next_actions(&e, &at(1)).is_empty());
        assert_eq!(next_actions(&e, &at(3)), [start("T-1")]);
    }

    #[test]
    fn an_urgent_later_ticket_is_started_before_an_earlier_one() {
        let mut e = team(2);
        // Dee can build too, so the slot (not the developer) is what is scarce.
        hire(&mut e, "fullstack-developer", "Dee");
        assert_eq!(next_actions(&e, &one_slot()), [start("T-1")]);
        prioritize(&mut e, "T-2", true);
        assert_eq!(next_actions(&e, &one_slot()), [start("T-2")]);
        // Clearing the mark restores creation order.
        prioritize(&mut e, "T-2", false);
        assert_eq!(next_actions(&e, &one_slot()), [start("T-1")]);
    }

    #[test]
    fn urgent_does_not_bypass_blockers_or_sprint_approval() {
        let mut e = team(2);
        run(
            &mut e,
            Actor::Employee("pam".into()),
            Command::SetBlockers {
                id: "T-2".into(),
                blocked_by: vec!["T-1".into()],
            },
        );
        prioritize(&mut e, "T-2", true);
        assert_eq!(next_actions(&e, &one_slot()), [start("T-1")]);
        // A ticket outside any approved sprint stays put even when urgent.
        let mut e = team(1);
        story(&mut e, "Loose");
        prioritize(&mut e, "T-2", true);
        run(
            &mut e,
            Actor::Employee("pam".into()),
            Command::AssignTicket {
                id: "T-2".into(),
                assignee: Some("dev".into()),
            },
        );
        assert_eq!(next_actions(&e, &one_slot()), [start("T-1")]);
    }

    #[test]
    fn an_urgent_ticket_whose_assignee_is_full_goes_to_a_free_developer() {
        let mut e = team(2);
        hire(&mut e, "fullstack-developer", "Dee");
        start_work(&mut e, "T-1");
        let running = Context {
            ticket_sessions: [("dev".to_string(), "T-1".to_string())].into(),
            ..ctx()
        };
        // Dev is full with T-1: T-2 goes to the idle developer, urgent or not.
        prioritize(&mut e, "T-2", true);
        assert_eq!(
            next_actions(&e, &running),
            [Action::Reassign {
                ticket: "T-2".into(),
                to: "dee".into()
            }]
        );
    }

    #[test]
    fn two_independent_tickets_of_one_developer_both_start() {
        let mut e = team(2);
        hire(&mut e, "fullstack-developer", "Dee");
        // First tick: T-1 starts for dev, T-2 is handed to dee.
        assert_eq!(
            next_actions(&e, &ctx()),
            [
                start("T-1"),
                Action::Reassign {
                    ticket: "T-2".into(),
                    to: "dee".into()
                }
            ]
        );
        // Once reassigned, it starts for dee next tick.
        run(
            &mut e,
            Actor::Employee("pam".into()),
            Command::AssignTicket {
                id: "T-2".into(),
                assignee: Some("dee".into()),
            },
        );
        let actions = next_actions(&e, &ctx());
        assert!(actions.contains(&Action::StartWork {
            ticket: "T-2".into(),
            employee: "dee".into()
        }));
    }

    #[test]
    fn a_developer_with_own_ready_work_does_not_steal() {
        let mut e = team(3);
        hire(&mut e, "fullstack-developer", "Dee");
        run(
            &mut e,
            Actor::Employee("pam".into()),
            Command::AssignTicket {
                id: "T-3".into(),
                assignee: Some("dee".into()),
            },
        );
        start_work(&mut e, "T-1");
        let running = Context {
            ticket_sessions: [("dev".to_string(), "T-1".to_string())].into(),
            ..ctx()
        };
        let actions = next_actions(&e, &running);
        assert!(actions.contains(&Action::StartWork {
            ticket: "T-3".into(),
            employee: "dee".into()
        }));
        assert!(!actions.iter().any(|a| matches!(a, Action::Reassign { .. })));
    }

    #[test]
    fn a_blocked_ticket_is_not_stolen() {
        let mut e = team(2);
        hire(&mut e, "fullstack-developer", "Dee");
        run(
            &mut e,
            Actor::Employee("pam".into()),
            Command::SetBlockers {
                id: "T-2".into(),
                blocked_by: vec!["T-1".into()],
            },
        );
        assert_eq!(next_actions(&e, &ctx()), [start("T-1")]);
    }

    #[test]
    fn shared_mode_still_serialises_independent_tickets() {
        let mut e = team(2);
        hire(&mut e, "fullstack-developer", "Dee");
        let shared = Context {
            shared_dev_worktree: true,
            ..ctx()
        };
        assert_eq!(next_actions(&e, &shared), [start("T-1")]);
    }

    #[test]
    fn an_urgent_ticket_with_nobody_free_adds_no_action() {
        let mut e = team(1);
        prioritize(&mut e, "T-1", true);
        stop(&mut e, "dev");
        assert!(next_actions(&e, &ctx()).is_empty());
        // A second developer who is held too changes nothing.
        hire(&mut e, "fullstack-developer", "Dee");
        stop(&mut e, "dee");
        assert!(next_actions(&e, &ctx()).is_empty());
    }

    #[test]
    fn an_urgent_ticket_takes_the_shared_worktree_first() {
        let mut e = team(2);
        prioritize(&mut e, "T-2", true);
        let shared = Context {
            shared_dev_worktree: true,
            ..ctx()
        };
        assert_eq!(next_actions(&e, &shared), [start("T-2")]);
    }

    #[test]
    fn ticket_order_puts_urgent_first_and_keeps_creation_order_otherwise() {
        let mut e = team(4);
        prioritize(&mut e, "T-4", true);
        prioritize(&mut e, "T-2", true);
        let ids: Vec<&str> = ticket_order(&e.state)
            .iter()
            .map(|t| t.id.as_str())
            .collect();
        assert_eq!(ids, ["T-2", "T-4", "T-1", "T-3"]);
    }

    #[test]
    fn nothing_starts_while_paused() {
        let mut e = team(1);
        run(&mut e, Actor::Stakeholder, Command::PauseDispatch);
        assert!(next_actions(&e, &ctx()).is_empty());
    }

    #[test]
    fn unassigned_and_unplanned_tickets_wait() {
        let mut e = team(1);
        story(&mut e, "Loose");
        assert_eq!(next_actions(&e, &ctx()), [start("T-1")]);
        run(
            &mut e,
            Actor::Employee("pam".into()),
            Command::AssignTicket {
                id: "T-1".into(),
                assignee: None,
            },
        );
        assert!(next_actions(&e, &ctx()).is_empty());
    }

    fn assign_to_lead(e: &mut Engine) {
        run(
            e,
            Actor::Stakeholder,
            Command::AssignTicket {
                id: "T-1".into(),
                assignee: Some("lead".into()),
            },
        );
    }

    #[test]
    fn a_ticket_whose_assignee_cannot_build_goes_to_the_least_loaded_developer() {
        let mut e = team(2);
        hire(&mut e, "fullstack-developer", "Dee");
        assign_to_lead(&mut e);
        // Dev builds nothing yet and Dee neither; ties go to the first hired.
        let reassign = Action::Reassign {
            ticket: "T-1".into(),
            to: "dev".into(),
        };
        assert_eq!(next_actions(&e, &ctx())[0], reassign);
        // With Dev already building something, Dee is lighter.
        start_work(&mut e, "T-2");
        assert!(next_actions(&e, &ctx()).contains(&Action::Reassign {
            ticket: "T-1".into(),
            to: "dee".into()
        }));
    }

    #[test]
    fn with_no_developer_to_take_it_one_alert_is_raised_once() {
        let mut e = engine();
        hire(&mut e, "project-manager", "Pam");
        hire(&mut e, "tech-lead", "Lead");
        story(&mut e, "Story 1");
        run(
            &mut e,
            Actor::Employee("pam".into()),
            Command::ProposeSprint {
                name: "S1".into(),
                goal: String::new(),
                tickets: vec!["T-1".into()],
            },
        );
        run(
            &mut e,
            Actor::Stakeholder,
            Command::ApproveSprint { id: "SP-1".into() },
        );
        assign_to_lead(&mut e);
        let actions = next_actions(&e, &ctx());
        let [Action::RaiseAlert { title, detail, .. }] = actions.as_slice() else {
            panic!("{actions:?}");
        };
        assert!(title.contains("T-1 can't start"), "{title}");
        run(
            &mut e,
            Actor::System,
            Command::RaiseAlert {
                title: title.clone(),
                detail: detail.clone(),
            },
        );
        assert!(next_actions(&e, &ctx()).is_empty());
    }

    #[test]
    fn wip_limit_starts_one_ticket_at_a_time_per_developer() {
        let mut e = team(2);
        assert_eq!(next_actions(&e, &ctx()), [start("T-1")]);
        start_work(&mut e, "T-1");
        let actions = next_actions(&e, &ctx());
        assert!(!actions.contains(&start("T-2")), "{actions:?}");
    }

    #[test]
    fn blocked_tickets_wait_for_their_blockers() {
        let mut e = team(2);
        run(
            &mut e,
            Actor::Employee("pam".into()),
            Command::SetBlockers {
                id: "T-1".into(),
                blocked_by: vec!["T-2".into()],
            },
        );
        assert_eq!(next_actions(&e, &ctx()), [start("T-2")]);
    }

    #[test]
    fn the_concurrent_agent_limit_holds_work_back() {
        let e = team(1);
        let full = Context {
            max_concurrent: 2,
            sessions_open: 2,
            ..Context::default()
        };
        assert!(next_actions(&e, &full).is_empty());
    }

    #[test]
    fn busy_tickets_get_no_second_action() {
        let e = team(1);
        let busy = Context {
            busy: ["T-1".to_string()].into(),
            ..ctx()
        };
        assert!(next_actions(&e, &busy).is_empty());
    }

    #[test]
    fn a_started_ticket_without_a_session_gets_one() {
        let mut e = team(1);
        start_work(&mut e, "T-1");
        let open = Action::OpenSession {
            ticket: "T-1".into(),
            employee: "dev".into(),
            kind: SessionKind::Work,
        };
        assert_eq!(next_actions(&e, &ctx()), [open]);
        let running = Context {
            ticket_sessions: [("dev".to_string(), "T-1".to_string())].into(),
            ..ctx()
        };
        assert!(next_actions(&e, &running).is_empty());
    }

    #[test]
    fn a_review_request_runs_the_tests() {
        let mut e = team(1);
        start_work(&mut e, "T-1");
        run(
            &mut e,
            Actor::Employee("dev".into()),
            Command::RequestReview {
                id: "T-1".into(),
                summary: "done".into(),
            },
        );
        assert_eq!(
            next_actions(&e, &ctx()),
            [Action::RunTests {
                ticket: "T-1".into()
            }]
        );
    }

    fn tested(e: &mut Engine, id: &str) {
        start_work(e, id);
        run(
            e,
            Actor::Employee("dev".into()),
            Command::RequestReview {
                id: id.into(),
                summary: String::new(),
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
    }

    #[test]
    fn passing_tests_go_to_the_least_loaded_reviewer_who_is_not_the_author() {
        let mut e = team(1);
        hire(&mut e, "tech-lead", "Second Lead");
        tested(&mut e, "T-1");
        assert_eq!(
            next_actions(&e, &ctx()),
            [Action::StartReview {
                ticket: "T-1".into(),
                reviewer: "lead".into()
            }]
        );
    }

    #[test]
    fn failing_tests_leave_the_ticket_with_its_developer() {
        let mut e = team(1);
        start_work(&mut e, "T-1");
        run(
            &mut e,
            Actor::Employee("dev".into()),
            Command::RequestReview {
                id: "T-1".into(),
                summary: String::new(),
            },
        );
        run(
            &mut e,
            Actor::System,
            Command::RecordTests {
                id: "T-1".into(),
                passed: false,
                skipped: false,
                output_tail: "boom".into(),
            },
        );
        let running = Context {
            ticket_sessions: [("dev".to_string(), "T-1".to_string())].into(),
            ..ctx()
        };
        assert!(next_actions(&e, &running).is_empty());
    }

    fn reviewed(e: &mut Engine, verdict: ReviewVerdict) {
        tested(e, "T-1");
        run(
            e,
            Actor::System,
            Command::StartReview {
                id: "T-1".into(),
                reviewer: "lead".into(),
            },
        );
        run(
            e,
            Actor::Employee("lead".into()),
            Command::SubmitReview {
                id: "T-1".into(),
                verdict,
                body: "see comments".into(),
            },
        );
    }

    #[test]
    fn a_ticket_in_review_gets_a_review_session() {
        let mut e = team(1);
        tested(&mut e, "T-1");
        run(
            &mut e,
            Actor::System,
            Command::StartReview {
                id: "T-1".into(),
                reviewer: "lead".into(),
            },
        );
        assert_eq!(
            next_actions(&e, &ctx()),
            [Action::OpenSession {
                ticket: "T-1".into(),
                employee: "lead".into(),
                kind: SessionKind::Review
            }]
        );
    }

    #[test]
    fn an_approved_ticket_merges() {
        let mut e = team(1);
        reviewed(&mut e, ReviewVerdict::Approve);
        assert_eq!(
            next_actions(&e, &ctx()),
            [Action::Merge {
                ticket: "T-1".into()
            }]
        );
    }

    #[test]
    fn merges_take_turns() {
        let mut e = team(2);
        // Two approved tickets at once: only the first merges this round.
        for id in ["T-1", "T-2"] {
            for t in e.state.tickets.iter_mut().filter(|t| t.id == id) {
                t.state = IN_REVIEW.into();
                t.reviewer = Some("lead".into());
                t.checks.insert("review_approved".into());
            }
        }
        let merges = next_actions(&e, &ctx())
            .into_iter()
            .filter(|a| matches!(a, Action::Merge { .. }))
            .count();
        assert_eq!(merges, 1);
    }

    #[test]
    fn requested_changes_reopen_the_developers_session() {
        let mut e = team(1);
        reviewed(&mut e, ReviewVerdict::RequestChanges);
        assert_eq!(
            next_actions(&e, &ctx()),
            [Action::OpenSession {
                ticket: "T-1".into(),
                employee: "dev".into(),
                kind: SessionKind::Work
            }]
        );
    }

    #[test]
    fn open_branches_rebase_after_a_merge_once_their_agent_is_quiet() {
        let mut e = team(2);
        start_work(&mut e, "T-1");
        // T-2 is the one that merged; T-1 is still being built.
        apply(
            &mut e,
            EventKind::BranchMerged {
                id: "T-2".into(),
                commit: "abc".into(),
            },
        );
        assert!(e.state.ticket("T-1").expect("ticket").needs_rebase);
        let running = Context {
            ticket_sessions: [("dev".to_string(), "T-1".to_string())].into(),
            ..ctx()
        };
        assert_eq!(
            next_actions(&e, &running),
            [Action::Rebase {
                ticket: "T-1".into()
            }]
        );
        // Mid-turn, the developer's tree is left alone.
        apply(
            &mut e,
            EventKind::AgentMessageChunk {
                employee: "dev".into(),
                text: "working".into(),
            },
        );
        assert!(next_actions(&e, &running).is_empty());
    }

    // ---- QA and releases ----

    fn merge_all(e: &mut Engine, n: usize) {
        for i in 1..=n {
            let id = format!("T-{i}");
            start_work(e, &id);
            for command in [
                Command::RecordTests {
                    id: id.clone(),
                    passed: true,
                    skipped: false,
                    output_tail: String::new(),
                },
                Command::StartReview {
                    id: id.clone(),
                    reviewer: "lead".into(),
                },
            ] {
                run(e, Actor::System, command);
            }
            run(
                e,
                Actor::Employee("lead".into()),
                Command::SubmitReview {
                    id: id.clone(),
                    verdict: ReviewVerdict::Approve,
                    body: String::new(),
                },
            );
            run(
                e,
                Actor::System,
                Command::RecordMerge {
                    id,
                    commit: "abc".into(),
                },
            );
        }
    }

    /// A round for `n` merged tickets, with Quinn on the team.
    fn round_of(n: usize) -> Engine {
        let mut e = team(n);
        // Dev can only build one ticket at a time, so merge them one after another.
        merge_all(&mut e, n);
        hire(&mut e, "qa-engineer", "Quinn");
        run(
            &mut e,
            Actor::System,
            Command::RecordPromotion {
                commit: "def".into(),
            },
        );
        e
    }

    fn env_ok(e: &mut Engine, step: EnvStep) {
        run(
            e,
            Actor::System,
            Command::RecordEnvRun {
                step,
                ok: true,
                skipped: false,
                output_tail: String::new(),
            },
        );
    }

    fn qa_session(ticket: &str) -> Action {
        Action::OpenSession {
            ticket: ticket.into(),
            employee: "quinn".into(),
            kind: SessionKind::Qa,
        }
    }

    fn pass(e: &mut Engine, id: &str) {
        run(
            e,
            Actor::Employee("quinn".into()),
            Command::SubmitQaVerdict {
                id: id.into(),
                verdict: QaVerdict::Pass,
                criteria: vec![],
                notes: String::new(),
                bug: None,
            },
        );
    }

    #[test]
    fn dev_promotes_to_qa_while_another_ticket_is_still_being_built() {
        let mut e = team(2);
        merge_all(&mut e, 1);
        // T-2 is still being built; T-1 already waits for QA.
        start_work(&mut e, "T-2");
        assert!(next_actions(&e, &ctx()).contains(&Action::Promote));
    }

    #[test]
    fn a_failed_story_waits_for_its_bug_before_the_next_round() {
        let mut e = team(1);
        merge_all(&mut e, 1);
        e.state.tickets[0].checks.insert("qa_failed".into());
        let mut bug = e.state.tickets[0].clone();
        bug.id = "T-9".into();
        bug.found_in = Some("T-1".into());
        bug.state = "in_progress".into();
        bug.checks.clear();
        e.state.tickets.push(bug);
        assert!(
            !next_actions(&e, &ctx()).contains(&Action::Promote),
            "the fix is not merged yet"
        );
        e.state.tickets[1].state = IN_QA.into();
        assert!(next_actions(&e, &ctx()).contains(&Action::Promote));
    }

    #[test]
    fn tickets_that_arrive_during_a_round_get_the_next_one() {
        let mut e = team(2);
        merge_all(&mut e, 1);
        hire(&mut e, "qa-engineer", "Quinn");
        run(
            &mut e,
            Actor::System,
            Command::RecordPromotion {
                commit: "def".into(),
            },
        );
        start_work(&mut e, "T-2");
        merge_all_from(&mut e, 2);
        assert!(
            !next_actions(&e, &ctx()).contains(&Action::Promote),
            "the round is still running"
        );
        e.state.qa.as_mut().expect("round").status = QaRoundStatus::Closed;
        assert!(next_actions(&e, &ctx()).contains(&Action::Promote));
    }

    /// Finish T-`from` (already started) the way `merge_all` does.
    fn merge_all_from(e: &mut Engine, from: usize) {
        let id = format!("T-{from}");
        for command in [
            Command::RecordTests {
                id: id.clone(),
                passed: true,
                skipped: false,
                output_tail: String::new(),
            },
            Command::StartReview {
                id: id.clone(),
                reviewer: "lead".into(),
            },
        ] {
            run(e, Actor::System, command);
        }
        run(
            e,
            Actor::Employee("lead".into()),
            Command::SubmitReview {
                id: id.clone(),
                verdict: ReviewVerdict::Approve,
                body: String::new(),
            },
        );
        run(
            e,
            Actor::System,
            Command::RecordMerge {
                id,
                commit: "abc".into(),
            },
        );
    }

    #[test]
    fn a_request_promotes_before_the_sprint_is_done() {
        let mut e = team(2);
        merge_all(&mut e, 1);
        start_work(&mut e, "T-2");
        run(&mut e, Actor::Stakeholder, Command::PromoteToQa);
        assert!(next_actions(&e, &ctx()).contains(&Action::Promote));
    }

    #[test]
    fn promotion_waits_for_a_busy_claim_a_running_round_and_a_pause() {
        let mut e = team(1);
        merge_all(&mut e, 1);
        let busy = Context {
            busy: [KEY_PROMOTE.to_string()].into(),
            ..ctx()
        };
        assert!(!next_actions(&e, &busy).contains(&Action::Promote));
        run(&mut e, Actor::Stakeholder, Command::PauseDispatch);
        assert!(next_actions(&e, &ctx()).is_empty());
        run(&mut e, Actor::Stakeholder, Command::ResumeDispatch);
        hire(&mut e, "qa-engineer", "Quinn");
        run(
            &mut e,
            Actor::System,
            Command::RecordPromotion {
                commit: "def".into(),
            },
        );
        assert!(!next_actions(&e, &ctx()).contains(&Action::Promote));
    }

    #[test]
    fn the_environment_comes_up_and_is_tested_before_any_agent_starts() {
        let mut e = round_of(1);
        assert_eq!(
            next_actions(&e, &ctx()),
            [Action::RunEnv { step: EnvStep::Up }]
        );
        let busy = Context {
            busy: [KEY_QA_ENV.to_string()].into(),
            ..ctx()
        };
        assert!(next_actions(&e, &busy).is_empty());
        env_ok(&mut e, EnvStep::Up);
        assert_eq!(
            next_actions(&e, &ctx()),
            [Action::RunEnv {
                step: EnvStep::Test
            }]
        );
        env_ok(&mut e, EnvStep::Test);
        assert_eq!(next_actions(&e, &ctx()), [qa_session("T-1")]);
    }

    #[test]
    fn a_failed_environment_waits_for_the_stakeholders_retry() {
        let mut e = round_of(1);
        run(
            &mut e,
            Actor::System,
            Command::RecordEnvRun {
                step: EnvStep::Up,
                ok: false,
                skipped: false,
                output_tail: "boom".into(),
            },
        );
        // Nothing runs on its own; the failure is only handed to the project manager.
        assert!(
            next_actions(&e, &ctx())
                .iter()
                .all(|a| matches!(a, Action::TriageFailure { .. }))
        );
        run(
            &mut e,
            Actor::Stakeholder,
            Command::RetryEnv {
                env: "qa".into(),
                step: EnvStep::Up,
            },
        );
        let actions = next_actions(&e, &ctx());
        assert!(actions.contains(&Action::RunEnv { step: EnvStep::Up }));
    }

    #[test]
    fn testers_take_tickets_up_to_their_room_and_the_agent_budget() {
        let mut e = round_of(2);
        env_ok(&mut e, EnvStep::Up);
        env_ok(&mut e, EnvStep::Test);
        // Quinn can test one ticket at a time.
        assert_eq!(next_actions(&e, &ctx()), [qa_session("T-1")]);
        let running = Context {
            ticket_sessions: [("quinn".to_string(), "T-1".to_string())].into(),
            ..ctx()
        };
        assert!(next_actions(&e, &running).is_empty());
        // A second tester picks up the other ticket.
        hire(&mut e, "qa-engineer", "Quentin");
        assert_eq!(
            next_actions(&e, &running),
            [Action::OpenSession {
                ticket: "T-2".into(),
                employee: "quentin".into(),
                kind: SessionKind::Qa
            }]
        );
        let full = Context {
            sessions_open: 3,
            ..running
        };
        assert!(next_actions(&e, &full).is_empty());
    }

    #[test]
    fn the_round_closes_when_every_ticket_has_a_verdict() {
        let mut e = round_of(1);
        env_ok(&mut e, EnvStep::Up);
        env_ok(&mut e, EnvStep::Test);
        pass(&mut e, "T-1");
        // Pass signs the ticket off; the verdict criteria are not needed with none set.
        assert_eq!(next_actions(&e, &ctx()), [Action::CloseRound]);
        let busy = Context {
            busy: [KEY_QA_CLOSE.to_string()].into(),
            ..ctx()
        };
        assert!(next_actions(&e, &busy).is_empty());
    }

    #[test]
    fn qa_does_not_move_while_a_release_is_in_flight() {
        let mut e = round_of(1);
        env_ok(&mut e, EnvStep::Up);
        env_ok(&mut e, EnvStep::Test);
        pass(&mut e, "T-1");
        run(&mut e, Actor::System, Command::CloseQaRound);
        // A new ticket reaches QA while the release sits at the gate.
        let mut later = e.clone();
        later.state.tickets.push(Ticket {
            id: "T-9".into(),
            state: "in_qa".into(),
            ..later.state.tickets[0].clone()
        });
        later.state.tickets[1].checks.remove("qa_passed");
        later.state.tickets[1].checks.remove("released");
        assert!(!next_actions(&later, &ctx()).contains(&Action::Promote));
        assert!(
            e.decide(&Actor::Stakeholder, Command::PromoteToQa).is_err(),
            "the engine refuses too"
        );
    }

    #[test]
    fn an_approved_release_is_merged_then_deployed() {
        let mut e = round_of(1);
        env_ok(&mut e, EnvStep::Up);
        env_ok(&mut e, EnvStep::Test);
        pass(&mut e, "T-1");
        run(&mut e, Actor::System, Command::CloseQaRound);
        // At the gate, nothing runs.
        assert!(next_actions(&e, &ctx()).is_empty());
        run(
            &mut e,
            Actor::Stakeholder,
            Command::ApproveRelease {
                id: "R-1".into(),
                version: "v0.1.0".into(),
                notes: "n".into(),
            },
        );
        let release = Action::Release { id: "R-1".into() };
        assert_eq!(next_actions(&e, &ctx()), [release]);
        run(
            &mut e,
            Actor::System,
            Command::RecordRelease {
                id: "R-1".into(),
                tag: "v0.1.0".into(),
                commit: "abc".into(),
            },
        );
        let deploy = Action::Deploy { id: "R-1".into() };
        assert_eq!(next_actions(&e, &ctx()), std::slice::from_ref(&deploy));
        // A failed deploy waits for the retry.
        run(
            &mut e,
            Actor::System,
            Command::RecordDeploy {
                id: "R-1".into(),
                ok: false,
                skipped: false,
                output_tail: String::new(),
            },
        );
        assert!(next_actions(&e, &ctx()).is_empty());
        run(
            &mut e,
            Actor::Stakeholder,
            Command::RetryEnv {
                env: "prod".into(),
                step: EnvStep::Deploy,
            },
        );
        assert_eq!(next_actions(&e, &ctx()), [deploy]);
    }

    fn limit_backend(e: &mut Engine, backend: &str) {
        run(
            e,
            Actor::System,
            Command::RecordBackendLimit {
                backend: backend.into(),
                until: None,
                message: "limit".into(),
            },
        );
    }

    #[test]
    fn nothing_starts_on_a_rate_limited_or_signed_out_backend() {
        let mut e = team(1);
        let backend = e
            .state
            .employee("dev")
            .expect("dev")
            .employee
            .backend
            .clone();
        limit_backend(&mut e, &backend);
        assert!(next_actions(&e, &ctx()).is_empty());
        run(
            &mut e,
            Actor::System,
            Command::RecordBackendRecovered {
                backend: backend.clone(),
            },
        );
        assert_eq!(next_actions(&e, &ctx()), [start("T-1")]);
        let halted = Context {
            halted_backends: [backend].into(),
            ..ctx()
        };
        assert!(next_actions(&e, &halted).is_empty());
    }

    #[test]
    fn a_limited_backend_gets_no_session_review_or_tester() {
        let mut e = team(1);
        start_work(&mut e, "T-1");
        let backend = e
            .state
            .employee("dev")
            .expect("dev")
            .employee
            .backend
            .clone();
        limit_backend(&mut e, &backend);
        assert!(next_actions(&e, &ctx()).is_empty(), "no developer session");
    }

    #[test]
    fn chat_sessions_do_not_hold_a_slot_but_ticket_sessions_do() {
        let e = team(2);
        // The caller counts ticket sessions only, so two open tickets leave room for one.
        let full = Context {
            max_concurrent: 2,
            sessions_open: 2,
            ..Context::default()
        };
        assert!(next_actions(&e, &full).is_empty());
        let room = Context {
            sessions_open: 1,
            ..full
        };
        assert_eq!(next_actions(&e, &room).len(), 1);
    }

    #[test]
    fn a_developer_who_has_not_asked_for_review_is_waiting() {
        let mut e = team(1);
        start_work(&mut e, "T-1");
        assert!(session_is_waiting(&e, "dev", "T-1"));
        // Someone else's session on the ticket is not waiting on it.
        assert!(!session_is_waiting(&e, "lead", "T-1"));
        assert!(!session_is_waiting(&e, "dev", "T-9"));
        // Asking the stakeholder is waiting on a person, not stuck.
        run(
            &mut e,
            Actor::Employee("dev".into()),
            Command::AskStakeholder {
                question: "which db?".into(),
            },
        );
        assert!(!session_is_waiting(&e, "dev", "T-1"));
    }

    #[test]
    fn a_developer_who_asked_for_review_is_not_waiting() {
        let mut e = team(1);
        start_work(&mut e, "T-1");
        run(
            &mut e,
            Actor::Employee("dev".into()),
            Command::RequestReview {
                id: "T-1".into(),
                summary: "done".into(),
            },
        );
        assert!(!session_is_waiting(&e, "dev", "T-1"));
    }

    #[test]
    fn backend_limits_are_recorded_once_per_reset_time_and_cleared_on_recovery() {
        let mut e = team(1);
        let limit = |until| Command::RecordBackendLimit {
            backend: "claude".into(),
            until,
            message: "hit".into(),
        };
        assert_eq!(e.decide(&Actor::System, limit(None)).expect("ok").len(), 1);
        run(&mut e, Actor::System, limit(None));
        assert!(e.state.backend_limits.contains_key("claude"));
        // The same limit again is not news; a new reset time is.
        assert!(
            e.decide(&Actor::System, limit(None))
                .expect("ok")
                .is_empty()
        );
        let later = chrono::DateTime::from_timestamp(4_000_000_000, 0);
        assert_eq!(e.decide(&Actor::System, limit(later)).expect("ok").len(), 1);
        assert!(
            e.decide(&Actor::Employee("dev".into()), limit(None))
                .is_err(),
            "only Chillax records limits"
        );
        let recovered = Command::RecordBackendRecovered {
            backend: "claude".into(),
        };
        run(&mut e, Actor::System, recovered.clone());
        assert!(e.state.backend_limits.is_empty());
        assert!(e.decide(&Actor::System, recovered).expect("ok").is_empty());
    }

    #[test]
    fn only_the_stakeholder_kicks_the_dispatcher() {
        let e = team(1);
        assert_eq!(
            e.decide(&Actor::Stakeholder, Command::Kick).expect("ok"),
            [EventKind::DispatchKicked]
        );
        assert!(
            e.decide(&Actor::Employee("dev".into()), Command::Kick)
                .is_err()
        );
        assert!(e.decide(&Actor::System, Command::Kick).is_err());
    }

    #[test]
    fn only_the_stakeholder_refreshes_usage() {
        let e = team(1);
        assert_eq!(
            e.decide(&Actor::Stakeholder, Command::RefreshUsage)
                .expect("ok"),
            [EventKind::UsageRefreshRequested]
        );
        assert!(
            e.decide(&Actor::Employee("dev".into()), Command::RefreshUsage)
                .is_err()
        );
    }

    fn fail_tests(e: &mut Engine, id: &str, output: &str) {
        run(
            e,
            Actor::System,
            Command::RecordTests {
                id: id.into(),
                passed: false,
                skipped: false,
                output_tail: output.into(),
            },
        );
    }

    fn triage_of(e: &Engine, ctx: &Context) -> Vec<Action> {
        next_actions(e, ctx)
            .into_iter()
            .filter(|a| matches!(a, Action::TriageFailure { .. }))
            .collect()
    }

    fn only_failure(e: &Engine) -> String {
        assert_eq!(e.state.failures.len(), 1, "{:?}", e.state.failures);
        e.state.failures[0].fingerprint.clone()
    }

    #[test]
    fn the_first_failed_test_run_stays_with_the_developer_and_the_second_is_triaged() {
        let mut e = team(1);
        start_work(&mut e, "T-1");
        fail_tests(&mut e, "T-1", "assertion failed in 1.2s");
        assert!(e.state.failures.is_empty());
        assert!(triage_of(&e, &ctx()).is_empty());
        fail_tests(&mut e, "T-1", "assertion failed in 3.4s");
        let fp = only_failure(&e);
        assert_eq!(
            triage_of(&e, &ctx()),
            [Action::TriageFailure {
                key: triage_key(&fp),
                fingerprint: fp,
                pm: Some("pam".into()),
            }]
        );
    }

    #[test]
    fn tests_that_pass_in_between_reset_the_streak_and_resolve_the_failure() {
        let mut e = team(1);
        start_work(&mut e, "T-1");
        fail_tests(&mut e, "T-1", "boom");
        run(
            &mut e,
            Actor::System,
            Command::RecordTests {
                id: "T-1".into(),
                passed: true,
                skipped: false,
                output_tail: String::new(),
            },
        );
        assert!(e.state.test_streaks.is_empty());
        assert!(e.state.failures.is_empty());
        // Two more in a row file a failure; a later pass resolves it before a bug exists.
        fail_tests(&mut e, "T-1", "boom");
        fail_tests(&mut e, "T-1", "boom");
        assert_eq!(triage_of(&e, &ctx()).len(), 1);
        run(
            &mut e,
            Actor::System,
            Command::RecordTests {
                id: "T-1".into(),
                passed: true,
                skipped: false,
                output_tail: String::new(),
            },
        );
        assert!(triage_of(&e, &ctx()).is_empty());
    }

    #[test]
    fn triage_waits_for_a_free_project_manager_on_a_working_backend() {
        let mut e = team(1);
        start_work(&mut e, "T-1");
        fail_tests(&mut e, "T-1", "boom");
        fail_tests(&mut e, "T-1", "boom");
        let busy = Context {
            busy: [triage_key(&only_failure(&e))].into(),
            ..ctx()
        };
        assert!(triage_of(&e, &busy).is_empty());
        let backend = e
            .state
            .employee("pam")
            .expect("pam")
            .employee
            .backend
            .clone();
        limit_backend(&mut e, &backend);
        assert!(triage_of(&e, &ctx()).is_empty(), "backend is limited");
        run(
            &mut e,
            Actor::System,
            Command::RecordBackendRecovered { backend },
        );
        apply(
            &mut e,
            EventKind::AgentMessageChunk {
                employee: "pam".into(),
                text: "thinking".into(),
            },
        );
        assert!(triage_of(&e, &ctx()).is_empty(), "the PM is mid-turn");
    }

    #[test]
    fn a_failure_is_briefed_three_times_and_then_given_up() {
        let mut e = team(1);
        start_work(&mut e, "T-1");
        fail_tests(&mut e, "T-1", "boom");
        fail_tests(&mut e, "T-1", "boom");
        let fp = only_failure(&e);
        for _ in 0..3 {
            let actions = triage_of(&e, &ctx());
            assert!(matches!(
                actions[..],
                [Action::TriageFailure { pm: Some(_), .. }]
            ));
            run(
                &mut e,
                Actor::System,
                Command::RecordTriageBriefed {
                    fingerprint: fp.clone(),
                },
            );
        }
        assert!(matches!(
            triage_of(&e, &ctx())[..],
            [Action::TriageFailure { pm: None, .. }]
        ));
        run(
            &mut e,
            Actor::System,
            Command::AbandonTriage { fingerprint: fp },
        );
        assert!(triage_of(&e, &ctx()).is_empty());
    }

    fn file_bug(e: &mut Engine, title: &str) -> String {
        run(
            e,
            Actor::Employee("pam".into()),
            Command::CreateTicket {
                kind: TicketKind::Bug,
                title: title.into(),
                description: String::new(),
                acceptance_criteria: vec![],
                estimate: None,
                epic: None,
                external: None,
            },
        );
        e.state.tickets.last().expect("a ticket").id.clone()
    }

    #[test]
    fn a_linked_bug_joins_the_active_sprint_ready_and_dedupes_the_failure() {
        let mut e = team(1);
        start_work(&mut e, "T-1");
        fail_tests(&mut e, "T-1", "boom");
        fail_tests(&mut e, "T-1", "boom");
        let fp = only_failure(&e);
        let bug = file_bug(&mut e, "Tests fail");
        run(
            &mut e,
            Actor::Stakeholder,
            Command::AssignTicket {
                id: bug.clone(),
                assignee: Some("lead".into()),
            },
        );
        run(
            &mut e,
            Actor::Employee("pam".into()),
            Command::LinkTriageBug {
                fingerprint: fp.clone(),
                bug: bug.clone(),
            },
        );
        let ticket = e.state.ticket(&bug).expect("bug");
        assert_eq!(ticket.sprint.as_deref(), Some("SP-1"));
        assert_eq!(Some(ticket.state.as_str()), e.workflow.planned_state());
        assert!(
            e.state
                .sprint("SP-1")
                .expect("sprint")
                .tickets
                .contains(&bug)
        );
        assert!(triage_of(&e, &ctx()).is_empty());
        // Another bug for the same failure is refused while the first is open.
        let other = file_bug(&mut e, "Same again");
        let err = e
            .decide(
                &Actor::Employee("pam".into()),
                Command::LinkTriageBug {
                    fingerprint: fp.clone(),
                    bug: other,
                },
            )
            .expect_err("duplicate");
        assert!(err.to_string().contains(&bug), "{err}");
        // The same failure again only counts.
        fail_tests(&mut e, "T-1", "boom");
        assert_eq!(e.state.failures[0].occurrences, 2);
        assert!(triage_of(&e, &ctx()).is_empty());
        assert!(
            e.decide(
                &Actor::Stakeholder,
                Command::LinkTriageBug {
                    fingerprint: fp,
                    bug,
                }
            )
            .is_err()
        );
    }

    #[test]
    fn a_rebase_conflict_is_triaged_once_its_resolve_turns_run_out_and_a_clean_rebase_resolves_it()
    {
        let mut e = team(1);
        start_work(&mut e, "T-1");
        run(
            &mut e,
            Actor::System,
            Command::RecordRebase {
                id: "T-1".into(),
                outcome: RebaseOutcome::Conflict,
                note: "CONFLICT in a.rs".into(),
            },
        );
        assert!(
            e.state.failures.is_empty(),
            "the developer resolves it first"
        );
        for _ in 0..=crate::engine::MAX_CONFLICT_ATTEMPTS {
            run(
                &mut e,
                Actor::System,
                Command::RecordSync {
                    id: "T-1".into(),
                    outcome: crate::SyncOutcome::Conflict,
                    files: vec!["a.rs".into()],
                    note: "CONFLICT in a.rs".into(),
                },
            );
        }
        assert_eq!(e.state.failures[0].class, crate::FailureClass::Rebase);
        assert_eq!(triage_of(&e, &ctx()).len(), 1);
        run(
            &mut e,
            Actor::System,
            Command::RecordRebase {
                id: "T-1".into(),
                outcome: RebaseOutcome::Rebased,
                note: String::new(),
            },
        );
        assert!(triage_of(&e, &ctx()).is_empty());
    }

    #[test]
    fn a_qa_environment_failure_is_triaged_and_the_retry_that_works_resolves_it() {
        let mut e = round_of(1);
        run(
            &mut e,
            Actor::System,
            Command::RecordEnvRun {
                step: EnvStep::Up,
                ok: false,
                skipped: false,
                output_tail: "port 8080 in use".into(),
            },
        );
        assert_eq!(e.state.failures[0].subject, "qa up");
        assert_eq!(triage_of(&e, &ctx()).len(), 1);
        run(
            &mut e,
            Actor::Stakeholder,
            Command::RetryEnv {
                env: "qa".into(),
                step: EnvStep::Up,
            },
        );
        run(
            &mut e,
            Actor::System,
            Command::RecordEnvRun {
                step: EnvStep::Up,
                ok: true,
                skipped: false,
                output_tail: String::new(),
            },
        );
        assert!(triage_of(&e, &ctx()).is_empty());
    }

    #[test]
    fn bugs_go_to_the_tech_lead_by_tag_and_a_lead_who_builds_may_review_only_when_alone() {
        let mut e = team(1);
        // Make the lead a developer too (data: owns_states in the role file).
        let lead = Role::parse(
            "---\nid: tech-lead\ntitle: Lead\ntags: [reviewer, tech-lead]\nowns_states: \
             [in_review, in_progress]\n---\nprompt\n",
        )
        .expect("role");
        e.roles.insert(lead);
        assert_eq!(crate::triage::bug_owner(&e).as_deref(), Some("lead"));
        run(
            &mut e,
            Actor::Employee("pam".into()),
            Command::AssignTicket {
                id: "T-1".into(),
                assignee: Some("lead".into()),
            },
        );
        start_work(&mut e, "T-1");
        run(
            &mut e,
            Actor::System,
            Command::RecordTests {
                id: "T-1".into(),
                passed: true,
                skipped: false,
                output_tail: String::new(),
            },
        );
        // Lead is the only reviewer: reviews their own fix.
        assert!(e.sole_reviewer("lead"));
        assert_eq!(
            next_actions(&e, &ctx()),
            [Action::StartReview {
                ticket: "T-1".into(),
                reviewer: "lead".into()
            }]
        );
        // With a second reviewer, no self-review.
        hire(&mut e, "tech-lead", "Second");
        assert!(!e.sole_reviewer("lead"));
        assert_eq!(
            next_actions(&e, &ctx()),
            [Action::StartReview {
                ticket: "T-1".into(),
                reviewer: "second".into()
            }]
        );
    }
}
