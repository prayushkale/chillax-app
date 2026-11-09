//! Request intake: the stakeholder describes a need in plain
//! words; the project manager splits it into work items and assigns each one. This
//! module holds the pure parts: the briefing, the roster the project manager picks from,
//! and why a request is waiting.

use std::fmt::Write as _;

use crate::engine::{Engine, State};
use crate::model::*;

/// Times the project manager is briefed about one request before Chillax gives up.
pub const MAX_ATTEMPTS: u32 = 3;

/// Longest request text accepted.
pub const MAX_TEXT: usize = 8000;

/// Claim key of the dispatcher action for one request.
pub fn request_key(id: &str) -> String {
    format!("@request-{id}")
}

/// Open requests, oldest first.
pub fn open_requests(state: &State) -> impl Iterator<Item = &Request> {
    state.requests.iter().filter(|r| r.status.is_open())
}

/// The request the project manager takes next: the stakeholder's own before any employee
/// suggestion, then oldest first.
pub fn next_request(state: &State) -> Option<&Request> {
    open_requests(state).min_by_key(|r| r.from.is_some())
}

/// Does an accepted `ticket` start now? The stakeholder's `start_now` says so; an
/// employee's bug suggestion does for a bug ticket, while improvements wait for a sprint
/// the stakeholder approves.
fn starts_now(request: &Request, ticket: &Ticket) -> bool {
    request.start_now
        || (request.from.is_some()
            && request.kind == Some(SuggestionKind::Bug)
            && ticket.kind == TicketKind::Bug)
}

/// How the sprint gate treats the tickets of a `start_now` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartPlan {
    /// Not started now (the request did not ask, or the ticket is already planned).
    Backlog,
    /// Join this active sprint.
    Join(SprintId),
    /// No sprint is running: open one for the request and approve it, because the
    /// request itself is the stakeholder's approval.
    NewSprint,
}

/// Where a ticket filed for `request` goes. A sprint that is still waiting for approval
/// is left alone (the stakeholder is already deciding about it), so the ticket stays in
/// the backlog.
pub fn start_plan(engine: &Engine, request: &Request, ticket: &Ticket) -> StartPlan {
    let state = &engine.state;
    if !starts_now(request, ticket)
        || ticket.dropped.is_some()
        || ticket.sprint.is_some()
        || ticket.state != engine.workflow.initial_state()
        || engine.workflow.planned_state().is_none()
    {
        return StartPlan::Backlog;
    }
    match state.sprint_in_flight() {
        Some(sp) if sp.status == SprintStatus::Active => StartPlan::Join(sp.id.clone()),
        Some(_) => StartPlan::Backlog,
        None => StartPlan::NewSprint,
    }
}

/// Why a queued request is not moving, for the UIs. `None` when the project manager is
/// on it (or it is done). `plans(role_id)` says whether a role works the first workflow
/// state (the planner); the engine has that rule, the UIs pass what their role list says.
pub fn wait_reason(
    state: &State,
    request: &Request,
    plans: impl Fn(&str) -> bool,
) -> Option<String> {
    if !request.status.is_open() {
        return None;
    }
    let planners: Vec<_> = state
        .employees
        .iter()
        .filter(|e| plans(&e.employee.role))
        .collect();
    if planners.is_empty() {
        return Some("no project manager is hired: hire one and it will be picked up".into());
    }
    if planners
        .iter()
        .all(|e| state.backend_limits.contains_key(&e.employee.backend))
    {
        return Some("the project manager's agent is rate limited; it waits for the reset".into());
    }
    match request.status {
        RequestStatus::Submitted => Some("waiting for the project manager to be free".into()),
        _ => None,
    }
}

/// One line per employee: who they are, what they own, how loaded they are.
pub fn roster(engine: &Engine) -> String {
    let state = &engine.state;
    let terminal = engine.workflow.terminal_state();
    let mut out = String::new();
    for e in &state.employees {
        let role = engine.roles.get(&e.employee.role);
        let open: Vec<&str> = state
            .assignments(&e.employee.id)
            .into_iter()
            .filter(|t| t.state != terminal && t.dropped.is_none())
            .map(|t| t.id.as_str())
            .collect();
        let _ = write!(
            out,
            "- {} (id `{}`): {}",
            e.employee.name, e.employee.id, e.employee.role_title
        );
        if let Some(role) = role {
            if !role.tags.is_empty() {
                let _ = write!(out, "; tags {}", role.tags.join(", "));
            }
            let owned: Vec<&str> = state
                .workflow_states
                .iter()
                .map(String::as_str)
                .filter(|s| engine.workflow.role_owns(role, s))
                .collect();
            if !owned.is_empty() {
                let _ = write!(out, "; works {}", owned.join(", "));
            }
            let _ = write!(out, "; up to {} at once", role.max_parallel_tasks);
        }
        if state.backend_limits.contains_key(&e.employee.backend) {
            out.push_str("; agent rate limited right now");
        }
        if open.is_empty() {
            out.push_str("; no open tickets\n");
        } else {
            let _ = writeln!(out, "; {} open: {}", open.len(), open.join(" "));
        }
    }
    if out.is_empty() {
        out.push_str("(nobody is hired)\n");
    }
    out
}

