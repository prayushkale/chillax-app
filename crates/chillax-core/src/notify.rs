//! What is worth interrupting the stakeholder for, and the summary a terminal host shows.
//!
//! Pure: [`notifications_for`] and [`agent_summary`] only read the projection. Adapters
//! implement [`Notifier`] and decide how to deliver (ADR 0018).

use async_trait::async_trait;

use crate::engine::State;
use crate::event::{Event, EventKind};
use crate::model::{EmployeeStatus, InboxItem, QaRoundStatus, QaVerdict, TicketId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    /// The stakeholder has to do something.
    Attention,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub title: String,
    pub body: String,
    pub level: Level,
    /// Stable per subject (`inbox:<id>`), so a notifier can suppress repeats.
    pub key: String,
}

/// Delivers notifications somewhere the stakeholder will see them. Best effort: an
/// implementation logs and swallows its own failures.
#[async_trait]
pub trait Notifier: Send + Sync {
    async fn notify(&self, n: Notification);
}

/// The notifications a freshly committed `event` calls for. `state` is the projection at
/// or after the event, used for names and QA verdicts.
pub fn notifications_for(event: &Event, state: &State) -> Vec<Notification> {
    let name = |id: &str| {
        state
            .employee(id)
            .map_or_else(|| id.to_string(), |e| e.employee.name.clone())
    };
    let one = |title: String, body: String, level, key: String| {
        vec![Notification {
            title,
            body,
            level,
            key,
        }]
    };
    match &event.kind {
        EventKind::PermissionRequested {
            id,
            employee,
            title,
            ..
        } => one(
            format!("Chillax needs you: {}", name(employee)),
            title.clone(),
            Level::Attention,
            format!("inbox:{id}"),
        ),
        EventKind::QuestionAsked {
            id,
            employee,
            question,
        } => one(
            format!("Chillax needs you: {}", name(employee)),
            question.clone(),
            Level::Attention,
            format!("inbox:{id}"),
        ),
        EventKind::AlertRaised { id, title, detail } => one(
            format!("Chillax alert: {title}"),
            detail.clone(),
            Level::Attention,
            format!("alert:{id}"),
        ),
        EventKind::AgentFailed {
            employee, error, ..
        } => one(
            format!("{} failed", name(employee)),
            error.clone(),
            Level::Error,
            format!("fail:{employee}"),
        ),
        EventKind::QaRoundClosed { number } => {
            let verdicts = state
                .qa
                .as_ref()
                .filter(|r| r.number == *number)
                .map(|r| r.verdicts.values().copied().collect::<Vec<_>>())
                .unwrap_or_default();
            let count = |v| verdicts.iter().filter(|x| **x == v).count();
            let (pass, fail) = (count(QaVerdict::Pass), count(QaVerdict::Fail));
            one(
                format!("QA round {number} closed"),
                format!("{pass} passed, {fail} failed"),
                if fail > 0 {
                    Level::Attention
                } else {
                    Level::Info
                },
                format!("qa:{number}"),
            )
        }
        EventKind::ReleaseTagged { tag, .. } => one(
            format!("Released {tag}"),
            String::new(),
            Level::Info,
            format!("release:{tag}"),
        ),
        EventKind::MergeFailed { id, reason } => one(
            format!("Merge of {id} failed"),
            reason.clone(),
            Level::Attention,
            format!("merge:{id}"),
        ),
        _ => Vec::new(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Idle,
    Working,
    /// The stakeholder has to decide something.
    Blocked,
}

/// Chillax as one aggregate agent: what a host with a status board shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSummary {
    pub state: AgentState,
    pub open_inbox: usize,
    /// Tickets under test in the open QA round.
    pub in_qa: usize,
    /// Employees whose agent failed. Shown beside the state, never as it.
    pub failed: usize,
    pub project: String,
}

/// What one hire is doing, for a status strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HireState {
    Working,
    Idle,
    /// Waiting for the stakeholder.
    Blocked,
    /// The stakeholder stopped the turn and has not messaged them since.
    Stopped,
    /// No agent session yet.
    Offline,
    Failed,
}

impl HireState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Idle => "idle",
            Self::Blocked => "blocked",
            Self::Stopped => "stopped",
            Self::Offline => "offline",
            Self::Failed => "failed",
        }
    }

    /// Display order: who needs you first, then who is busy.
    fn rank(self) -> u8 {
        match self {
            Self::Blocked => 0,
            Self::Working => 1,
            Self::Failed => 2,
            _ => 3,
        }
    }
}

/// One hire, by id and name only: never ticket titles, which may be private.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HireSummary {
    pub id: String,
    pub name: String,
    pub state: HireState,
    /// The first assigned ticket that is neither finished nor dropped.
    pub ticket: Option<TicketId>,
}

