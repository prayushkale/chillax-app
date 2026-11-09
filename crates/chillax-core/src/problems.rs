//! Problems the project manager handles before the stakeholder hears of them (ADR 0032).
//! Pure and clock-free: the runner supplies the facts only it knows (`ProblemNote`), this
//! module merges them with what the state shows and writes the briefing. The dispatcher
//! decides when to send it (`dispatch::problem_actions`).

use std::collections::BTreeMap;

use crate::dispatch::held;
use crate::engine::Engine;
use crate::model::*;

/// The briefing never grows past this many characters.
const MAX_CHARS: usize = 1500;

/// One problem's detail is cut to this many characters.
const MAX_DETAIL: usize = 200;

/// What went wrong, in the order a briefing lists it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ProblemKind {
    /// A dispatcher action that is not about one ticket kept failing.
    ActionFailed,
    /// An agent session crashed.
    AgentCrashed,
    /// An agent ended two turns in a row without moving its ticket.
    AgentStuck,
    /// An app stopped restarting after crashing too often.
    AppCrashing,
    /// A checkout is dirty or behind.
    CheckoutDirty,
}

impl ProblemKind {
    /// A stable lowercase word for briefings and logs.
    pub fn label(self) -> &'static str {
        match self {
            Self::ActionFailed => "action-failed",
            Self::AgentCrashed => "agent-crashed",
            Self::AgentStuck => "agent-stuck",
            Self::AppCrashing => "app-crashing",
            Self::CheckoutDirty => "checkout-dirty",
        }
    }
}

/// A fact only the runner knows: something that went wrong and how far handling got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProblemNote {
    pub key: String,
    pub kind: ProblemKind,
    /// Who or what it is about (an employee name, an app, an action).
    pub subject: String,
    pub detail: String,
    /// The employee it concerns, so a held one is never briefed about.
    pub employee: Option<EmployeeId>,
    pub first_seen_secs: u64,
    /// Briefings sent so far.
    pub attempts: u32,
}

/// One entry of a briefing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub key: String,
    pub kind: ProblemKind,
    pub subject: String,
    pub detail: String,
    pub attempts: u32,
}

/// The problems to brief now: the runner's notes plus employees whose session failed,
/// without anyone held, the planner (who would handle it) or a backend that is limited.
/// Ordered by kind, then key; at most `max`.
pub fn problems(engine: &Engine, notes: &[ProblemNote], max: usize) -> Vec<Problem> {
    let mut found: BTreeMap<(ProblemKind, String), Problem> = BTreeMap::new();
    for e in &engine.state.employees {
        let id = &e.employee.id;
        let planner = engine.roles.get(&e.employee.role).is_some_and(|r| {
            engine
                .workflow
                .role_owns(r, engine.workflow.initial_state())
        });
        if e.status != EmployeeStatus::Failed
            || e.stopped
            || planner
            || engine
                .state
                .backend_limits
                .contains_key(&e.employee.backend)
        {
            continue;
        }
        let key = format!("agent-{id}");
        found.insert(
            (ProblemKind::AgentCrashed, key.clone()),
            Problem {
                key,
                kind: ProblemKind::AgentCrashed,
                subject: e.employee.name.clone(),
                detail: "the agent session failed".into(),
                attempts: 0,
            },
        );
    }
    for n in notes {
        if n.employee.as_deref().is_some_and(|e| held(engine, e)) {
            continue;
        }
        found.insert(
            (n.kind, n.key.clone()),
            Problem {
                key: n.key.clone(),
                kind: n.kind,
                subject: n.subject.clone(),
                detail: n.detail.clone(),
                attempts: n.attempts,
            },
        );
    }
    found.into_values().take(max).collect()
}

fn one_line(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= MAX_DETAIL {
        flat
    } else {
        flat.chars().take(MAX_DETAIL - 3).collect::<String>() + "..."
    }
}

/// The briefing the project manager receives: one line per problem, ids and names only.
/// `None` when there is nothing to brief.
pub fn briefing(problems: &[Problem], max_attempts: u32) -> Option<String> {
    if problems.is_empty() {
        return None;
    }
    let intro = "[Chillax] Problem briefing. Handle each one with the cheapest fix first: \
                 retry_action, then restart_teammate, then assign to another developer, then \
                 file a bug with create_story. Finish every one with problem_resolved and a \
                 one-line summary. ask_stakeholder only for credentials, billing, approvals or \
                 policy. Never touch a stopped teammate.\n";
    let mut text = intro.to_string();
    for (i, p) in problems.iter().enumerate() {
        let line = format!(
            "P-{} | {} | {} | {} | tried {}/{}\n",
            p.key,
            p.kind.label(),
            p.subject,
            one_line(&p.detail),
            p.attempts,
            max_attempts
        );
        let more = format!("+{} more\n", problems.len() - i);
        if text.chars().count() + line.chars().count() + more.chars().count() > MAX_CHARS {
            text.push_str(&more);
            break;
        }
        text.push_str(&line);
    }
    Some(text.trim_end().to_string())
}