/// What the project manager is told about a request.
pub fn briefing(engine: &Engine, request: &Request) -> String {
    let suggester = request
        .from
        .as_deref()
        .and_then(|id| engine.state.employee(id));
    let mut out = match suggester {
        Some(e) => {
            let seen = request
                .ticket
                .as_deref()
                .map(|t| format!(", noticed while on {t}"))
                .unwrap_or_default();
            format!(
                "[Chillax] Suggestion {} from {} ({}), a {}{seen}. Decide whether it is worth \
                 doing; if so turn it into work items and give each one an owner, otherwise \
                 decline it.\n\n\"\"\"\n{}\n\"\"\"\n\n",
                request.id,
                e.employee.name,
                e.employee.role_title,
                request.kind.map_or("suggestion", SuggestionKind::label),
                request.text.trim()
            )
        }
        None => format!(
            "[Chillax] Request {} from the stakeholder. Turn it into work items and give each \
             one an owner.\n\n\"\"\"\n{}\n\"\"\"\n\n",
            request.id,
            request.text.trim()
        ),
    };
    if suggester.is_some() {
        out.push_str(
            "Accept it only when it is real, specific and in scope: check `list_tickets` \
             for a duplicate first. File a bug as `kind` bug: it joins the active sprint and \
             starts at once. File an improvement as a story: it waits in the backlog for the \
             next sprint. Decline a vague, duplicate or out-of-scope one by calling \
             `finish_request` with the reason.\n\n",
        );
    } else if request.start_now {
        out.push_str(
            "They want this started now: the tickets you file for it join the active sprint \
             (or open one) and are ready at once, so there is no sprint to propose.\n\n",
        );
    } else {
        out.push_str(
            "They did not ask to start now: the tickets wait in the backlog for the next \
             sprint.\n\n",
        );
    }
    if !request.tickets.is_empty() {
        let _ = writeln!(
            out,
            "Already filed for this request (do not file them again): {}.\n",
            request.tickets.join(" ")
        );
    }
    out.push_str("Team, with what each person owns and carries:\n");
    out.push_str(&roster(engine));
    let _ = write!(
        out,
        "\nHow to do it:\n\
         1. Check `list_tickets` so you do not duplicate work that exists.\n\
         2. Split the request into small stories, bugs or tasks. A bug report is a bug; a new \
         capability is a story. Write 2-5 testable acceptance criteria for each, an estimate, \
         and `blocked_by` where order matters. Group them under an epic only when there are \
         several.\n\
         3. File each with `create_story`, passing `request_id` \"{id}\" and `assignee`: the \
         best fit by role and tags, and among equals the one with the fewest open tickets. \
         Developers build, the Tech Lead takes architecture-heavy and bug work. If nobody fits, \
         file it without an assignee and say which role to hire.\n\
         4. When done, call `finish_request` with `request_id` \"{id}\" and a short summary \
         (ticket, owner, why). If the request needs nothing filed (a duplicate, or a question), \
         call it with that explanation. Ask the stakeholder with `ask_stakeholder` only when the \
         request cannot be understood.\n\
         5. Reply with the list of what you created and who got each ticket.",
        id = request.id
    );
    out
}

/// The text of the Inbox note when a request is triaged: what was filed, and for whom.
pub fn note(state: &State, request: &Request, summary: &str) -> (String, String) {
    let title = match request.from.as_deref().and_then(|id| state.employee(id)) {
        Some(e) if request.tickets.is_empty() => {
            format!("Declined {}'s suggestion {}", e.employee.name, request.id)
        }
        Some(e) => format!("Filed {}'s suggestion {}", e.employee.name, request.id),
        None => format!("Request {} filed", request.id),
    };
    let mut detail = String::new();
    if request.tickets.is_empty() {
        detail.push_str("No tickets were needed.\n");
    }
    for id in &request.tickets {
        if let Some(t) = state.ticket(id) {
            let _ = writeln!(
                detail,
                "{} ({}, {}): {} -> {}",
                t.id,
                t.kind.label(),
                t.state,
                t.title.lines().next().unwrap_or(""),
                t.assignee
                    .as_deref()
                    .and_then(|a| state.employee(a))
                    .map_or("unassigned", |e| e.employee.name.as_str()),
            );
        }
    }
    let summary = summary.trim();
    if !summary.is_empty() {
        let _ = write!(detail, "\n{summary}");
    }
    (title, detail.trim_end().to_string())
}