fn waits_on_stakeholder(inbox: &[InboxItem], id: &str) -> bool {
    inbox.iter().any(|i| {
        matches!(i, InboxItem::Permission { employee, .. } | InboxItem::Question { employee, .. }
            if employee == id)
    })
}

/// Every hire's state, in display order (blocked, working, failed, the rest; hiring order
/// within each). Precedence: stopped, failed, waiting on the stakeholder, working, offline,
/// idle.
pub fn hire_summaries(state: &State) -> Vec<HireSummary> {
    let mut all: Vec<HireSummary> = state
        .employees
        .iter()
        .map(|e| {
            let id = &e.employee.id;
            let hire_state = if e.stopped {
                HireState::Stopped
            } else if e.status == EmployeeStatus::Failed {
                HireState::Failed
            } else if e.status == EmployeeStatus::WaitingOnStakeholder
                || waits_on_stakeholder(&state.inbox, id)
            {
                HireState::Blocked
            } else if e.turn_open
                || e.reply_pending
                || matches!(e.status, EmployeeStatus::Working | EmployeeStatus::Starting)
            {
                HireState::Working
            } else if e.status == EmployeeStatus::Offline {
                HireState::Offline
            } else {
                HireState::Idle
            };
            HireSummary {
                id: id.clone(),
                name: e.employee.name.clone(),
                state: hire_state,
                ticket: state
                    .assignments(id)
                    .into_iter()
                    .find(|t| t.dropped.is_none() && !state.is_finished(t))
                    .map(|t| t.id.clone()),
            }
        })
        .collect();
    all.sort_by_key(|h| h.state.rank());
    all
}

/// `Dana working T-4 · Sam idle`, cut to `max_chars` with a trailing `+N` for the hires
/// left out.
pub fn compact(hires: &[HireSummary], max_chars: usize) -> String {
    let part = |h: &HireSummary| match &h.ticket {
        Some(t) => format!("{} {} {t}", h.name, h.state.label()),
        None => format!("{} {}", h.name, h.state.label()),
    };
    let parts: Vec<String> = hires.iter().map(part).collect();
    for keep in (0..=parts.len()).rev() {
        let mut text = parts[..keep].join(" · ");
        if keep < parts.len() {
            if !text.is_empty() {
                text.push_str(" · ");
            }
            text.push_str(&format!("+{}", parts.len() - keep));
        }
        if text.chars().count() <= max_chars {
            return text;
        }
    }
    format!("+{}", parts.len())
        .chars()
        .take(max_chars)
        .collect()
}

