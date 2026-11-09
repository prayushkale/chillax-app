//! The project manager's board check-in: a short, deterministic digest of the
//! open work and the team, plus a fingerprint that tells whether anything changed since
//! the last check-in. Pure, so the rules are unit-tested without IO; the dispatcher
//! decides when to send it (`dispatch::checkin_actions`) and the runner delivers it.

use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::engine::{Engine, State};
use crate::model::*;
use crate::notify::{HireState, compact, hire_summaries};
use crate::workflow::IN_PROGRESS;

/// Consecutive check-in intervals with an unchanged board after which a check-in is sent
/// anyway: a stuck board is by definition an unchanged one.
pub const FORCE_AFTER: u32 = 3;

/// Intervals without change after which a ticket is listed as stalled.
pub const STALLED_AFTER: u32 = 2;

/// The digest never grows past this many characters.
const MAX_CHARS: usize = 1500;

/// Names listed on one line before the rest is summarised as `+N`.
const MAX_LISTED: usize = 12;

/// Tickets that still need work: not dropped, not finished, and either unplanned or in
/// the active sprint. Urgent ones first, then by ticket number.
fn open_tickets(engine: &Engine) -> Vec<&Ticket> {
    let state = &engine.state;
    let mut open: Vec<&Ticket> = state
        .tickets
        .iter()
        .filter(|t| t.dropped.is_none() && !state.is_finished(t))
        .filter(|t| {
            t.sprint.as_deref().is_none_or(|sp| {
                state
                    .sprint(sp)
                    .is_some_and(|sp| sp.status == SprintStatus::Active)
            })
        })
        .collect();
    open.sort_by_key(|t| (!t.urgent, number(&t.id), t.id.clone()));
    open
}

/// The numeric part of `T-12`, so `T-9` sorts before `T-10`.
fn number(id: &str) -> u64 {
    id.rsplit('-')
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(u64::MAX)
}

/// Whether there is any open work to check in on.
pub fn has_open_work(engine: &Engine) -> bool {
    !open_tickets(engine).is_empty()
}

fn name_of<'a>(state: &'a State, employee: &'a str) -> &'a str {
    state
        .employee(employee)
        .map_or(employee, |e| e.employee.name.as_str())
}

fn list(items: &[String]) -> String {
    if items.len() <= MAX_LISTED {
        return items.join(" ");
    }
    format!(
        "{} +{}",
        items[..MAX_LISTED].join(" "),
        items.len() - MAX_LISTED
    )
}

/// The board as one short text, or `None` when there is no open work. `stalled` maps a
/// ticket to the check-in intervals it has not changed in (the runner remembers them; an
/// empty map names nothing); `max_parallel` is `[budget] max_concurrent_agents` when known. One line per topic, at most [`MAX_CHARS`].
pub fn digest(
    engine: &Engine,
    stalled: &BTreeMap<TicketId, u32>,
    max_parallel: Option<usize>,
) -> Option<String> {
    let open = open_tickets(engine);
    if open.is_empty() {
        return None;
    }
    let state = &engine.state;
    let cap = engine.team.max_team_size as usize;
    let hires = hire_summaries(state);
    let hire_state = |id: &str| hires.iter().find(|h| h.id == id).map(|h| h.state);
    let mut lines = Vec::new();

    lines.push(format!(
        "Team {}/{cap}. {}",
        state.employees.len(),
        compact(&hires, 500)
    ));

    let counts: Vec<String> = engine
        .workflow
        .states
        .iter()
        .filter(|s| s.key != engine.workflow.terminal_state())
        .map(|s| {
            let n = open.iter().filter(|t| t.state == s.key).count();
            format!("{} {n}", s.key)
        })
        .collect();
    let limit = max_parallel.map_or(String::new(), |n| format!(" Parallel limit {n}."));
    lines.push(format!(
        "Open: {} ({}).{limit}",
        open.len(),
        counts.join(", ")
    ));

    for t in open.iter().filter(|t| t.urgent) {
        let who = t
            .assignee
            .as_deref()
            .map_or("unassigned", |a| name_of(state, a));
        lines.push(format!("URGENT: {} {} {who}", t.id, t.state));
    }

    let stuck: Vec<String> = open
        .iter()
        .filter_map(|t| {
            let n = *stalled.get(&t.id)?;
            let who = t
                .assignee
                .as_deref()
                .map_or("unassigned", |a| name_of(state, a));
            (n >= STALLED_AFTER).then(|| format!("{} {} {who} x{n}", t.id, t.state))
        })
        .collect();
    if !stuck.is_empty() {
        lines.push(format!(
            "Stalled (no change over several check-ins): {}",
            stuck.join(", ")
        ));
    }

    let unassigned: Vec<String> = open
        .iter()
        .filter(|t| t.assignee.is_none())
        .map(|t| t.id.clone())
        .collect();
    let waiting: Vec<String> = open
        .iter()
        .filter(|t| !state.blockers_clear(t))
        .map(|t| {
            let open_blockers: Vec<&str> = t
                .blocked_by
                .iter()
                .filter(|b| !blocker_closed(state, b))
                .map(String::as_str)
                .collect();
            format!("{}<-{}", t.id, open_blockers.join(","))
        })
        .collect();
    let mut parts = Vec::new();
    if !unassigned.is_empty() {
        parts.push(format!("Unassigned: {}.", list(&unassigned)));
    }
    if !waiting.is_empty() {
        parts.push(format!("Waiting on blockers: {}.", list(&waiting)));
    }
    if !parts.is_empty() {
        lines.push(parts.join(" "));
    }

    let planned = engine.workflow.planned_state();
    let initial = engine.workflow.initial_state();
    let startable: Vec<String> = open
        .iter()
        .filter(|t| t.state == initial || Some(t.state.as_str()) == planned)
        .filter(|t| state.blockers_clear(t))
        .filter(|t| {
            t.assignee
                .as_deref()
                .is_none_or(|a| matches!(hire_state(a), Some(HireState::Idle | HireState::Offline)))
        })
        .map(|t| t.id.clone())
        .collect();
    if !startable.is_empty() {
        let free = free_developers(engine, &hires);
        let room = cap.saturating_sub(state.employees.len());
        lines.push(format!(
            "Independent & startable now (no open blockers, no assignee or idle owner): {}. \
             Free developers: {free}. Room to hire: {room}.",
            list(&startable)
        ));
    }

    let idle: Vec<String> = hires
        .iter()
        .filter(|h| matches!(h.state, HireState::Idle | HireState::Offline) && h.ticket.is_none())
        .filter_map(|h| {
            let e = state.employee(&h.id)?;
            let role = engine.roles.get(&e.employee.role)?;
            engine
                .workflow
                .role_owns(role, IN_PROGRESS)
                .then(|| format!("{} ({})", h.name, role.id))
        })
        .collect();
    if !idle.is_empty() {
        lines.push(format!(
            "Idle developers with no ticket: {}.",
            idle.join(", ")
        ));
    }

    let mut text = lines.join("\n");
    if text.chars().count() > MAX_CHARS {
        text = text.chars().take(MAX_CHARS - 3).collect::<String>() + "...";
    }
    Some(text)
}

