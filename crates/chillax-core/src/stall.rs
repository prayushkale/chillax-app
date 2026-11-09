//! The stall watchdog: when open sprint work has not moved for a while, close
//! agent turns that hang and hand the board to the project manager with a reason per
//! ticket. Pure, so the rules are unit-tested without IO; the runner keeps the clock
//! (`StallCtx`) and performs the actions `dispatch::next_actions` picks.

use std::time::Duration;

use crate::dispatch::{Context, why_idle};
use crate::engine::{Engine, State};
use crate::model::*;

/// The title of the stakeholder alert raised when the project manager could not get the
/// board moving; the runner clears it once work moves again.
pub const ALERT_TITLE: &str = "Work has stalled";

/// A session in the middle of a turn, as the runtime sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusySession {
    pub employee: EmployeeId,
    /// The ticket of a ticket session; none for a chat session.
    pub ticket: Option<TicketId>,
    /// How long the agent has shown no activity (no output, tool call or plan).
    pub quiet: Duration,
}

/// What the runner remembers about the current stall window. The decision has no clock,
/// so every age is handed in as a duration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StallCtx {
    /// `[team] stall_minutes`. Zero is off, like a `None` stall in the [`Context`].
    pub threshold: Duration,
    /// Time since the board last moved (or since startup, a resume, or new open work).
    pub idle_for: Duration,
    /// Time since the project manager was briefed about this stall, if he was.
    pub briefed_for: Option<Duration>,
    /// The stakeholder was already told about this stall.
    pub alerted: bool,
    /// Sessions mid-turn right now.
    pub busy_sessions: Vec<BusySession>,
}

/// Tickets that should be moving: in an active sprint, not dropped, not finished.
pub fn open_work(state: &State) -> Vec<&Ticket> {
    state
        .tickets
        .iter()
        .filter(|t| t.dropped.is_none() && !state.is_finished(t))
        .filter(|t| {
            t.sprint
                .as_deref()
                .and_then(|sp| state.sprint(sp))
                .is_some_and(|sp| sp.status == SprintStatus::Active)
        })
        .collect()
}

/// Whether the watchdog may act at all: dispatch runs and there is open sprint work.
/// Outside that the runner restarts the window, so a pause or an empty board never
/// counts as stalled time.
pub fn watching(engine: &Engine) -> bool {
    !engine.state.dispatch_paused && !open_work(&engine.state).is_empty()
}

/// Whether the board has stalled: watched, and nothing moved for the whole threshold.
pub fn stalled(engine: &Engine, stall: &StallCtx) -> bool {
    !stall.threshold.is_zero() && stall.idle_for >= stall.threshold && watching(engine)
}

/// Sessions whose turn hangs: busy with no agent activity for the whole threshold. An
/// employee waiting on the stakeholder (a question or a permission) is not hung.
pub fn hung<'a>(engine: &Engine, stall: &'a StallCtx) -> Vec<&'a BusySession> {
    stall
        .busy_sessions
        .iter()
        .filter(|s| s.quiet >= stall.threshold)
        .filter(|s| !waits_on_stakeholder(&engine.state, &s.employee))
        .collect()
}

fn waits_on_stakeholder(state: &State, employee: &str) -> bool {
    state.inbox.iter().any(|item| {
        matches!(item, InboxItem::Question { employee: e, .. }
            | InboxItem::Permission { employee: e, .. } if e == employee)
    })
}

fn name_of<'a>(state: &'a State, employee: &'a str) -> &'a str {
    state
        .employee(employee)
        .map_or(employee, |e| e.employee.name.as_str())
}

/// "1 minute", "20 minutes": rounded up, never zero.
fn minutes(d: Duration) -> String {
    match d.as_secs().div_ceil(60).max(1) {
        1 => "1 minute".to_string(),
        n => format!("{n} minutes"),
    }
}

/// The project manager's briefing: how long the board has not moved, each open ticket with
/// its state, owner and why it is not moving, the hung sessions Chillax closed, and what
/// to do about it.
pub fn briefing(engine: &Engine, ctx: &Context, idle_for: Duration, closed: &[String]) -> String {
    let state = &engine.state;
    let mut lines = vec![format!(
        "[Chillax] Stall check: no ticket has moved for {} and work is open. \
         Find out why and get it moving again.",
        minutes(idle_for)
    )];
    lines.push(String::new());
    lines.push("Open tickets (state, assignee: why it is not moving):".to_string());
    for t in open_work(state) {
        let who = t
            .assignee
            .as_deref()
            .map_or("unassigned", |a| name_of(state, a));
        lines.push(format!(
            "- {} \"{}\" {}, {who}: {}",
            t.id,
            t.title,
            t.state,
            why_idle(engine, ctx, t)
        ));
    }
    if !closed.is_empty() {
        lines.push(String::new());
        lines.push(format!(
            "Hung turns Chillax closed (no agent activity for {} or more): {}. Their work \
             restarts with a fresh briefing.",
            ctx.stall
                .as_ref()
                .map_or_else(|| minutes(idle_for), |s| minutes(s.threshold)),
            closed.join(", ")
        ));
    }
    lines.push(String::new());
    lines.push(
        "Fix it with your tools: retry_action (key = ticket id) for a ticket cooling down \
         after a failure, restart_teammate for a failed or stuck teammate, assign to move work \
         to a free teammate, transition for a ticket in the wrong state, message_teammate to \
         unstick someone, create_story to split out or unblock a blocker, hire_teammate if \
         nobody can take the work. Use ask_stakeholder only when a human decision is truly \
         needed. Then report_progress with what you found and did."
            .to_string(),
    );
    lines.join("\n")
}

/// The stakeholder alert when a briefing did not get the board moving.
pub fn alert_detail(engine: &Engine, ctx: &Context, idle_for: Duration) -> String {
    let state = &engine.state;
    let reasons: Vec<String> = open_work(state)
        .into_iter()
        .map(|t| format!("{} {}: {}", t.id, t.state, why_idle(engine, ctx, t)))
        .collect();
    format!(
        "No ticket has moved for {} and handing the board to the project manager \
         did not get it going. {}",
        minutes(idle_for),
        reasons.join("; ")
    )
}