/// Blocked beats working beats idle.
pub fn agent_summary(state: &State) -> AgentSummary {
    let round = state.open_round();
    let working = hire_summaries(state)
        .iter()
        .any(|h| h.state == HireState::Working)
        || round
            .is_some_and(|r| matches!(r.status, QaRoundStatus::Starting | QaRoundStatus::Testing));
    let open_inbox = state.inbox.len();
    AgentSummary {
        state: if open_inbox > 0 {
            AgentState::Blocked
        } else if working {
            AgentState::Working
        } else {
            AgentState::Idle
        },
        open_inbox,
        in_qa: round.map_or(0, |r| r.tickets.len()),
        failed: state
            .employees
            .iter()
            .filter(|e| e.status == EmployeeStatus::Failed)
            .count(),
        project: state.company.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::EmployeeView;
    use crate::model::{Employee, InboxItem, QaRound};
    use chrono::DateTime;

    fn ev(kind: EventKind) -> Event {
        Event {
            seq: 1,
            ts: DateTime::from_timestamp(0, 0).expect("epoch"),
            actor: crate::event::Actor::System,
            kind,
        }
    }

    fn state() -> State {
        State {
            company: "Acme".into(),
            employees: vec![EmployeeView {
                employee: Employee {
                    id: "dana".into(),
                    name: "Dana".into(),
                    role: "dev".into(),
                    role_title: "Developer".into(),
                    backend: "claude".into(),
                    model: None,
                    effort: None,
                },
                status: EmployeeStatus::Idle,
                last_progress: None,
                turn_open: false,
                stopped: false,
                reply_pending: false,
                pending_agent: None,
            }],
            ..State::default()
        }
    }

    fn round(status: QaRoundStatus) -> QaRound {
        QaRound {
            number: 2,
            sprint: None,
            commit: "abc".into(),
            status,
            tickets: vec!["T-1".into(), "T-2".into()],
            verdicts: [
                ("T-1".to_string(), QaVerdict::Pass),
                ("T-2".to_string(), QaVerdict::Fail),
            ]
            .into(),
            env_retries: Default::default(),
            up_done: false,
        }
    }

    fn one(kind: EventKind, state: &State) -> Notification {
        let mut all = notifications_for(&ev(kind), state);
        assert_eq!(all.len(), 1);
        all.remove(0)
    }

    #[test]
    fn people_needed_events_notify_with_the_employee_name_and_a_stable_key() {
        let s = state();
        let n = one(
            EventKind::PermissionRequested {
                id: "i1".into(),
                employee: "dana".into(),
                title: "Run rm".into(),
                choices: vec![],
            },
            &s,
        );
        assert_eq!(n.title, "Chillax needs you: Dana");
        assert_eq!(n.body, "Run rm");
        assert_eq!(n.level, Level::Attention);
        assert_eq!(n.key, "inbox:i1");
        let n = one(
            EventKind::QuestionAsked {
                id: "i2".into(),
                employee: "dana".into(),
                question: "Which db?".into(),
            },
            &s,
        );
        assert_eq!((n.body.as_str(), n.key.as_str()), ("Which db?", "inbox:i2"));
    }

    #[test]
    fn alerts_failures_merges_and_releases_notify() {
        let s = state();
        let n = one(
            EventKind::AlertRaised {
                id: "i3".into(),
                title: "Diverged".into(),
                detail: "dev moved".into(),
            },
            &s,
        );
        assert_eq!((n.key.as_str(), n.body.as_str()), ("alert:i3", "dev moved"));
        let n = one(
            EventKind::AgentFailed {
                employee: "dana".into(),
                error: "boom".into(),
                ticket: None,
            },
            &s,
        );
        assert_eq!(n.level, Level::Error);
        assert_eq!(
            (n.title.as_str(), n.key.as_str()),
            ("Dana failed", "fail:dana")
        );
        let n = one(
            EventKind::MergeFailed {
                id: "T-1".into(),
                reason: "conflict".into(),
            },
            &s,
        );
        assert_eq!(n.key, "merge:T-1");
        let n = one(
            EventKind::ReleaseTagged {
                id: "r1".into(),
                tag: "v0.2.0".into(),
                commit: "abc".into(),
            },
            &s,
        );
        assert_eq!(n.title, "Released v0.2.0");
        assert_eq!(n.level, Level::Info);
    }

    #[test]
    fn a_closed_qa_round_reports_the_verdicts() {
        let mut s = state();
        s.qa = Some(round(QaRoundStatus::Closed));
        let n = one(EventKind::QaRoundClosed { number: 2 }, &s);
        assert_eq!(n.title, "QA round 2 closed");
        assert_eq!(n.body, "1 passed, 1 failed");
        assert_eq!(n.level, Level::Attention);
    }

    #[test]
    fn unrelated_events_do_not_notify() {
        let s = state();
        assert!(notifications_for(&ev(EventKind::DispatchPaused), &s).is_empty());
        assert!(
            notifications_for(
                &ev(EventKind::AgentStopped {
                    employee: "dana".into()
                }),
                &s
            )
            .is_empty()
        );
    }

    #[test]
    fn blocked_beats_working_beats_idle() {
        let mut s = state();
        assert_eq!(agent_summary(&s).state, AgentState::Idle);
        s.employees[0].turn_open = true;
        assert_eq!(agent_summary(&s).state, AgentState::Working);
        s.inbox.push(InboxItem::Alert {
            id: "i1".into(),
            title: "t".into(),
            detail: String::new(),
        });
        let summary = agent_summary(&s);
        assert_eq!(summary.state, AgentState::Blocked);
        assert_eq!((summary.open_inbox, summary.project.as_str()), (1, "Acme"));
    }

    #[test]
    fn a_running_qa_round_is_work_and_a_failed_employee_is_not_a_state() {
        let mut s = state();
        s.employees[0].status = EmployeeStatus::Failed;
        let summary = agent_summary(&s);
        assert_eq!((summary.state, summary.failed), (AgentState::Idle, 1));
        s.qa = Some(round(QaRoundStatus::Testing));
        let summary = agent_summary(&s);
        assert_eq!((summary.state, summary.in_qa), (AgentState::Working, 2));
        s.qa = Some(round(QaRoundStatus::Closed));
        assert_eq!(agent_summary(&s).state, AgentState::Idle);
    }

    fn hire(id: &str, name: &str, status: EmployeeStatus) -> EmployeeView {
        let mut v = state().employees.remove(0);
        v.employee.id = id.into();
        v.employee.name = name.into();
        v.status = status;
        v
    }

    fn ticket(id: &str, assignee: &str, state: &str) -> crate::model::Ticket {
        serde_json::from_value(serde_json::json!({
            "id": id, "kind": "task", "title": "secret title", "state": state,
            "assignee": assignee,
        }))
        .expect("ticket")
    }

    fn only(s: &State) -> HireSummary {
        hire_summaries(s).remove(0)
    }

    #[test]
    fn hire_state_precedence() {
        let mut s = state();
        assert_eq!(only(&s).state, HireState::Idle);
        s.employees[0].status = EmployeeStatus::Offline;
        assert_eq!(only(&s).state, HireState::Offline);
        s.employees[0].reply_pending = true;
        assert_eq!(only(&s).state, HireState::Working);
        s.employees[0].reply_pending = false;
        s.employees[0].turn_open = true;
        assert_eq!(only(&s).state, HireState::Working);
        s.inbox.push(InboxItem::Question {
            id: "i1".into(),
            employee: "dana".into(),
            question: "q".into(),
        });
        assert_eq!(only(&s).state, HireState::Blocked);
        s.employees[0].status = EmployeeStatus::Failed;
        assert_eq!(only(&s).state, HireState::Failed);
        s.employees[0].stopped = true;
        assert_eq!(only(&s).state, HireState::Stopped);
    }

    #[test]
    fn waiting_status_blocks_without_an_inbox_item_and_other_inbox_items_do_not() {
        let mut s = state();
        s.employees[0].status = EmployeeStatus::WaitingOnStakeholder;
        assert_eq!(only(&s).state, HireState::Blocked);
        s.employees[0].status = EmployeeStatus::Idle;
        s.inbox.push(InboxItem::Alert {
            id: "i1".into(),
            title: "t".into(),
            detail: String::new(),
        });
        assert_eq!(only(&s).state, HireState::Idle);
    }

    #[test]
    fn a_hire_shows_the_first_unfinished_ticket() {
        let mut s = state();
        s.workflow_states = vec!["backlog".into(), "in_progress".into(), "done".into()];
        let mut dropped = ticket("T-2", "dana", "backlog");
        dropped.dropped = Some("no".into());
        s.tickets = vec![
            ticket("T-1", "dana", "done"),
            dropped,
            ticket("T-3", "dana", "in_progress"),
            ticket("T-4", "dana", "backlog"),
        ];
        assert_eq!(only(&s).ticket.as_deref(), Some("T-3"));
        s.tickets.truncate(1);
        assert_eq!(only(&s).ticket, None);
    }

    #[test]
    fn a_stopped_hire_is_not_working_in_the_aggregate() {
        let mut s = state();
        s.employees[0].turn_open = true;
        s.employees[0].stopped = true;
        assert_eq!(agent_summary(&s).state, AgentState::Idle);
        s.employees[0].stopped = false;
        s.employees[0].turn_open = false;
        s.employees[0].reply_pending = true;
        assert_eq!(agent_summary(&s).state, AgentState::Working);
    }

    #[test]
    fn hires_are_ordered_blocked_then_working_then_the_rest() {
        let mut s = state();
        s.employees = vec![
            hire("a", "Ann", EmployeeStatus::Idle),
            hire("b", "Bob", EmployeeStatus::Working),
            hire("c", "Cy", EmployeeStatus::WaitingOnStakeholder),
        ];
        let names: Vec<_> = hire_summaries(&s).into_iter().map(|h| h.name).collect();
        assert_eq!(names, ["Cy", "Bob", "Ann"]);
    }

    #[test]
    fn compact_lists_names_states_and_tickets_never_titles() {
        let mut s = state();
        s.workflow_states = vec!["backlog".into(), "done".into()];
        s.employees = vec![
            hire("a", "Dana", EmployeeStatus::Working),
            hire("b", "Sam", EmployeeStatus::Idle),
        ];
        s.tickets = vec![ticket("T-4", "a", "backlog")];
        let text = compact(&hire_summaries(&s), 80);
        assert_eq!(text, "Dana working T-4 · Sam idle");
        assert!(!text.contains("secret"));
    }

    #[test]
    fn compact_truncates_with_a_count_of_the_rest() {
        let mut s = state();
        s.employees = vec![
            hire("a", "Dana", EmployeeStatus::Working),
            hire("b", "Sam", EmployeeStatus::Idle),
            hire("c", "Lee", EmployeeStatus::Idle),
        ];
        let hires = hire_summaries(&s);
        assert_eq!(compact(&hires, 30), "Dana working · Sam idle · +1");
        assert_eq!(compact(&hires, 20), "Dana working · +2");
        assert_eq!(compact(&hires, 3), "+3");
        assert_eq!(compact(&[], 10), "");
    }
}