/// A blocker is closed once it is merged or done (or dropped).
fn blocker_closed(state: &State, id: &str) -> bool {
    state.ticket(id).is_none_or(|b| {
        b.dropped.is_some() || state.is_finished(b) || b.checks.contains("merged_to_dev")
    })
}

/// Developers who could take a ticket now: their role builds, they are not stopped,
/// failed or waiting on the stakeholder, and they carry fewer tickets than they may.
fn free_developers(engine: &Engine, hires: &[crate::notify::HireSummary]) -> usize {
    engine
        .state
        .employees
        .iter()
        .filter(|e| {
            let state = hires
                .iter()
                .find(|h| h.id == e.employee.id)
                .map(|h| h.state);
            matches!(
                state,
                Some(HireState::Idle | HireState::Offline | HireState::Working)
            )
        })
        .filter_map(|e| {
            let role = engine.roles.get(&e.employee.role)?;
            engine.workflow.role_owns(role, IN_PROGRESS).then(|| {
                let load = engine
                    .state
                    .tickets
                    .iter()
                    .filter(|t| {
                        t.dropped.is_none()
                            && t.state == IN_PROGRESS
                            && t.assignee.as_deref() == Some(e.employee.id.as_str())
                    })
                    .count();
                load < role.max_parallel_tasks as usize
            })
        })
        .filter(|free| *free)
        .count()
}

/// A hash of everything a check-in would care about: each ticket's state, owner, facts
/// and blockers, and each hire's state. Equal fingerprints mean nothing moved. Within one
/// process only (std's hasher), which is all the runner's memory needs.
pub fn fingerprint(state: &State) -> String {
    let mut tickets: Vec<&Ticket> = state.tickets.iter().collect();
    tickets.sort_by(|a, b| a.id.cmp(&b.id));
    let mut h = DefaultHasher::new();
    for t in tickets {
        (
            &t.id,
            &t.state,
            &t.assignee,
            t.checks.len(),
            t.review_requested,
            t.branch.is_some(),
            &t.blocked_by,
            t.urgent,
            t.dropped.is_some(),
        )
            .hash(&mut h);
    }
    for hire in hire_summaries(state) {
        // No session yet reads the same as idle: the briefing itself opens the session.
        let label = match hire.state {
            HireState::Offline => HireState::Idle.label(),
            other => other.label(),
        };
        (&hire.id, label, &hire.ticket).hash(&mut h);
    }
    format!("{:016x}", h.finish())
}

/// The message the project manager receives.
pub fn briefing(digest: &str) -> String {
    format!(
        "[Chillax] Board check-in. Review it, then act: unblock/reassign/split stuck work, \
         hire (hire_teammate, reason required) only when independent tickets wait and no free \
         developer fits, never above the cap. Reply with a 2-line status. If nothing needs \
         changing say so and stop.\n\n{digest}"
    )
}
