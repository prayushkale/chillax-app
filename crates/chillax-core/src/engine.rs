//! The engine: `decide` turns a command into events (pure, no IO); `apply` folds an
//! event into the [`State`] projection. Replaying every event through `apply` rebuilds
//! the state after a restart.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::apps::{AppStatus, AppWant};
use crate::command::Command;
use crate::config::TeamConfig;
use crate::error::CommandError;
use crate::event::{Actor, Event, EventKind};
use crate::model::*;
use crate::release;
use crate::role::{Role, RoleCatalog};
use crate::triage::{self, FailureClass, TriageStatus};
use crate::workflow::{IN_PROGRESS, IN_QA, IN_REVIEW, Transition, Workflow};

/// An employee plus what their agent session is doing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmployeeView {
    #[serde(flatten)]
    pub employee: Employee,
    pub status: EmployeeStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_progress: Option<String>,
    /// True between the first chunk of a turn and `AgentTurnEnded`.
    #[serde(default)]
    pub turn_open: bool,
    /// The stakeholder stopped this employee's turn and has not messaged them since: the
    /// dispatcher gives them no new work meanwhile.
    #[serde(default)]
    pub stopped: bool,
    /// A stakeholder message is out and the reply has not started or ended yet.
    /// `turn_open` only becomes true on the first chunk, so a stop needs this too.
    #[serde(default)]
    pub reply_pending: bool,
    /// An agent change the stakeholder queued while this employee was replying; applied
    /// when the turn ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_agent: Option<PendingAgent>,
}

/// A queued agent change: the same three ids `EmployeeAgentChanged` carries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingAgent {
    pub backend: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

/// Read model of the whole company, served at `/api/v1/state`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub company: String,
    pub last_seq: u64,
    pub employees: Vec<EmployeeView>,
    pub chats: BTreeMap<EmployeeId, Vec<ChatLine>>,
    pub inbox: Vec<InboxItem>,
    pub epics: Vec<Epic>,
    pub tickets: Vec<Ticket>,
    pub sprints: Vec<Sprint>,
    /// Workflow state keys in board order.
    pub workflow_states: Vec<String>,
    /// The workflow's transitions, so frontends can offer only legal moves.
    #[serde(default)]
    pub workflow_transitions: Vec<Transition>,
    #[serde(default)]
    pub inbox_seq: u64,
    /// Every hire ever made, by role id (including employees since let go). Ranks the
    /// hiring catalog.
    #[serde(default)]
    pub hires_by_role: BTreeMap<String, u32>,
    /// The agent and model new hires get unless the stakeholder picks others.
    #[serde(default)]
    pub hiring: HiringDefaults,
    /// The dispatcher starts no new work and runs no ticket sessions while paused.
    #[serde(default)]
    pub dispatch_paused: bool,
    /// Backends whose usage limit is hit: the dispatcher starts no work on them until a
    /// probe succeeds.
    #[serde(default)]
    pub backend_limits: BTreeMap<String, BackendLimit>,
    /// What each employee's agent is doing right now, for the Agent live view.
    #[serde(default)]
    pub live: BTreeMap<EmployeeId, LiveView>,
    /// The open QA round, or the last one once it closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qa: Option<QaRound>,
    /// Someone asked for `dev` to be promoted to `qa` and no round has started yet.
    #[serde(default)]
    pub qa_requested: bool,
    /// Releases proposed so far, oldest first.
    #[serde(default)]
    pub releases: Vec<Release>,
    /// A retry the stakeholder asked for and the dispatcher has not run yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_retry: Option<EnvRetry>,
    /// Seq of the latest `ActivityCleared`; activity views show only later events.
    #[serde(default)]
    pub activity_cleared_seq: u64,
    /// Failures that may become bugs, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<crate::triage::Failure>,
    /// Failed test runs in a row, per ticket. The second one is triaged.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub test_streaks: BTreeMap<TicketId, u32>,
    /// The stakeholder asked for the next sprint to be planned and none has been proposed
    /// yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planning: Option<PlanningRequest>,
    /// What the stakeholder asked for in their own words, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requests: Vec<Request>,
    /// The dev, QA and prod apps Chillax runs, in that order (ADR 0015).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub apps: Vec<crate::apps::RunningApp>,
    /// Apps employees say they started, per (employee, label). Kept apart from `apps`
    /// because the dispatcher reads those.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reported_apps: Vec<crate::apps::ReportedApp>,
    /// Tokens and cost recorded so far (ADR 0023).
    #[serde(default)]
    pub usage: crate::usage::UsageStats,
    /// The live agent limit and the hardware maximum; `None` until the first start
    /// records them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_limit: Option<AgentLimit>,
    /// The company chat feed, oldest first, at most `COMPANY_CHAT_KEEP` entries.
    /// Clearing a transcript does not touch it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub company_chat: Vec<crate::model::CompanyChatEntry>,
}

/// Company chat entries kept; the oldest are dropped past this.
pub const COMPANY_CHAT_KEEP: usize = 500;

/// How many conflicts with `dev` a ticket's assignee gets a resolve turn for before the
/// conflict is triaged like any failed merge.
pub const MAX_CONFLICT_ATTEMPTS: u32 = 2;

/// Employees named in `text` with `@id` or `@FirstName` (case-insensitive, on a word
/// boundary), without duplicates, in order of appearance. `@all` names nobody.
pub fn resolve_mentions(state: &State, text: &str) -> Vec<EmployeeId> {
    let mut found: Vec<EmployeeId> = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find('@') {
        let before_ok = rest[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
        rest = &rest[at + 1..];
        if !before_ok {
            continue;
        }
        let end = rest
            .find(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_'))
            .unwrap_or(rest.len());
        let word = rest[..end].trim_end_matches(['-', '_']);
        if word.is_empty() {
            continue;
        }
        let hit = state
            .employees
            .iter()
            .find(|e| e.employee.id.eq_ignore_ascii_case(word))
            .or_else(|| {
                state.employees.iter().find(|e| {
                    e.employee
                        .name
                        .split_whitespace()
                        .next()
                        .is_some_and(|first| first.eq_ignore_ascii_case(word))
                })
            });
        if let Some(e) = hit
            && !found.contains(&e.employee.id)
        {
            found.push(e.employee.id.clone());
        }
    }
    found
}

impl State {
    /// Employees whose agent is working right now: what the agent limit is compared with.
    pub fn agents_running(&self) -> usize {
        self.employees
            .iter()
            .filter(|e| e.status == EmployeeStatus::Working)
            .count()
    }

    /// Employees the projection shows with a live agent (idle, working, waiting, failed
    /// or mid-turn). Right after a start no agent process exists, so these are leftovers
    /// of a run that did not record `AgentStopped`.
    pub fn stale_agents(&self) -> Vec<String> {
        self.employees
            .iter()
            .filter(|e| e.status != EmployeeStatus::Offline || e.turn_open)
            .map(|e| e.employee.id.clone())
            .collect()
    }

    /// Supervised and reported apps for the dashboards, grouped by label.
    pub fn running_apps(&self) -> Vec<crate::apps::RunningAppView> {
        crate::apps::merge_running(&self.apps, &self.reported_apps)
    }

    pub fn app(&self, env: &str) -> Option<&crate::apps::RunningApp> {
        self.apps.iter().find(|a| a.env == env)
    }

    fn app_mut(&mut self, env: &str) -> Option<&mut crate::apps::RunningApp> {
        self.apps.iter_mut().find(|a| a.env == env)
    }

    pub fn request(&self, id: &str) -> Option<&Request> {
        self.requests.iter().find(|r| r.id == id)
    }

    pub fn employee(&self, id: &str) -> Option<&EmployeeView> {
        self.employees.iter().find(|e| e.employee.id == id)
    }

    fn employee_mut(&mut self, id: &str) -> Option<&mut EmployeeView> {
        self.employees.iter_mut().find(|e| e.employee.id == id)
    }

    /// States the stakeholder may move a ticket in `from` to by hand: what the workflow
    /// allows, minus the moves Chillax makes itself.
    /// Moves the `sprint_start` gate would reject are left out, via the same check
    /// `decide` uses.
    pub fn stakeholder_moves(&self, ticket: &Ticket) -> Vec<&str> {
        self.workflow_transitions
            .iter()
            .filter(|t| t.from == ticket.state && !t.chillax_owned)
            .filter(|t| self.sprint_gate_allows(ticket, &t.to))
            .map(|t| t.to.as_str())
            .collect()
    }

    /// The `sprint_start` gate: work leaves the initial state for the planned state only
    /// once the sprint containing it is approved. Shared by `decide` and the UIs so the
    /// moves offered and the moves accepted cannot drift apart.
    pub fn sprint_gate_allows(&self, ticket: &Ticket, to: &str) -> bool {
        let initial = self.workflow_states.first().map(String::as_str);
        let planned = initial.and_then(|i| {
            self.workflow_transitions
                .iter()
                .find(|t| t.from == i)
                .map(|t| t.to.as_str())
        });
        if Some(ticket.state.as_str()) != initial || Some(to) != planned {
            return true;
        }
        ticket
            .sprint
            .as_deref()
            .and_then(|sp| self.sprint(sp))
            .is_some_and(|sp| sp.status == SprintStatus::Active)
    }

    pub fn ticket(&self, id: &str) -> Option<&Ticket> {
        self.tickets.iter().find(|t| t.id == id)
    }

    fn ticket_mut(&mut self, id: &str) -> Option<&mut Ticket> {
        self.tickets.iter_mut().find(|t| t.id == id)
    }

    pub fn sprint(&self, id: &str) -> Option<&Sprint> {
        self.sprints.iter().find(|s| s.id == id)
    }

    pub fn inbox_item(&self, id: &str) -> Option<&InboxItem> {
        self.inbox.iter().find(|i| i.id() == id)
    }

    /// The id the next inbox item will get. Runtime code that records inbox events
    /// allocates through this while holding the hub lock.
    pub fn next_inbox_id(&self) -> InboxId {
        format!("IN-{}", self.inbox_seq + 1)
    }

    /// Display name for an actor (employee name, "Stakeholder", "Chillax").
    pub fn actor_name(&self, actor: &Actor) -> String {
        match actor {
            Actor::Stakeholder => "Stakeholder".into(),
            Actor::System => "Chillax".into(),
            Actor::Employee(id) => self
                .employee(id)
                .map(|e| e.employee.name.clone())
                .unwrap_or_else(|| id.clone()),
        }
    }

    /// Tickets assigned to an employee, oldest first.
    pub fn assignments(&self, employee: &str) -> Vec<&Ticket> {
        self.tickets
            .iter()
            .filter(|t| t.assignee.as_deref() == Some(employee))
            .collect()
    }

    /// True when every blocker of `ticket` is merged into dev, done or dropped.
    pub fn blockers_clear(&self, ticket: &Ticket) -> bool {
        ticket.blocked_by.iter().all(|b| self.blocker_closed(b))
    }

    /// Whether the blocker `id` no longer holds anything up: merged, done, dropped or gone.
    pub fn blocker_closed(&self, id: &str) -> bool {
        self.ticket(id).is_none_or(|b| {
            b.dropped.is_some()
                || b.state == self.terminal_state()
                || b.checks.contains("merged_to_dev")
        })
    }

    /// The sprint that is waiting at the gate or running, if any. Planning the next
    /// sprint needs this to be `None`.
    pub fn sprint_in_flight(&self) -> Option<&Sprint> {
        self.sprints
            .iter()
            .find(|s| matches!(s.status, SprintStatus::Proposed | SprintStatus::Active))
    }

    /// Sprints that are over and worth looking at: released or closed, oldest first.
    /// Rejected ones never ran, so they are left out.
    pub fn past_sprints(&self) -> Vec<&Sprint> {
        self.sprints
            .iter()
            .filter(|s| matches!(s.status, SprintStatus::Released | SprintStatus::Closed))
            .collect()
    }

    /// The tickets the Board shows for a view, in ticket order, without dropped ones.
    pub fn board_tickets(&self, view: &SprintView) -> Vec<&Ticket> {
        let in_flight = self.sprint_in_flight().map(|s| s.id.as_str());
        self.tickets
            .iter()
            .filter(|t| t.dropped.is_none())
            .filter(|t| match view {
                SprintView::Active => in_flight.is_some() && t.sprint.as_deref() == in_flight,
                SprintView::Sprint(id) => t.sprint.as_deref() == Some(id.as_str()),
                SprintView::Backlog => t.sprint.is_none(),
            })
            .collect()
    }

    /// The view after `view`: Active, then past sprints newest to oldest, then Backlog,
    /// then back to Active. A sprint that is gone counts as Active.
    pub fn next_sprint_view(&self, view: &SprintView) -> SprintView {
        let cycle = self.sprint_view_cycle();
        let at = cycle.iter().position(|v| v == view).unwrap_or(0);
        cycle[(at + 1) % cycle.len()].clone()
    }

    /// The view before `view`, the reverse of [`State::next_sprint_view`].
    pub fn prev_sprint_view(&self, view: &SprintView) -> SprintView {
        let cycle = self.sprint_view_cycle();
        let at = cycle.iter().position(|v| v == view).unwrap_or(0);
        cycle[(at + cycle.len() - 1) % cycle.len()].clone()
    }

    /// Whether `view` can still be shown (its sprint exists).
    pub fn sprint_view_exists(&self, view: &SprintView) -> bool {
        self.sprint_view_cycle().contains(view)
    }

    fn sprint_view_cycle(&self) -> Vec<SprintView> {
        let mut cycle = vec![SprintView::Active];
        cycle.extend(
            self.past_sprints()
                .iter()
                .rev()
                .map(|s| SprintView::Sprint(s.id.clone())),
        );
        cycle.push(SprintView::Backlog);
        cycle
    }

    /// The tickets of a sprint that still count: not dropped.
    pub fn sprint_tickets<'a>(&'a self, sprint: &'a Sprint) -> impl Iterator<Item = &'a Ticket> {
        sprint
            .tickets
            .iter()
            .filter_map(|id| self.ticket(id))
            .filter(|t| t.dropped.is_none())
    }

    /// Whether the stakeholder may still drop the ticket: not already dropped, not done,
    /// and its code not in dev yet. Shared by `decide` and the UIs.
    pub fn droppable(&self, ticket: &Ticket) -> bool {
        ticket.dropped.is_none()
            && !self.is_finished(ticket)
            && !ticket.checks.contains("merged_to_dev")
            && !ticket.checks.contains("released")
    }

    /// Done or dropped: nothing more to do for the ticket.
    pub fn is_settled(&self, ticket: &Ticket) -> bool {
        ticket.dropped.is_some() || self.is_finished(ticket)
    }

    /// The QA round that has not closed, if any.
    pub fn open_round(&self) -> Option<&QaRound> {
        self.qa
            .as_ref()
            .filter(|r| r.status != QaRoundStatus::Closed)
    }

    /// The release that is still moving through the gate, the merge and the tag.
    pub fn open_release(&self) -> Option<&Release> {
        self.releases.iter().find(|r| {
            matches!(
                r.status,
                ReleaseStatus::Gated | ReleaseStatus::Approved | ReleaseStatus::Tagged
            )
        })
    }

    pub fn release(&self, id: &str) -> Option<&Release> {
        self.releases.iter().find(|r| r.id == id)
    }

    /// The workflow's last state: where finished work ends up.
    pub fn terminal_state(&self) -> &str {
        self.workflow_states.last().map_or("done", String::as_str)
    }

    /// True when the ticket has reached the workflow's last state.
    pub fn is_finished(&self, ticket: &Ticket) -> bool {
        ticket.state == self.terminal_state()
    }

    /// Tickets merged into `dev` that QA has not signed off yet.
    pub fn waiting_for_qa(&self) -> Vec<&Ticket> {
        self.tickets
            .iter()
            .filter(|t| t.state == IN_QA && !t.checks.contains("qa_passed"))
            .collect()
    }

    /// Tickets a QA round can start over now: waiting ones, except a story that failed
    /// QA until the bugs filed against it have been merged (else the closed round would
    /// retest the story at once, before its fix).
    pub fn qa_ready(&self) -> Vec<&Ticket> {
        self.waiting_for_qa()
            .into_iter()
            .filter(|t| {
                !t.checks.contains("qa_failed")
                    || self
                        .tickets
                        .iter()
                        .filter(|b| b.found_in.as_deref() == Some(t.id.as_str()))
                        .all(|b| b.state == IN_QA || self.is_finished(b))
            })
            .collect()
    }

    fn live_tool(&mut self, employee: &str, tool_call_id: &str) -> Option<&mut LiveTool> {
        self.live
            .get_mut(employee)?
            .tools
            .iter_mut()
            .rev()
            .find(|t| t.tool_call_id == tool_call_id)
    }

    fn chat(&mut self, employee: &str) -> &mut Vec<ChatLine> {
        self.chats.entry(employee.to_string()).or_default()
    }

    fn push_company_chat(&mut self, entry: CompanyChatEntry) {
        self.company_chat.push(entry);
        if self.company_chat.len() > COMPANY_CHAT_KEEP {
            let over = self.company_chat.len() - COMPANY_CHAT_KEEP;
            self.company_chat.drain(..over);
        }
    }

    fn push_line(&mut self, employee: &str, speaker: Speaker, text: impl Into<String>) {
        self.chat(employee).push(ChatLine {
            speaker,
            text: text.into(),
            tool_call_id: None,
        });
    }

    fn bump_inbox(&mut self, id: &str) {
        if let Some(n) = id.strip_prefix("IN-").and_then(|n| n.parse().ok()) {
            self.inbox_seq = self.inbox_seq.max(n);
        }
    }
}

/// Engine = projection + the rules needed to validate commands.
#[derive(Debug, Clone)]
pub struct Engine {
    pub state: State,
    pub roles: RoleCatalog,
    pub workflow: Workflow,
    /// Who may hire teammates and how big the team may grow.
    pub team: TeamConfig,
    /// What this machine can run, for validating the agent limit. Not state: the value
    /// that counts is the one carried by the event.
    pub hardware_max: Option<usize>,
}

impl Engine {
    /// Tell the engine what this machine can run, so the agent limit is validated.
    pub fn with_hardware_max(mut self, hardware_max: usize) -> Self {
        self.hardware_max = Some(hardware_max);
        self
    }

    /// Use the project's `[team]` policy.
    pub fn with_team(mut self, team: TeamConfig) -> Self {
        self.team = team;
        self
    }

    /// See [`State::stale_agents`].
    pub fn stale_agents(&self) -> Vec<String> {
        self.state.stale_agents()
    }

    pub fn new(company: impl Into<String>, roles: RoleCatalog, workflow: Workflow) -> Self {
        let state = State {
            company: company.into(),
            workflow_states: workflow.states.iter().map(|s| s.key.clone()).collect(),
            workflow_transitions: workflow.transitions.clone(),
            ..State::default()
        };
        Self {
            state,
            roles,
            workflow,
            team: TeamConfig::default(),
            hardware_max: None,
        }
    }

    /// Validate a command and return the events it produces. Pure: never mutates.
    pub fn decide(&self, actor: &Actor, cmd: Command) -> Result<Vec<EventKind>, CommandError> {
        let s = &self.state;
        if let Some(id) = live_ticket_of(&cmd)
            && s.ticket(id).is_some_and(|t| t.dropped.is_some())
        {
            return Err(CommandError::Invalid(format!("{id} was dropped")));
        }
        let events = match cmd {
            Command::HireEmployee {
                role,
                name,
                backend,
                model,
                effort,
                keep_defaults,
            } => {
                stakeholder_only(actor, "hire")?;
                let role = self
                    .roles
                    .get(&role)
                    .ok_or_else(|| CommandError::UnknownRole(role.clone()))?;
                let name = name
                    .and_then(|n| non_blank(&n))
                    .unwrap_or_else(|| crate::names::default_name(s, &role.title));
                let id = self.unique_employee_id(&name);
                let backend = backend.and_then(|b| non_blank(&b));
                let model = model.and_then(|m| non_blank(&m));
                let effort = effort.and_then(|e| non_blank(&e));
                let picked = backend.is_some() || model.is_some() || effort.is_some();
                let (backend, model, effort) = hire_choice(&s.hiring, role, backend, model, effort);
                let mut events = vec![EventKind::EmployeeHired {
                    employee: Employee {
                        id,
                        name,
                        role: role.id.clone(),
                        role_title: role.title.clone(),
                        backend: backend.clone(),
                        model: model.clone(),
                        effort: effort.clone(),
                    },
                }];
                // What the stakeholder picks while hiring becomes the default.
                let defaults = HiringDefaults {
                    backend: Some(backend),
                    model,
                    effort,
                };
                if picked && !keep_defaults && defaults != s.hiring {
                    events.push(EventKind::HiringDefaultsSet {
                        backend: defaults.backend,
                        model: defaults.model,
                        effort: defaults.effort,
                    });
                }
                events
            }
            Command::HireTeammate { role, name, reason } => {
                let hirer = match actor {
                    Actor::Employee(id) => self.require_employee(id)?,
                    _ => {
                        return Err(CommandError::Invalid(
                            "only an employee hires teammates this way; the stakeholder uses HireEmployee"
                                .into(),
                        ));
                    }
                };
                if !self.team.hiring_roles.contains(&hirer.employee.role) {
                    return Err(CommandError::Invalid("your role may not hire".into()));
                }
                let role = self
                    .roles
                    .get(&role)
                    .ok_or_else(|| CommandError::UnknownRole(role.clone()))?;
                if self.team.hiring_roles.contains(&role.id) {
                    return Err(CommandError::Invalid(format!(
                        "{} is a hiring role; ask the stakeholder to hire one",
                        role.id
                    )));
                }
                if s.employees.len() >= self.team.max_team_size as usize {
                    return Err(CommandError::Invalid(format!(
                        "the team is at its cap of {} people; ask the stakeholder",
                        self.team.max_team_size
                    )));
                }
                let reason = non_empty(reason, "reason")?;
                let name = name
                    .and_then(|n| non_blank(&n))
                    .unwrap_or_else(|| crate::names::default_name(s, &role.title));
                let id = self.unique_employee_id(&name);
                let (backend, model, effort) = hire_choice(&s.hiring, role, None, None, None);
                let title = format!("{} hired {} ({})", hirer.employee.name, name, role.title);
                vec![
                    EventKind::EmployeeHired {
                        employee: Employee {
                            id,
                            name,
                            role: role.id.clone(),
                            role_title: role.title.clone(),
                            backend,
                            model,
                            effort,
                        },
                    },
                    EventKind::AlertRaised {
                        id: s.next_inbox_id(),
                        title,
                        detail: reason,
                    },
                ]
            }
            Command::ReportApp {
                env,
                url,
                port,
                ticket,
                note,
                down,
            } => {
                // Chillax itself reports the apps it detects listening.
                let detected = matches!(actor, Actor::System);
                let me = if detected {
                    crate::apps::DETECTED_BY.to_string()
                } else {
                    let me = employee_only(actor, "report an app")?;
                    self.require_employee(&me)?;
                    me
                };
                let ticket = match ticket {
                    Some(id) => Some(self.require_ticket(&id)?),
                    None => {
                        let mut mine = s.tickets.iter().filter(|t| {
                            t.assignee.as_deref() == Some(me.as_str())
                                && t.state == crate::workflow::IN_PROGRESS
                        });
                        match (mine.next(), mine.next()) {
                            (Some(t), None) => Some(t),
                            _ => None,
                        }
                    }
                };
                let label = match env.as_deref() {
                    Some(raw) => crate::apps::normalize_label(raw),
                    None => Ok(ticket.map_or_else(|| "dev".to_string(), |t| t.id.clone())),
                }
                .map_err(CommandError::Invalid)?;
                let existing = s
                    .reported_apps
                    .iter()
                    .find(|a| a.employee == me && a.label == label);
                let note = match note {
                    Some(n) => crate::apps::normalize_note(&n).map_err(CommandError::Invalid)?,
                    None => None,
                };
                let (url, port) = if down {
                    match existing {
                        Some(a) => (a.url.clone(), a.port),
                        None => return Ok(vec![]),
                    }
                } else {
                    match (url, port) {
                        (Some(u), None) => crate::apps::normalize_reported_url(&u)
                            .map_err(CommandError::Invalid)?,
                        (None, Some(p)) if p >= 1 => (crate::apps::localhost_url(p), Some(p)),
                        (None, Some(_)) => {
                            return Err(CommandError::Invalid(
                                "port must be between 1 and 65535".into(),
                            ));
                        }
                        _ => {
                            return Err(CommandError::Invalid(
                                "give exactly one of url and port".into(),
                            ));
                        }
                    }
                };
                if !down
                    && !detected
                    && existing.is_none()
                    && s.reported_apps.iter().filter(|a| a.employee == me).count()
                        >= crate::apps::MAX_REPORTED_PER_EMPLOYEE
                {
                    return Err(CommandError::Invalid(format!(
                        "you already have {} apps reported; report one down first",
                        crate::apps::MAX_REPORTED_PER_EMPLOYEE
                    )));
                }
                vec![EventKind::AppReported {
                    env: label,
                    url,
                    port,
                    ticket: ticket.map(|t| t.id.clone()),
                    employee: me,
                    worktree: ticket.and_then(|t| t.worktree.clone()),
                    note,
                    down,
                }]
            }
            Command::SetEmployeeAgent {
                employee,
                when_idle,
                backend,
                model,
                effort,
            } => {
                stakeholder_only(actor, "change an agent")?;
                let view = self.require_employee(&employee)?;
                if view.turn_open && !when_idle {
                    return Err(CommandError::Invalid(format!(
                        "{} is replying; try again when they finish",
                        view.employee.name
                    )));
                }
                let (backend, model, effort) =
                    employee_agent_choice(&view.employee, backend, model, effort);
                let current = &view.employee;
                let same_as_current = backend == current.backend
                    && model == current.model
                    && effort == current.effort;
                if view.turn_open {
                    let same_as_queued = view.pending_agent.as_ref().is_some_and(|p| {
                        p.backend == backend && p.model == model && p.effort == effort
                    });
                    if same_as_queued || (same_as_current && view.pending_agent.is_none()) {
                        return Ok(vec![]);
                    }
                    // Asking for the current agent again withdraws the queued change.
                    return Ok(vec![EventKind::EmployeeAgentChangeQueued {
                        employee,
                        backend,
                        model,
                        effort,
                    }]);
                }
                if same_as_current {
                    return Ok(vec![]);
                }
                vec![EventKind::EmployeeAgentChanged {
                    employee,
                    backend,
                    model,
                    effort,
                }]
            }
            Command::ApplyQueuedAgentChange { employee } => {
                system_only(actor, "apply a queued agent change")?;
                let view = self.require_employee(&employee)?;
                match &view.pending_agent {
                    Some(p) if !view.turn_open => vec![EventKind::EmployeeAgentChanged {
                        employee,
                        backend: p.backend.clone(),
                        model: p.model.clone(),
                        effort: p.effort.clone(),
                    }],
                    _ => vec![],
                }
            }
            Command::SetHiringDefaults {
                backend,
                model,
                effort,
            } => {
                stakeholder_only(actor, "change hiring defaults")?;
                let backend = backend.and_then(|b| non_blank(&b));
                let model = model.and_then(|m| non_blank(&m));
                if model.is_some() && backend.is_none() {
                    return Err(CommandError::Invalid(
                        "a default model needs a default backend".into(),
                    ));
                }
                let effort = effort.and_then(|e| non_blank(&e));
                if effort.is_some() && backend.is_none() {
                    return Err(CommandError::Invalid(
                        "a default effort needs a default backend".into(),
                    ));
                }
                vec![EventKind::HiringDefaultsSet {
                    backend,
                    model,
                    effort,
                }]
            }
            Command::AdoptRole {
                markdown,
                revisions,
                overwrite,
            } => {
                stakeholder_only(actor, "adopt roles")?;
                let role = Role::parse(&markdown)
                    .map_err(|e| CommandError::Invalid(format!("role file: {e}")))?;
                if role.id.is_empty() || slug(&role.id) != role.id {
                    return Err(CommandError::Invalid(format!(
                        "role id `{}` must be lowercase letters, digits and dashes",
                        role.id
                    )));
                }
                non_empty(role.title.clone(), "title")?;
                non_empty(role.prompt.clone(), "prompt")?;
                if let Some(base) = &role.base_role
                    && self.roles.get(base).is_none()
                {
                    return Err(CommandError::UnknownRole(base.clone()));
                }
                match self.roles.get(&role.id) {
                    // Hand-written and built-in roles are never replaced by a persona.
                    Some(existing) if existing.source.is_none() => {
                        return Err(CommandError::Invalid(format!(
                            "role `{}` already exists and was not adopted from a catalog; \
                             choose another id",
                            role.id
                        )));
                    }
                    Some(_) if !overwrite => {
                        return Err(CommandError::Invalid(format!(
                            "role `{}` already exists; adopt with overwrite to replace it",
                            role.id
                        )));
                    }
                    _ => {}
                }
                vec![EventKind::RoleAdopted {
                    id: role.id,
                    title: role.title,
                    base_role: role.base_role,
                    source: role.source,
                    revisions,
                    markdown,
                }]
            }
            Command::LetGo { employee } => {
                stakeholder_only(actor, "let employees go")?;
                self.require_employee(&employee)?;
                let mut events = Vec::new();
                for item in &s.inbox {
                    if let InboxItem::Permission {
                        id, employee: e, ..
                    } = item
                        && *e == employee
                    {
                        events.push(EventKind::PermissionResolved {
                            id: id.clone(),
                            choice: None,
                            automatic: true,
                        });
                    }
                }
                for ticket in s.assignments(&employee) {
                    events.push(EventKind::TicketAssigned {
                        id: ticket.id.clone(),
                        assignee: None,
                    });
                }
                events.push(EventKind::EmployeeLetGo { employee });
                events
            }
            Command::SendChat { to, text } => {
                self.require_employee(&to)?;
                if actor.employee() == Some(to.as_str()) {
                    return Err(CommandError::Invalid("cannot message yourself".into()));
                }
                let text = non_empty(text, "message")?;
                vec![EventKind::ChatMessageSent { to, text }]
            }
            Command::PostCompanyChat { text, to } => {
                stakeholder_only(actor, "post in the company chat")?;
                let text = non_empty(text, "message")?;
                match to {
                    Some(to) => {
                        self.require_employee(&to)?;
                        vec![EventKind::ChatMessageSent { to, text }]
                    }
                    None => {
                        let mentions = resolve_mentions(s, &text);
                        vec![EventKind::CompanyChatPosted { text, mentions }]
                    }
                }
            }
            Command::ResolvePermission { id, choice } => {
                stakeholder_only(actor, "resolve permission requests")?;
                match s.inbox_item(&id) {
                    Some(InboxItem::Permission { choices, .. }) => {
                        if let Some(choice) = &choice
                            && !choices.iter().any(|c| &c.id == choice)
                        {
                            return Err(CommandError::Invalid(format!(
                                "`{choice}` is not one of the offered options"
                            )));
                        }
                    }
                    _ => return Err(CommandError::UnknownInboxItem(id)),
                }
                vec![EventKind::PermissionResolved {
                    id,
                    choice,
                    automatic: false,
                }]
            }
            Command::AskStakeholder { question } => {
                let employee = employee_only(actor, "ask the stakeholder")?;
                vec![EventKind::QuestionAsked {
                    id: s.next_inbox_id(),
                    employee,
                    question: non_empty(question, "question")?,
                }]
            }
            Command::AnswerQuestion { id, answer } => {
                stakeholder_only(actor, "answer questions")?;
                if !matches!(s.inbox_item(&id), Some(InboxItem::Question { .. })) {
                    return Err(CommandError::UnknownInboxItem(id));
                }
                vec![EventKind::QuestionAnswered {
                    id,
                    answer: non_empty(answer, "answer")?,
                }]
            }
            Command::ReportProgress { summary } => {
                let employee = employee_only(actor, "report progress")?;
                vec![EventKind::ProgressReported {
                    employee,
                    summary: non_empty(summary, "summary")?,
                }]
            }
            Command::ClearActivity => {
                stakeholder_only(actor, "clear activity")?;
                vec![EventKind::ActivityCleared]
            }
            Command::ClearChat { employee } => {
                stakeholder_only(actor, "clear a chat")?;
                let view = self.require_employee(&employee)?;
                if view.turn_open {
                    return Err(CommandError::Invalid(format!(
                        "{} is replying; try again when they finish",
                        view.employee.name
                    )));
                }
                vec![EventKind::ChatCleared { employee }]
            }
            Command::ClearEmployeeContext { employee, ticket } => {
                system_only(actor, "clear an employee's context")?;
                self.require_employee(&employee)?;
                vec![EventKind::EmployeeContextCleared { employee, ticket }]
            }
            Command::CancelTurn { employee } => {
                stakeholder_only(actor, "stop an agent")?;
                let view = self.require_employee(&employee)?;
                // A double press or a turn that just ended is harmless, not an error.
                if !(view.turn_open || view.status == EmployeeStatus::Working || view.reply_pending)
                {
                    return Ok(vec![]);
                }
                vec![EventKind::TurnCancelled { employee }]
            }
            Command::PrioritizeTicket { id, urgent } => {
                stakeholder_only(actor, "prioritise tickets")?;
                let t = self.require_ticket(&id)?;
                if s.is_settled(t) {
                    return Err(CommandError::Invalid(format!(
                        "{id} is already {}: nothing left to prioritise",
                        if t.dropped.is_some() {
                            "dropped"
                        } else {
                            "finished"
                        }
                    )));
                }
                if urgent == t.urgent {
                    return Ok(vec![]);
                }
                let mut events = vec![EventKind::TicketPrioritized {
                    id: id.clone(),
                    urgent,
                }];
                if urgent {
                    events.push(EventKind::AlertRaised {
                        id: s.next_inbox_id(),
                        title: format!("{id} is urgent: {}", t.title),
                        detail: format!(
                            "{id} will be taken up before other work. Next step: staff for it, or hire a developer if none fits. Sprint approval and blockers still apply."
                        ),
                    });
                }
                events
            }
            Command::SetMaxConcurrentAgents { n } => {
                stakeholder_only(actor, "change the agent limit")?;
                let current = s.agent_limit;
                let Some(hardware_max) = self.hardware_max.or(current.map(|a| a.hardware_max))
                else {
                    return Err(CommandError::Invalid(
                        "this machine's agent maximum is not known yet".into(),
                    ));
                };
                if n < 1 || n > hardware_max {
                    return Err(CommandError::Invalid(format!(
                        "limit must be 1..={hardware_max}"
                    )));
                }
                if current
                    == Some(AgentLimit {
                        limit: n,
                        hardware_max,
                    })
                {
                    return Ok(vec![]);
                }
                vec![EventKind::MaxConcurrentAgentsChanged { n, hardware_max }]
            }
            Command::CreateEpic { title, description } => vec![EventKind::EpicCreated {
                epic: Epic {
                    id: format!("EP-{}", s.epics.len() + 1),
                    title: non_empty(title, "title")?,
                    description,
                    external: None,
                },
            }],
            Command::CreateTicket {
                kind,
                title,
                description,
                acceptance_criteria,
                estimate,
                epic,
                external,
            } => {
                if let Some(epic) = &epic
                    && !s.epics.iter().any(|e| &e.id == epic)
                {
                    return Err(CommandError::UnknownEpic(epic.clone()));
                }
                vec![EventKind::TicketCreated {
                    ticket: Ticket {
                        id: format!("T-{}", s.tickets.len() + 1),
                        kind,
                        title: non_empty(title, "title")?,
                        description,
                        acceptance_criteria,
                        estimate,
                        epic,
                        state: self.workflow.initial_state().to_string(),
                        assignee: None,
                        sprint: None,
                        comments: Vec::new(),
                        external,
                        blocked_by: Vec::new(),
                        branch: None,
                        worktree: None,
                        reviewer: None,
                        checks: Default::default(),
                        needs_rebase: false,
                        review_requested: false,
                        found_in: None,
                        pr_number: None,
                        pr_url: None,
                        dropped: None,
                        urgent: false,
                        conflict: None,
                    },
                }]
            }
            Command::UpdateTicket {
                id,
                title,
                description,
                acceptance_criteria,
                estimate,
                epic,
            } => {
                self.require_ticket(&id)?;
                if let Some(epic) = &epic
                    && !s.epics.iter().any(|e| &e.id == epic)
                {
                    return Err(CommandError::UnknownEpic(epic.clone()));
                }
                vec![EventKind::TicketUpdated {
                    id,
                    title,
                    description,
                    acceptance_criteria,
                    estimate,
                    epic,
                }]
            }
            Command::DropTicket { id, reason } => {
                stakeholder_only(actor, "drop tickets")?;
                let ticket = self.require_ticket(&id)?;
                if ticket.dropped.is_some() {
                    return Err(CommandError::Invalid(format!("{id} is already dropped")));
                }
                if !s.droppable(ticket) {
                    return Err(CommandError::Invalid(format!(
                        "{id} is already in dev: file a bug instead of dropping it"
                    )));
                }
                let reason = non_empty(reason, "reason")?;
                let mut events = Vec::new();
                if ticket.assignee.is_some() {
                    events.push(EventKind::TicketAssigned {
                        id: id.clone(),
                        assignee: None,
                    });
                }
                events.push(EventKind::TicketDropped { id, reason });
                events
            }
            Command::RequestSprintPlanning { note } => {
                stakeholder_only(actor, "ask for sprint planning")?;
                if let Some(sp) = s.sprint_in_flight() {
                    return Err(CommandError::Invalid(format!(
                        "sprint {} is still {}: finish or close it first",
                        sp.id,
                        if sp.status == SprintStatus::Proposed {
                            "waiting for your approval"
                        } else {
                            "active"
                        }
                    )));
                }
                let note = note.as_deref().and_then(non_blank);
                vec![EventKind::SprintPlanningRequested { note }]
            }
            Command::RecordPlanningBriefed => {
                system_only(actor, "record planning briefings")?;
                if s.planning.as_ref().is_some_and(|p| !p.briefed) {
                    vec![EventKind::SprintPlanningBriefed]
                } else {
                    Vec::new()
                }
            }
            Command::SubmitRequest { text, start_now } => {
                stakeholder_only(actor, "submit requests")?;
                let text = non_empty(text, "request")?;
                if text.chars().count() > crate::intake::MAX_TEXT {
                    return Err(CommandError::Invalid(format!(
                        "a request is at most {} characters",
                        crate::intake::MAX_TEXT
                    )));
                }
                vec![EventKind::RequestSubmitted {
                    id: format!("R-{}", s.requests.len() + 1),
                    text,
                    start_now,
                    from: None,
                    kind: None,
                    ticket: None,
                }]
            }
            Command::SuggestWork { kind, text, ticket } => {
                let from = employee_only(actor, "suggest work")?;
                self.require_employee(&from)?;
                let text = non_empty(text, "suggestion")?;
                if text.chars().count() > crate::intake::MAX_TEXT {
                    return Err(CommandError::Invalid(format!(
                        "a suggestion is at most {} characters",
                        crate::intake::MAX_TEXT
                    )));
                }
                if let Some(ticket) = &ticket {
                    self.require_ticket(ticket)?;
                }
                let open = crate::intake::open_requests(s)
                    .filter(|r| r.from.as_deref() == Some(from.as_str()))
                    .count();
                if open >= self.team.max_open_suggestions as usize {
                    return Err(CommandError::Invalid(format!(
                        "you have {open} suggestions waiting for the project manager; \
                         wait for them to be handled"
                    )));
                }
                vec![EventKind::RequestSubmitted {
                    id: format!("R-{}", s.requests.len() + 1),
                    text,
                    start_now: false,
                    from: Some(from),
                    kind: Some(kind),
                    ticket,
                }]
            }
            Command::RecordRequestBriefed { id } => {
                system_only(actor, "record request briefings")?;
                self.require_open_request(&id)?;
                vec![EventKind::RequestBriefed { id }]
            }
            Command::AbandonRequest { id } => {
                system_only(actor, "abandon requests")?;
                self.require_open_request(&id)?;
                vec![EventKind::RequestAbandoned { id }]
            }
            Command::LinkRequestTicket { request, ticket } => {
                not_stakeholder(actor, "link tickets to requests")?;
                let req = self.require_open_request(&request)?;
                let t = self.require_ticket(&ticket)?;
                if req.tickets.contains(&ticket) {
                    return Ok(Vec::new());
                }
                self.link_request_events(req, t)
            }
            Command::CompleteRequest { id, summary } => {
                not_stakeholder(actor, "finish requests")?;
                let req = self.require_open_request(&id)?;
                let summary = summary.trim().to_string();
                let (title, detail) = crate::intake::note(s, req, &summary);
                vec![
                    EventKind::RequestTriaged { id, summary },
                    EventKind::AlertRaised {
                        id: s.next_inbox_id(),
                        title,
                        detail,
                    },
                ]
            }
            Command::ResolveProblem { key, summary } => {
                self.require_planner(actor, "resolve problems")?;
                let key = non_empty(key, "problem key")?;
                let summary = non_empty(summary, "summary")?;
                vec![
                    EventKind::ProblemResolved {
                        key: key.clone(),
                        summary: summary.clone(),
                    },
                    EventKind::AlertRaised {
                        id: s.next_inbox_id(),
                        title: format!("Project manager handled {key}"),
                        detail: format!("PM handled: {summary}"),
                    },
                ]
            }
            Command::RetryAction { key } => {
                self.require_planner(actor, "retry actions")?;
                vec![EventKind::ActionRetryRequested {
                    key: non_empty(key, "problem key")?,
                }]
            }
            Command::RestartEmployee { employee } => {
                let me = self.require_planner(actor, "restart teammates")?;
                let view = self.require_employee(&employee)?;
                if view.employee.id == me {
                    return Err(CommandError::Invalid(
                        "you cannot restart yourself; ask the stakeholder".into(),
                    ));
                }
                if view.stopped {
                    return Err(CommandError::Invalid(format!(
                        "{} was stopped by the stakeholder; leave them until the stakeholder writes to them",
                        view.employee.name
                    )));
                }
                vec![EventKind::EmployeeRestarted { employee }]
            }
            Command::CloseSprint { id } => {
                stakeholder_only(actor, "close sprints")?;
                let sprint = s
                    .sprint(&id)
                    .ok_or_else(|| CommandError::UnknownSprint(id.clone()))?;
                if sprint.status != SprintStatus::Active {
                    return Err(CommandError::Invalid(format!(
                        "sprint {id} is not active; only an active sprint can be closed"
                    )));
                }
                if s.open_release()
                    .is_some_and(|r| r.sprint.as_deref() == Some(id.as_str()))
                {
                    return Err(CommandError::Invalid(format!(
                        "sprint {id} has a release in progress"
                    )));
                }
                let carried: Vec<TicketId> = s
                    .sprint_tickets(sprint)
                    .filter(|t| !s.is_finished(t))
                    .map(|t| t.id.clone())
                    .collect();
                let mut events = Vec::new();
                // Planned but not started work goes back to the backlog.
                if let Some(planned) = self.workflow.planned_state() {
                    let initial = self.workflow.initial_state();
                    for t in carried.iter().filter_map(|c| s.ticket(c)) {
                        if t.state == planned
                            && self.workflow.check_transition(planned, initial).is_ok()
                        {
                            events.push(EventKind::TicketTransitioned {
                                id: t.id.clone(),
                                from: planned.to_string(),
                                to: initial.to_string(),
                            });
                        }
                    }
                }
                events.push(EventKind::SprintClosed { id, carried });
                events
            }
            Command::AssignTicket { id, assignee } => {
                self.require_ticket(&id)?;
                if let Some(assignee) = &assignee {
                    let hire = self.require_employee(assignee)?;
                    // Employees may not hand build work to a role that cannot do it; the
                    // stakeholder and Chillax (which repairs bad assignments) may.
                    if actor.employee().is_some() {
                        self.check_can_build(&id, &hire.employee.role, assignee)?;
                    }
                }
                vec![EventKind::TicketAssigned { id, assignee }]
            }
            Command::TransitionTicket { id, to } => {
                let ticket = self.require_ticket(&id)?;
                // The dev loop moves tickets forward itself, so branches, tests and
                // reviews cannot be skipped.
                if *actor != Actor::System
                    && let Some(how) = self.loop_owned(ticket, &to)
                {
                    return Err(CommandError::Invalid(format!("{id} → {to}: {how}")));
                }
                let transition = self.workflow.check_transition(&ticket.state, &to)?;
                let missing = transition.unmet(&ticket.checks);
                if !missing.is_empty() {
                    return Err(CommandError::RequirementsUnmet {
                        from: ticket.state.clone(),
                        to,
                        missing,
                    });
                }
                // The sprint_start gate: work is only planned once the stakeholder has
                // approved the sprint that contains it.
                if !s.sprint_gate_allows(ticket, &to) {
                    return Err(CommandError::Invalid(format!(
                        "{id} is not in an approved sprint"
                    )));
                }
                vec![EventKind::TicketTransitioned {
                    from: ticket.state.clone(),
                    id,
                    to,
                }]
            }
            Command::CommentTicket {
                id,
                body,
                external_id,
                attachments,
            } => {
                let ticket = self.require_ticket(&id)?;
                // Imports are retried after crashes and webhook redeliveries.
                if let Some(ext) = &external_id
                    && ticket
                        .comments
                        .iter()
                        .any(|c| c.external_id.as_ref() == Some(ext))
                {
                    return Ok(Vec::new());
                }
                crate::attachment::validate(&attachments)?;
                // A picture may be the whole comment.
                let body = if attachments.is_empty() {
                    non_empty(body, "comment")?
                } else {
                    body.trim().to_string()
                };
                vec![EventKind::TicketCommented {
                    id,
                    body,
                    external_id,
                    attachments,
                }]
            }
            Command::ProposeSprint {
                name,
                goal,
                tickets,
            } => {
                if tickets.is_empty() {
                    return Err(CommandError::Invalid("a sprint needs tickets".into()));
                }
                for id in &tickets {
                    let ticket = self.require_ticket(id)?;
                    if ticket.dropped.is_some() {
                        return Err(CommandError::Invalid(format!("{id} was dropped")));
                    }
                    // Leftovers of a sprint that closed or shipped can join the next one.
                    if let Some(sp) = ticket.sprint.as_deref().and_then(|sp| s.sprint(sp))
                        && matches!(sp.status, SprintStatus::Proposed | SprintStatus::Active)
                    {
                        return Err(CommandError::Invalid(format!(
                            "{id} is already in sprint {}",
                            sp.id
                        )));
                    }
                }
                vec![EventKind::SprintProposed {
                    sprint: Sprint {
                        id: format!("SP-{}", s.sprints.len() + 1),
                        name: non_empty(name, "name")?,
                        goal,
                        tickets,
                        status: SprintStatus::Proposed,
                        external: None,
                    },
                    gate: s.next_inbox_id(),
                }]
            }
            Command::ApproveSprint { id } => {
                stakeholder_only(actor, "approve sprints")?;
                let sprint = self.require_proposed_sprint(&id)?;
                let mut events = vec![EventKind::SprintApproved { id: id.clone() }];
                let initial = self.workflow.initial_state();
                if let Some(planned) = self.workflow.planned_state() {
                    for ticket in sprint.tickets.iter().filter_map(|t| s.ticket(t)) {
                        if ticket.state == initial {
                            events.push(EventKind::TicketTransitioned {
                                id: ticket.id.clone(),
                                from: initial.to_string(),
                                to: planned.to_string(),
                            });
                        }
                    }
                }
                events
            }
            Command::RejectSprint { id, reason } => {
                stakeholder_only(actor, "reject sprints")?;
                self.require_proposed_sprint(&id)?;
                vec![EventKind::SprintRejected { id, reason }]
            }
            Command::SetBlockers { id, blocked_by } => {
                self.require_ticket(&id)?;
                let mut unique = Vec::new();
                for blocker in blocked_by {
                    self.require_ticket(&blocker)?;
                    if blocker == id || self.depends_on(&blocker, &id) {
                        return Err(CommandError::Invalid(format!(
                            "{blocker} cannot block {id}: that would be a cycle"
                        )));
                    }
                    if !unique.contains(&blocker) {
                        unique.push(blocker);
                    }
                }
                vec![EventKind::TicketBlockersSet {
                    id,
                    blocked_by: unique,
                }]
            }
            Command::Handoff { id, role, note } => {
                let ticket = self.require_ticket(&id)?;
                let from = match (&ticket.assignee, actor.employee()) {
                    (Some(owner), Some(me)) if owner != me => {
                        return Err(CommandError::Invalid(format!(
                            "{id} is assigned to {owner}, not you"
                        )));
                    }
                    (Some(owner), _) => owner.clone(),
                    (None, Some(me)) => me.to_string(),
                    (None, None) => "nobody".to_string(),
                };
                let to = self
                    .least_loaded(&role, Some(&from))
                    .ok_or_else(|| CommandError::Invalid(format!("no one with role {role}")))?;
                vec![
                    EventKind::TicketHandedOff {
                        id: id.clone(),
                        from,
                        to: to.clone(),
                        note: note.clone(),
                    },
                    EventKind::TicketAssigned {
                        id: id.clone(),
                        assignee: Some(to),
                    },
                    comment(&id, &format!("Handed off. {note}")),
                ]
            }
            Command::StartWork {
                id,
                branch,
                worktree,
            } => {
                system_only(actor, "start work")?;
                let ticket = self.require_ticket(&id)?;
                let employee = ticket
                    .assignee
                    .clone()
                    .ok_or_else(|| CommandError::Invalid(format!("{id} has no assignee")))?;
                self.require_employee(&employee)?;
                if !s.blockers_clear(ticket) {
                    return Err(CommandError::Invalid(format!("{id} is still blocked")));
                }
                if ticket.state != self.workflow.planned_state().unwrap_or_default() {
                    return Err(CommandError::Invalid(format!(
                        "{id} is {}, not planned",
                        ticket.state
                    )));
                }
                let moved = self.dev_transition(ticket, IN_PROGRESS, &[])?;
                vec![
                    EventKind::WorkStarted {
                        id: id.clone(),
                        employee: employee.clone(),
                        branch: non_empty(branch, "branch")?,
                        worktree: non_empty(worktree, "worktree")?,
                    },
                    moved,
                    comment(&id, &format!("{employee} started work.")),
                ]
            }
            Command::RequestReview { id, summary } => {
                let employee = employee_only(actor, "request a review")?;
                let ticket = self.require_ticket(&id)?;
                if ticket.assignee.as_deref() != Some(employee.as_str()) {
                    return Err(CommandError::Invalid(format!(
                        "{id} is not assigned to you"
                    )));
                }
                // A conflicted ticket in review asks for the tests the same way.
                let resolving = ticket.state == IN_REVIEW && ticket.conflict.is_some();
                if ticket.state != IN_PROGRESS && !resolving {
                    return Err(CommandError::Invalid(format!(
                        "{id} is {}, not in progress",
                        ticket.state
                    )));
                }
                vec![EventKind::ReviewRequested {
                    id,
                    employee,
                    summary: summary.trim().to_string(),
                }]
            }
            Command::RecordTests {
                id,
                passed,
                skipped,
                output_tail,
            } => {
                system_only(actor, "record test runs")?;
                let ticket = self.require_ticket(&id)?;
                let resolving = ticket.state == IN_REVIEW && ticket.conflict.is_some();
                if ticket.state != IN_PROGRESS && !resolving {
                    return Err(CommandError::Invalid(format!(
                        "{id} is {}, not in progress",
                        ticket.state
                    )));
                }
                let note = match (skipped, passed) {
                    (true, _) => "Tests skipped: no test command is configured.".to_string(),
                    (false, true) => "Tests passed.".to_string(),
                    (false, false) => format!("Tests failed.\n\n{output_tail}"),
                };
                let mut events = vec![EventKind::TestsRun {
                    id: id.clone(),
                    passed,
                    skipped,
                    output_tail,
                }];
                // A resolution that broke the tests is ordinary rework: the approval goes.
                if resolving && !passed && !skipped {
                    events.push(self.dev_transition(ticket, IN_PROGRESS, &[])?);
                }
                events.push(comment(&id, &note));
                events
            }
            Command::RecordPr { id, number, url } => {
                system_only(actor, "record pull requests")?;
                self.require_ticket(&id)?;
                vec![EventKind::PrOpened {
                    id,
                    number,
                    url: non_empty(url, "pull request url")?,
                }]
            }
            Command::StartReview { id, reviewer } => {
                system_only(actor, "start a review")?;
                let ticket = self.require_ticket(&id)?;
                self.require_employee(&reviewer)?;
                let own_work = ticket.assignee.as_deref() == Some(reviewer.as_str());
                if own_work && !self.sole_reviewer(&reviewer) {
                    return Err(CommandError::Invalid(
                        "no one reviews their own work".into(),
                    ));
                }
                let moved = self.dev_transition(ticket, IN_REVIEW, &[])?;
                let mut events = vec![
                    EventKind::ReviewStarted {
                        id: id.clone(),
                        reviewer: reviewer.clone(),
                    },
                    moved,
                    comment(&id, &format!("{reviewer} is reviewing.")),
                ];
                if own_work {
                    events.push(comment(
                        &id,
                        "Self-review: no one else can review, so the author reviews their own \
                         work. Look at this change closely before releasing it.",
                    ));
                }
                events
            }
            Command::SubmitReview { id, verdict, body } => {
                let reviewer = employee_only(actor, "submit a review")?;
                let ticket = self.require_ticket(&id)?;
                if ticket.reviewer.as_deref() != Some(reviewer.as_str()) {
                    return Err(CommandError::Invalid(format!(
                        "you are not the reviewer of {id}"
                    )));
                }
                if ticket.state != IN_REVIEW {
                    return Err(CommandError::Invalid(format!(
                        "{id} is {}, not in review",
                        ticket.state
                    )));
                }
                let body = body.trim().to_string();
                let mut events = vec![EventKind::ReviewSubmitted {
                    id: id.clone(),
                    reviewer,
                    verdict,
                    body: body.clone(),
                }];
                match verdict {
                    ReviewVerdict::Approve => {
                        events.push(comment(&id, &format!("Review approved. {body}")));
                    }
                    ReviewVerdict::RequestChanges => {
                        non_empty(body.clone(), "review feedback")?;
                        events.push(self.dev_transition(ticket, IN_PROGRESS, &[])?);
                        events.push(comment(&id, &format!("Changes requested.\n\n{body}")));
                    }
                }
                events
            }
            Command::RecordMerge { id, commit } => {
                system_only(actor, "record merges")?;
                let ticket = self.require_ticket(&id)?;
                let moved = self.dev_transition(ticket, IN_QA, &["merged_to_dev"])?;
                vec![
                    EventKind::BranchMerged {
                        id: id.clone(),
                        commit: non_empty(commit.clone(), "commit")?,
                    },
                    moved,
                    comment(&id, &format!("Merged {commit} into dev.")),
                ]
            }
            Command::RecordMergeFailed { id, reason } => {
                system_only(actor, "record merge failures")?;
                let ticket = self.require_ticket(&id)?;
                let moved = self.dev_transition(ticket, IN_PROGRESS, &[])?;
                vec![
                    EventKind::MergeFailed {
                        id: id.clone(),
                        reason: reason.clone(),
                    },
                    moved,
                    comment(&id, &format!("Could not merge into dev.\n\n{reason}")),
                ]
            }
            Command::RecordRebase { id, outcome, note } => {
                system_only(actor, "record rebases")?;
                self.require_ticket(&id)?;
                vec![EventKind::BranchRebased { id, outcome, note }]
            }
            Command::RecordSync {
                id,
                outcome,
                files,
                note,
            } => {
                system_only(actor, "record syncs with dev")?;
                let ticket = self.require_ticket(&id)?;
                self.decide_sync(ticket, outcome, files, note)?
            }
            Command::RecordConflictBriefed { id } => {
                system_only(actor, "record conflict briefings")?;
                let ticket = self.require_ticket(&id)?;
                if ticket.conflict.is_none() {
                    return Err(CommandError::Invalid(format!("{id} has no conflict")));
                }
                vec![EventKind::ConflictBriefed { id }]
            }
            Command::RecordTriageBriefed { fingerprint } => {
                system_only(actor, "record triage briefings")?;
                self.require_failure(&fingerprint)?;
                vec![EventKind::TriageBriefed { fingerprint }]
            }
            Command::AbandonTriage { fingerprint } => {
                system_only(actor, "abandon triage")?;
                self.require_failure(&fingerprint)?;
                vec![EventKind::TriageAbandoned { fingerprint }]
            }
            Command::LinkTriageBug { fingerprint, bug } => {
                if *actor == Actor::Stakeholder {
                    return Err(CommandError::Invalid(
                        "only the project manager or Chillax can link a bug to a failure".into(),
                    ));
                }
                let failure = self.require_failure(&fingerprint)?;
                let terminal = self.workflow.terminal_state();
                if failure.status == TriageStatus::Filed
                    && let Some(open) = failure
                        .bug
                        .as_deref()
                        .and_then(|b| s.ticket(b))
                        .filter(|t| t.state != terminal)
                {
                    return Err(CommandError::Invalid(format!(
                        "this failure already has an open bug, {}; do not file another",
                        open.id
                    )));
                }
                let ticket = self.require_ticket(&bug)?;
                if ticket.kind != TicketKind::Bug {
                    return Err(CommandError::Invalid(format!("{bug} is not a bug")));
                }
                if failure.ticket.as_deref() == Some(bug.as_str()) {
                    return Err(CommandError::Invalid(
                        "a failure cannot be its own bug".into(),
                    ));
                }
                let sprint = triage::bug_sprint(s, failure);
                let mut events = vec![EventKind::TriageBugFiled {
                    fingerprint,
                    bug: bug.clone(),
                    sprint: sprint.clone(),
                }];
                if let (Some(_), Some(planned)) = (&sprint, self.workflow.planned_state())
                    && ticket.state == self.workflow.initial_state()
                {
                    events.push(EventKind::TicketTransitioned {
                        id: bug,
                        from: ticket.state.clone(),
                        to: planned.to_string(),
                    });
                }
                events
            }
            Command::PromoteToQa => {
                if actor.employee().is_some() {
                    return Err(CommandError::Invalid(
                        "only the stakeholder or Chillax can promote dev to qa".into(),
                    ));
                }
                self.require_no_open_round()?;
                if s.waiting_for_qa().is_empty() {
                    return Err(CommandError::Invalid("nothing is waiting in QA".into()));
                }
                if s.qa_requested {
                    Vec::new()
                } else {
                    vec![EventKind::QaPromotionRequested]
                }
            }
            Command::RecordPromotion { commit } => {
                system_only(actor, "record promotions")?;
                self.require_no_open_round()?;
                let tickets = if s.qa_requested {
                    s.waiting_for_qa()
                } else {
                    s.qa_ready()
                };
                let Some(first) = tickets.first() else {
                    return Err(CommandError::Invalid("nothing is waiting in QA".into()));
                };
                vec![EventKind::QaRoundStarted {
                    round: QaRound {
                        number: s.qa.as_ref().map_or(1, |r| r.number + 1),
                        sprint: first.sprint.clone(),
                        commit: non_empty(commit, "commit")?,
                        status: QaRoundStatus::Starting,
                        tickets: tickets.iter().map(|t| t.id.clone()).collect(),
                        verdicts: Default::default(),
                        env_retries: Default::default(),
                        up_done: false,
                    },
                }]
            }
            Command::RecordEnvRun {
                step,
                ok,
                skipped,
                output_tail,
            } => {
                system_only(actor, "record environment runs")?;
                if step == EnvStep::Deploy {
                    return Err(CommandError::Invalid(
                        "deploys belong to a release, not a QA round".into(),
                    ));
                }
                // `down` runs as a round wraps up; `up` and `test` need it open.
                let usable = match step {
                    EnvStep::Down => s.qa.is_some(),
                    _ => s.open_round().is_some(),
                };
                if !usable {
                    return Err(CommandError::Invalid("no QA round is open".into()));
                }
                let alert = (!ok && !skipped).then(|| s.next_inbox_id());
                vec![EventKind::EnvCommandRun {
                    step,
                    ok,
                    skipped,
                    output_tail,
                    alert,
                }]
            }
            Command::CloseQaRound => {
                system_only(actor, "close QA rounds")?;
                let round = match s.open_round() {
                    Some(r) if r.status == QaRoundStatus::Testing => r,
                    _ => return Err(CommandError::Invalid("no QA round is testing".into())),
                };
                // Tickets sent back to development drop out of the round.
                let untested: Vec<&str> = round
                    .tickets
                    .iter()
                    .filter(|id| s.ticket(id).is_some_and(|t| t.state == IN_QA))
                    .filter(|id| !round.verdicts.contains_key(*id))
                    .map(String::as_str)
                    .collect();
                if !untested.is_empty() {
                    return Err(CommandError::Invalid(format!(
                        "QA has not tested {} yet",
                        untested.join(", ")
                    )));
                }
                let mut events = vec![EventKind::QaRoundClosed {
                    number: round.number,
                }];
                // A clean round proposes its release once the whole sprint is done; one
                // with failures waits for the fix (or for the stakeholder to force the
                // gate), and QA keeps testing tickets as they arrive until then.
                let sprint_done = round
                    .sprint
                    .as_deref()
                    .and_then(|sp| s.sprint(sp))
                    .is_none_or(|sp| {
                        sp.tickets
                            .iter()
                            .all(|id| s.ticket(id).is_none_or(|t| s.is_settled(t)))
                    });
                if sprint_done
                    && release::known_issues(s).is_empty()
                    && !release::candidates(s).is_empty()
                    && s.open_release().is_none()
                {
                    events.push(self.propose_release(false)?);
                }
                events
            }
            Command::RetryEnv { env, step } => {
                stakeholder_only(actor, "retry environment steps")?;
                let retryable = match (env.as_str(), step) {
                    ("qa", EnvStep::Up | EnvStep::Test) => s
                        .open_round()
                        .is_some_and(|r| r.status == QaRoundStatus::EnvFailed),
                    // An approved release that could not be made is retried the same way.
                    ("prod", EnvStep::Deploy) => s.releases.last().is_some_and(|r| {
                        matches!(
                            r.status,
                            ReleaseStatus::DeployFailed | ReleaseStatus::Approved
                        )
                    }),
                    _ => false,
                };
                if !retryable {
                    return Err(CommandError::Invalid(format!(
                        "nothing to retry: {env} {step:?} has not failed"
                    )));
                }
                vec![EventKind::EnvRetryRequested { env, step }]
            }
            Command::StartApp { env } => self.request_app(actor, env, AppWant::Start)?,
            Command::StopApp { env } => self.request_app(actor, env, AppWant::Stop)?,
            Command::RestartApp { env } => self.request_app(actor, env, AppWant::Restart)?,
            Command::ConfigureApps { apps } => {
                system_only(actor, "configure apps")?;
                let same = apps.len() == s.apps.len()
                    && apps.iter().zip(&s.apps).all(|(info, app)| {
                        info.env == app.env && info.port == app.port && info.url == app.url
                    });
                if same {
                    Vec::new()
                } else {
                    vec![EventKind::AppsConfigured { apps }]
                }
            }
            Command::RecordAppStarted {
                env,
                port,
                url,
                commit,
            } => {
                system_only(actor, "record app starts")?;
                self.known_app(&env)?;
                vec![EventKind::AppStarted {
                    env,
                    port,
                    url,
                    commit,
                }]
            }
            Command::RecordAppStopped { env, reason } => {
                system_only(actor, "record app stops")?;
                self.known_app(&env)?;
                vec![EventKind::AppStopped { env, reason }]
            }
            Command::RecordAppCrashed {
                env,
                exit,
                tail,
                will_retry,
                commit,
            } => {
                system_only(actor, "record app crashes")?;
                self.known_app(&env)?;
                vec![EventKind::AppCrashed {
                    env,
                    exit,
                    tail,
                    will_retry,
                    commit,
                }]
            }
            Command::RecordAppRestarted {
                env,
                reason,
                attempt,
            } => {
                system_only(actor, "record app restarts")?;
                self.known_app(&env)?;
                vec![EventKind::AppRestarted {
                    env,
                    reason,
                    attempt,
                }]
            }
            Command::OpenReleaseGate { force } => {
                stakeholder_only(actor, "open the release gate")?;
                if let Some(r) = s.open_round() {
                    return Err(CommandError::Invalid(format!(
                        "QA round {} is still open",
                        r.number
                    )));
                }
                vec![self.propose_release(force)?]
            }
            Command::ApproveRelease { id, version, notes } => {
                stakeholder_only(actor, "approve releases")?;
                self.require_gated_release(&id)?;
                let parsed = release::Version::parse(&version).ok_or_else(|| {
                    CommandError::Invalid(format!("`{version}` is not a version like v1.2.3"))
                })?;
                if !release::is_newer(s, &parsed) {
                    return Err(CommandError::Invalid(format!(
                        "{parsed} is not newer than the last release"
                    )));
                }
                vec![EventKind::ReleaseApproved {
                    id,
                    version: parsed.to_string(),
                    notes: non_empty(notes, "release notes")?,
                }]
            }
            Command::RejectRelease { id, reason } => {
                stakeholder_only(actor, "reject releases")?;
                // An approved release that keeps failing can be given up on as well.
                match self.state.release(&id) {
                    Some(r) if r.status == ReleaseStatus::Approved => {}
                    _ => {
                        self.require_gated_release(&id)?;
                    }
                }
                vec![EventKind::ReleaseRejected { id, reason }]
            }
            Command::RecordRelease { id, tag, commit } => {
                system_only(actor, "record releases")?;
                match s.release(&id) {
                    Some(r) if r.status == ReleaseStatus::Approved => {}
                    _ => {
                        return Err(CommandError::Invalid(format!(
                            "release {id} is not approved"
                        )));
                    }
                }
                vec![EventKind::ReleaseTagged {
                    id,
                    tag: non_empty(tag, "tag")?,
                    commit: non_empty(commit, "commit")?,
                }]
            }
            Command::RecordDeploy {
                id,
                ok,
                skipped,
                output_tail,
            } => {
                system_only(actor, "record deploys")?;
                match s.release(&id) {
                    Some(r)
                        if matches!(
                            r.status,
                            ReleaseStatus::Tagged | ReleaseStatus::DeployFailed
                        ) => {}
                    _ => {
                        return Err(CommandError::Invalid(format!(
                            "release {id} is not waiting for a deploy"
                        )));
                    }
                }
                let alert = (!ok && !skipped).then(|| s.next_inbox_id());
                vec![EventKind::DeployRun {
                    id,
                    ok,
                    skipped,
                    output_tail,
                    alert,
                }]
            }
            Command::SubmitQaVerdict {
                id,
                verdict,
                criteria,
                notes,
                bug,
            } => self.qa_verdict(actor, id, verdict, criteria, notes, bug)?,
            Command::RaiseAlert { title, detail } => {
                system_only(actor, "raise alerts")?;
                let title = non_empty(title, "alert title")?;
                let detail = detail.trim().to_string();
                let known = s.inbox.iter().any(|item| {
                    matches!(item, InboxItem::Alert { title: t, detail: d, .. }
                        if *t == title && *d == detail)
                });
                if known {
                    Vec::new()
                } else {
                    vec![EventKind::AlertRaised {
                        id: s.next_inbox_id(),
                        title,
                        detail,
                    }]
                }
            }
            Command::DismissAlert { id } => {
                if actor.employee().is_some() {
                    return Err(CommandError::Invalid(
                        "only the stakeholder or Chillax can dismiss alerts".into(),
                    ));
                }
                if !s
                    .inbox
                    .iter()
                    .any(|item| matches!(item, InboxItem::Alert { id: i, .. } if *i == id))
                {
                    return Err(CommandError::Invalid(format!("{id} is not an alert")));
                }
                vec![EventKind::AlertDismissed { id }]
            }
            Command::PauseDispatch => {
                stakeholder_only(actor, "pause dispatch")?;
                if s.dispatch_paused {
                    Vec::new()
                } else {
                    vec![EventKind::DispatchPaused]
                }
            }
            Command::ResumeDispatch => {
                stakeholder_only(actor, "resume dispatch")?;
                if s.dispatch_paused {
                    vec![EventKind::DispatchResumed]
                } else {
                    Vec::new()
                }
            }
            Command::RefreshUsage => {
                stakeholder_only(actor, "refresh usage")?;
                vec![EventKind::UsageRefreshRequested]
            }
            Command::Kick => {
                stakeholder_only(actor, "refresh the dispatcher")?;
                vec![EventKind::DispatchKicked]
            }
            Command::RecordBackendLimit {
                backend,
                until,
                message,
            } => {
                system_only(actor, "record backend limits")?;
                let backend = non_empty(backend, "backend")?;
                let same = s
                    .backend_limits
                    .get(&backend)
                    .is_some_and(|known| known.until == until);
                if same {
                    Vec::new()
                } else {
                    vec![EventKind::BackendLimited {
                        backend,
                        until,
                        message: message.trim().to_string(),
                    }]
                }
            }
            Command::RecordBackendRecovered { backend } => {
                system_only(actor, "record backend recoveries")?;
                if s.backend_limits.contains_key(&backend) {
                    vec![EventKind::BackendRecovered { backend }]
                } else {
                    Vec::new()
                }
            }
            Command::LinkExternal {
                entity,
                id,
                external,
            } => {
                if actor.employee().is_some() {
                    return Err(CommandError::Invalid(
                        "employees cannot link tracker items".into(),
                    ));
                }
                let known = match entity {
                    ExternalEntity::Epic => s.epics.iter().any(|e| e.id == id),
                    ExternalEntity::Ticket => s.ticket(&id).is_some(),
                    ExternalEntity::Sprint => s.sprint(&id).is_some(),
                };
                if !known {
                    return Err(CommandError::Invalid(format!("no {entity:?} {id} to link")));
                }
                if non_blank(&external.id).is_none() {
                    return Err(CommandError::Invalid(
                        "external id must not be empty".into(),
                    ));
                }
                vec![EventKind::TrackerLinked {
                    entity,
                    id,
                    external,
                }]
            }
        };
        Ok(events)
    }

    fn known_app(&self, env: &str) -> Result<(), CommandError> {
        if self.state.app(env).is_some() {
            Ok(())
        } else {
            Err(CommandError::Invalid(format!("there is no {env} app")))
        }
    }

    /// The stakeholder's start, stop or restart of an app, checked against its status.
    fn request_app(
        &self,
        actor: &Actor,
        env: String,
        want: AppWant,
    ) -> Result<Vec<EventKind>, CommandError> {
        stakeholder_only(actor, "start, stop or restart apps")?;
        let Some(app) = self.state.app(&env) else {
            return Err(CommandError::Invalid(format!(
                "{env} has no app: set `run` and `port` under [env.{env}] in chillax.toml"
            )));
        };
        let refusal = match want {
            AppWant::Start if !app.can_start() => {
                Some(format!("the {env} app is already {}", app.status.label()))
            }
            AppWant::Stop if !app.can_stop() => Some(format!("the {env} app is not running")),
            AppWant::Restart if !app.can_restart() => {
                Some(format!("the {env} app is not running; start it instead"))
            }
            _ => None,
        };
        match refusal {
            Some(why) => Err(CommandError::Invalid(why)),
            None => Ok(vec![EventKind::AppRequested { env, want }]),
        }
    }

    /// Fold one event into the projection.
    pub fn apply(&mut self, event: &Event) {
        let actor_name = self.state.actor_name(&event.actor);
        let s = &mut self.state;
        s.last_seq = event.seq;
        match &event.kind {
            EventKind::EmployeeHired { employee } => {
                *s.hires_by_role.entry(employee.role.clone()).or_default() += 1;
                s.chat(&employee.id);
                s.employees.push(EmployeeView {
                    employee: employee.clone(),
                    status: EmployeeStatus::Offline,
                    last_progress: None,
                    turn_open: false,
                    stopped: false,
                    reply_pending: false,
                    pending_agent: None,
                });
            }
            EventKind::RoleAdopted { markdown, .. } => match Role::parse(markdown) {
                Ok(role) => self.roles.insert(role),
                Err(err) => tracing::warn!(%err, "ignoring unparsable adopted role"),
            },
            EventKind::HiringDefaultsSet {
                backend,
                model,
                effort,
            } => {
                s.hiring = HiringDefaults {
                    backend: backend.clone(),
                    model: model.clone(),
                    effort: effort.clone(),
                };
            }
            EventKind::EmployeeAgentChanged {
                employee,
                backend,
                model,
                effort,
            } => {
                if let Some(e) = s.employee_mut(employee) {
                    e.employee.backend = backend.clone();
                    e.employee.model = model.clone();
                    e.employee.effort = effort.clone();
                    e.stopped = false;
                    e.pending_agent = None;
                }
                s.push_line(
                    employee,
                    Speaker::System,
                    format!(
                        "Now running on {}",
                        crate::event::agent_label(backend, model, effort)
                    ),
                );
            }
            EventKind::EmployeeAgentChangeQueued {
                employee,
                backend,
                model,
                effort,
            } => {
                let mut withdrawn = false;
                if let Some(e) = s.employee_mut(employee) {
                    withdrawn = backend == &e.employee.backend
                        && model == &e.employee.model
                        && effort == &e.employee.effort;
                    e.pending_agent = (!withdrawn).then(|| PendingAgent {
                        backend: backend.clone(),
                        model: model.clone(),
                        effort: effort.clone(),
                    });
                }
                let line = if withdrawn {
                    "Agent change cancelled".to_string()
                } else {
                    format!(
                        "Will switch to {} after this reply",
                        crate::event::agent_label(backend, model, effort)
                    )
                };
                s.push_line(employee, Speaker::System, line);
            }
            EventKind::EmployeeLetGo { employee } => {
                s.employees.retain(|e| e.employee.id != *employee);
                s.live.remove(employee);
                s.inbox.retain(|item| match item {
                    InboxItem::Permission { employee: e, .. }
                    | InboxItem::Question { employee: e, .. } => e != employee,
                    InboxItem::SprintGate { .. }
                    | InboxItem::EnvAlert { .. }
                    | InboxItem::ReleaseGate { .. }
                    | InboxItem::Alert { .. } => true,
                });
                s.push_line(employee, Speaker::System, "Let go.");
            }
            EventKind::ChatMessageSent { to, text } => {
                let speaker = match &event.actor {
                    Actor::Employee(from) => {
                        s.push_line(from, Speaker::System, format!("→ message to {to}: {text}"));
                        Speaker::Teammate(actor_name.clone())
                    }
                    _ => {
                        if event.actor == Actor::Stakeholder
                            && let Some(e) = s.employee_mut(to)
                        {
                            e.stopped = false;
                            e.reply_pending = true;
                        }
                        Speaker::Stakeholder
                    }
                };
                s.push_line(to, speaker, text.clone());
                let from = match &event.actor {
                    Actor::System => None,
                    Actor::Stakeholder => Some(None),
                    Actor::Employee(id) => Some(Some(id.clone())),
                };
                if let Some(from) = from {
                    s.push_company_chat(CompanyChatEntry {
                        seq: event.seq,
                        ts: event.ts,
                        from,
                        from_name: actor_name,
                        to: Some(to.clone()),
                        mentions: Vec::new(),
                        text: text.clone(),
                    });
                }
            }
            EventKind::CompanyChatPosted { text, mentions } => {
                s.push_company_chat(CompanyChatEntry {
                    seq: event.seq,
                    ts: event.ts,
                    from: None,
                    from_name: actor_name,
                    to: None,
                    mentions: mentions.clone(),
                    text: text.clone(),
                });
                for id in mentions {
                    if let Some(e) = s.employee_mut(id) {
                        e.stopped = false;
                        e.reply_pending = true;
                        s.push_line(id, Speaker::Stakeholder, format!("(company chat) {text}"));
                    }
                }
            }
            EventKind::AgentStarted { employee, .. } => {
                if let Some(e) = s.employee_mut(employee) {
                    e.status = EmployeeStatus::Idle;
                }
            }
            EventKind::AgentMessageChunk { employee, text } => {
                let open = s.employee_mut(employee).is_some_and(|e| {
                    e.status = EmployeeStatus::Working;
                    std::mem::replace(&mut e.turn_open, true)
                });
                let chat = s.chat(employee);
                match chat.last_mut() {
                    Some(line) if open && line.speaker == Speaker::Agent => {
                        line.text.push_str(text)
                    }
                    _ => chat.push(ChatLine {
                        speaker: Speaker::Agent,
                        text: text.clone(),
                        tool_call_id: None,
                    }),
                }
            }
            EventKind::AgentToolCall {
                employee,
                tool_call_id,
                title,
                tool_kind,
                status,
            } => {
                if let Some(e) = s.employee_mut(employee) {
                    e.status = EmployeeStatus::Working;
                    e.turn_open = true;
                }
                s.chat(employee).push(ChatLine {
                    speaker: Speaker::Tool,
                    text: tool_text(title, status),
                    tool_call_id: Some(tool_call_id.clone()),
                });
                let live = s.live.entry(employee.clone()).or_default();
                live.tools.push(LiveTool {
                    tool_call_id: tool_call_id.clone(),
                    title: title.clone(),
                    tool_kind: tool_kind.clone(),
                    status: status.clone(),
                    content: Vec::new(),
                });
                if live.tools.len() > LIVE_TOOLS_KEPT {
                    live.tools.remove(0);
                }
            }
            EventKind::AgentToolCallUpdated {
                employee,
                tool_call_id,
                title,
                status,
            } => {
                if let Some(line) = s
                    .chat(employee)
                    .iter_mut()
                    .rev()
                    .find(|l| l.tool_call_id.as_deref() == Some(tool_call_id))
                {
                    let (old_title, old_status) = split_tool_text(&line.text);
                    line.text = tool_text(
                        title.as_deref().unwrap_or(old_title),
                        status.as_deref().unwrap_or(old_status),
                    );
                }
                if let Some(tool) = s.live_tool(employee, tool_call_id) {
                    if let Some(title) = title {
                        tool.title = title.clone();
                    }
                    if let Some(status) = status {
                        tool.status = status.clone();
                    }
                }
            }
            EventKind::AgentTurnEnded { employee, .. } => {
                s.usage.turn_ended(employee);
                if let Some(e) = s.employee_mut(employee) {
                    e.status = EmployeeStatus::Idle;
                    e.turn_open = false;
                    e.reply_pending = false;
                }
            }
            EventKind::MaxConcurrentAgentsChanged { n, hardware_max } => {
                s.agent_limit = Some(AgentLimit {
                    limit: *n,
                    hardware_max: *hardware_max,
                });
            }
            EventKind::TicketPrioritized { id, urgent } => {
                let mut newly = None;
                if let Some(t) = s.ticket_mut(id) {
                    if *urgent && !t.urgent {
                        newly = Some(t.title.clone());
                    }
                    t.urgent = *urgent;
                }
                // Tell the planners in their chat, so the PM sees what the stakeholder asked.
                if let Some(title) = newly {
                    let planner = self.workflow.initial_state();
                    let pms: Vec<EmployeeId> = s
                        .employees
                        .iter()
                        .filter(|e| {
                            self.roles
                                .get(&e.employee.role)
                                .is_some_and(|r| self.workflow.role_owns(r, planner))
                        })
                        .map(|e| e.employee.id.clone())
                        .collect();
                    for pm in pms {
                        s.push_line(
                            &pm,
                            Speaker::System,
                            format!("The stakeholder marked {id} urgent: {title}. It is taken up before other work."),
                        );
                    }
                }
            }
            EventKind::TurnCancelled { employee } => {
                if let Some(e) = s.employee_mut(employee) {
                    e.stopped = e.stopped || e.turn_open || e.reply_pending;
                    e.status = EmployeeStatus::Idle;
                    e.turn_open = false;
                    e.reply_pending = false;
                }
                s.inbox.retain(
                    |i| !matches!(i, InboxItem::Permission { employee: e, .. } if e == employee),
                );
                s.push_line(employee, Speaker::System, "Stopped.");
            }
            EventKind::UsageRecorded {
                employee,
                ticket,
                session,
                model,
                input_tokens,
                output_tokens,
                cached_tokens,
                cache_write_tokens,
                cost_micros,
                currency,
                estimated,
                context_used,
                context_size,
            } => s.usage.record(
                event.seq,
                event.ts,
                &crate::usage::UsageInput {
                    employee,
                    ticket: ticket.as_deref(),
                    session,
                    model: model.as_deref(),
                    input_tokens: *input_tokens,
                    output_tokens: *output_tokens,
                    cached_tokens: *cached_tokens,
                    cache_write_tokens: *cache_write_tokens,
                    cost_micros: *cost_micros,
                    currency,
                    estimated: *estimated,
                    context_used: *context_used,
                    context_size: *context_size,
                },
            ),
            EventKind::AgentFailed {
                employee, error, ..
            } => {
                s.usage.session_closed(employee);
                if let Some(e) = s.employee_mut(employee) {
                    e.status = EmployeeStatus::Failed;
                    e.turn_open = false;
                    e.reply_pending = false;
                }
                s.push_line(employee, Speaker::System, format!("⚠ {error}"));
            }
            EventKind::EmployeeRestarted { employee } => {
                if let Some(e) = s.employee_mut(employee) {
                    e.status = EmployeeStatus::Offline;
                    e.turn_open = false;
                    e.reply_pending = false;
                }
            }
            EventKind::AgentStopped { employee } => {
                s.usage.session_closed(employee);
                if let Some(e) = s.employee_mut(employee) {
                    e.status = EmployeeStatus::Offline;
                    e.turn_open = false;
                    e.reply_pending = false;
                }
            }
            EventKind::PermissionRequested {
                id,
                employee,
                title,
                choices,
            } => {
                s.bump_inbox(id);
                s.inbox.push(InboxItem::Permission {
                    id: id.clone(),
                    employee: employee.clone(),
                    title: title.clone(),
                    choices: choices.clone(),
                });
                if let Some(e) = s.employee_mut(employee) {
                    e.status = EmployeeStatus::WaitingOnStakeholder;
                }
                s.push_line(
                    employee,
                    Speaker::System,
                    format!("🔒 {id} asks permission: {title}"),
                );
            }
            EventKind::PermissionResolved { id, choice, .. } => {
                if let Some(pos) = s.inbox.iter().position(|i| i.id() == id) {
                    let item = s.inbox.remove(pos);
                    if let InboxItem::Permission {
                        employee, choices, ..
                    } = item
                    {
                        if let Some(e) = s.employee_mut(&employee)
                            && e.status == EmployeeStatus::WaitingOnStakeholder
                        {
                            e.status = EmployeeStatus::Working;
                        }
                        let name = choice
                            .as_deref()
                            .and_then(|c| choices.iter().find(|o| o.id == c))
                            .map_or("cancelled", |o| o.name.as_str());
                        s.push_line(&employee, Speaker::System, format!("🔓 {id}: {name}"));
                    }
                }
            }
            EventKind::QuestionAsked {
                id,
                employee,
                question,
            } => {
                s.bump_inbox(id);
                s.inbox.push(InboxItem::Question {
                    id: id.clone(),
                    employee: employee.clone(),
                    question: question.clone(),
                });
                s.push_line(employee, Speaker::System, format!("❓ {id}: {question}"));
            }
            EventKind::QuestionAnswered { id, answer } => {
                if let Some(pos) = s.inbox.iter().position(|i| i.id() == id)
                    && let InboxItem::Question { employee, .. } = s.inbox.remove(pos)
                {
                    s.push_line(
                        &employee,
                        Speaker::Stakeholder,
                        format!("(answer to {id}) {answer}"),
                    );
                }
            }
            EventKind::ProgressReported { employee, summary } => {
                if let Some(e) = s.employee_mut(employee) {
                    e.last_progress = Some(summary.clone());
                }
            }
            EventKind::ActivityCleared => s.activity_cleared_seq = event.seq,
            EventKind::ChatCleared { employee } => {
                if let Some(e) = s.employee_mut(employee) {
                    e.reply_pending = false;
                }
                s.chats.remove(employee);
                // The transcript appends live tool calls the chat has no line for, so a
                // cleared chat must drop them too (the plan stays).
                if let Some(l) = s.live.get_mut(employee) {
                    l.tools.clear();
                }
            }
            EventKind::EmployeeContextCleared { employee, .. } => {
                // Like ChatCleared for the buffer, but the plan stays and no session
                // closes: only the transcript view is emptied.
                if let Some(e) = s.employee_mut(employee) {
                    e.reply_pending = false;
                }
                s.chats.remove(employee);
                if let Some(l) = s.live.get_mut(employee) {
                    l.tools.clear();
                }
            }
            EventKind::EpicCreated { epic } => s.epics.push(epic.clone()),
            EventKind::TicketCreated { ticket } => s.tickets.push(ticket.clone()),
            EventKind::TicketUpdated {
                id,
                title,
                description,
                acceptance_criteria,
                estimate,
                epic,
            } => {
                if let Some(t) = s.ticket_mut(id) {
                    if let Some(v) = epic {
                        t.epic = Some(v.clone());
                    }
                    if let Some(v) = title {
                        t.title = v.clone();
                    }
                    if let Some(v) = description {
                        t.description = v.clone();
                    }
                    if let Some(v) = acceptance_criteria {
                        t.acceptance_criteria = v.clone();
                    }
                    if let Some(v) = estimate {
                        t.estimate = Some(*v);
                    }
                }
            }
            EventKind::TicketAssigned { id, assignee } => {
                if let Some(t) = s.ticket_mut(id) {
                    t.assignee = assignee.clone();
                }
            }
            EventKind::TicketDropped { id, reason } => {
                let mut sprint = None;
                if let Some(t) = s.ticket_mut(id) {
                    t.dropped = Some(reason.clone());
                    t.assignee = None;
                    t.reviewer = None;
                    t.review_requested = false;
                    t.needs_rebase = false;
                    sprint = t.sprint.clone();
                }
                // A running sprint whose every ticket was dropped has nothing to ship.
                if let Some(sp) = sprint.and_then(|sp| s.sprint(&sp).cloned())
                    && sp.status == SprintStatus::Active
                    && s.sprint_tickets(&sp).next().is_none()
                {
                    set_sprint_status(s, &sp.id, SprintStatus::Closed);
                }
            }
            EventKind::SprintPlanningRequested { note } => {
                s.planning = Some(PlanningRequest {
                    note: note.clone(),
                    briefed: false,
                });
            }
            EventKind::RequestSubmitted {
                id,
                text,
                start_now,
                from,
                kind,
                ticket,
            } => s.requests.push(Request {
                id: id.clone(),
                text: text.clone(),
                start_now: *start_now,
                status: RequestStatus::Submitted,
                attempts: 0,
                tickets: Vec::new(),
                summary: None,
                from: from.clone(),
                kind: *kind,
                ticket: ticket.clone(),
            }),
            EventKind::RequestBriefed { id } => {
                if let Some(r) = s.requests.iter_mut().find(|r| r.id == *id) {
                    r.attempts += 1;
                    r.status = RequestStatus::Briefed;
                }
            }
            EventKind::RequestAbandoned { id } => {
                if let Some(r) = s.requests.iter_mut().find(|r| r.id == *id) {
                    r.status = RequestStatus::Abandoned;
                }
            }
            EventKind::RequestTicketLinked {
                request,
                ticket,
                sprint,
            } => {
                if let Some(r) = s.requests.iter_mut().find(|r| r.id == *request) {
                    r.tickets.push(ticket.clone());
                }
                if let Some(sp) = sprint {
                    if let Some(t) = s.ticket_mut(ticket) {
                        t.sprint = Some(sp.clone());
                    }
                    if let Some(sp) = s.sprints.iter_mut().find(|x| x.id == *sp) {
                        sp.tickets.push(ticket.clone());
                    }
                }
            }
            EventKind::RequestTriaged { id, summary } => {
                if let Some(r) = s.requests.iter_mut().find(|r| r.id == *id) {
                    r.status = RequestStatus::Triaged;
                    r.summary = Some(summary.clone()).filter(|x| !x.is_empty());
                }
            }
            EventKind::SprintPlanningBriefed => {
                if let Some(p) = s.planning.as_mut() {
                    p.briefed = true;
                }
            }
            EventKind::SprintClosed { id, carried } => {
                set_sprint_status(s, id, SprintStatus::Closed);
                if let Some(sp) = s.sprints.iter_mut().find(|sp| sp.id == *id) {
                    sp.tickets.retain(|t| !carried.contains(t));
                }
                for t in s.tickets.iter_mut().filter(|t| carried.contains(&t.id)) {
                    t.sprint = None;
                }
            }
            EventKind::TicketTransitioned { id, from, to } => {
                let planned = self.workflow.planned_state().map(str::to_string);
                if let Some(t) = s.ticket_mut(id) {
                    t.state = to.clone();
                    if from == IN_QA && to != self.workflow.terminal_state() {
                        t.checks.remove("qa_failed");
                        t.checks.remove("qa_passed");
                    }
                    // Sent back for more work: the earlier tests and approval are stale,
                    // and so is any conflict being resolved for the merge.
                    if to == IN_PROGRESS && Some(from.as_str()) != planned.as_deref() {
                        t.checks.remove("tests_pass");
                        t.checks.remove("review_approved");
                        if from == IN_REVIEW {
                            t.conflict = None;
                        }
                    }
                }
            }
            EventKind::TicketCommented {
                id,
                body,
                external_id,
                attachments,
            } => {
                if let Some(t) = s.ticket_mut(id) {
                    t.comments.push(Comment {
                        author: actor_name,
                        body: body.clone(),
                        external_id: external_id.clone(),
                        attachments: attachments.clone(),
                    });
                }
            }
            EventKind::SprintProposed { sprint, gate } => {
                s.planning = None;
                s.bump_inbox(gate);
                for id in &sprint.tickets {
                    if let Some(t) = s.ticket_mut(id) {
                        t.sprint = Some(sprint.id.clone());
                    }
                }
                s.inbox.push(InboxItem::SprintGate {
                    id: gate.clone(),
                    sprint: sprint.id.clone(),
                });
                s.sprints.push(sprint.clone());
            }
            EventKind::SprintApproved { id } => {
                set_sprint_status(s, id, SprintStatus::Active);
            }
            EventKind::SprintRejected { id, .. } => {
                set_sprint_status(s, id, SprintStatus::Rejected);
                for t in s.tickets.iter_mut() {
                    if t.sprint.as_deref() == Some(id) {
                        t.sprint = None;
                    }
                }
            }
            EventKind::TrackerLinked {
                entity,
                id,
                external,
            } => {
                let slot = match entity {
                    ExternalEntity::Epic => s
                        .epics
                        .iter_mut()
                        .find(|e| e.id == *id)
                        .map(|e| &mut e.external),
                    ExternalEntity::Ticket => s.ticket_mut(id).map(|t| &mut t.external),
                    ExternalEntity::Sprint => s
                        .sprints
                        .iter_mut()
                        .find(|sp| sp.id == *id)
                        .map(|sp| &mut sp.external),
                };
                if let Some(slot) = slot {
                    *slot = Some(external.clone());
                }
            }
            EventKind::TicketBlockersSet { id, blocked_by } => {
                if let Some(t) = s.ticket_mut(id) {
                    t.blocked_by = blocked_by.clone();
                }
            }
            EventKind::TicketHandedOff { .. } => {}
            EventKind::WorkStarted {
                id,
                branch,
                worktree,
                ..
            } => {
                if let Some(t) = s.ticket_mut(id) {
                    t.branch = Some(branch.clone());
                    t.worktree = Some(worktree.clone());
                    t.reviewer = None;
                    t.needs_rebase = false;
                    t.review_requested = false;
                    t.checks.clear();
                }
            }
            EventKind::ReviewRequested { id, .. } => {
                if let Some(t) = s.ticket_mut(id) {
                    t.review_requested = true;
                }
            }
            EventKind::TestsRun {
                id,
                passed,
                skipped,
                output_tail,
            } => {
                if let Some(t) = s.ticket_mut(id) {
                    t.review_requested = false;
                    if *passed || *skipped {
                        t.checks.insert("tests_pass".into());
                        t.conflict = None;
                    } else {
                        t.checks.remove("tests_pass");
                    }
                }
                if *passed || *skipped {
                    s.test_streaks.remove(id);
                    triage::resolve_ticket(s, id, &[FailureClass::DevTests]);
                } else {
                    let streak = s.test_streaks.entry(id.clone()).or_default();
                    *streak += 1;
                    // The first failure goes back to the developer; the second is triaged.
                    if *streak >= 2 {
                        triage::record_failure(
                            s,
                            FailureClass::DevTests,
                            Some(id),
                            id,
                            output_tail,
                        );
                    }
                }
            }
            EventKind::PrOpened { id, number, url } => {
                if let Some(t) = s.ticket_mut(id) {
                    t.pr_number = Some(*number);
                    t.pr_url = Some(url.clone());
                }
            }
            EventKind::ReviewStarted { id, reviewer } => {
                if let Some(t) = s.ticket_mut(id) {
                    t.reviewer = Some(reviewer.clone());
                }
            }
            EventKind::ReviewSubmitted { id, verdict, .. } => {
                if let Some(t) = s.ticket_mut(id) {
                    match verdict {
                        ReviewVerdict::Approve => {
                            t.checks.insert("review_approved".into());
                        }
                        ReviewVerdict::RequestChanges => {
                            t.checks.remove("review_approved");
                            t.checks.remove("tests_pass");
                        }
                    }
                }
            }
            EventKind::BranchMerged { id, .. } => {
                triage::resolve_ticket(s, id, &[FailureClass::Merge, FailureClass::Rebase]);
                if let Some(t) = s.ticket_mut(id) {
                    t.checks.insert("merged_to_dev".into());
                    t.needs_rebase = false;
                    t.conflict = None;
                }
                // Everyone still building is now behind dev.
                for t in s.tickets.iter_mut() {
                    if t.id != *id && t.state == IN_PROGRESS && t.branch.is_some() {
                        t.needs_rebase = true;
                    }
                }
            }
            EventKind::MergeFailed { id, reason } => {
                if let Some(t) = s.ticket_mut(id) {
                    t.checks.remove("review_approved");
                    t.checks.remove("tests_pass");
                    t.conflict = None;
                }
                triage::record_failure(s, FailureClass::Merge, Some(id), id, reason);
            }
            EventKind::BranchRebased { id, outcome, .. } => {
                if let Some(t) = s.ticket_mut(id) {
                    t.needs_rebase = *outcome == RebaseOutcome::Deferred;
                }
                match outcome {
                    // The dispatcher follows with a conflict sync, which gives the assignee a
                    // resolve turn; triage waits until those run out.
                    RebaseOutcome::Conflict | RebaseOutcome::Deferred => {}
                    RebaseOutcome::Rebased => {
                        triage::resolve_ticket(s, id, &[FailureClass::Rebase]);
                    }
                }
            }
            EventKind::BranchSynced {
                id,
                outcome,
                files,
                note,
            } => {
                let mut exhausted = false;
                if let Some(t) = s.ticket_mut(id) {
                    match outcome {
                        SyncOutcome::Conflict | SyncOutcome::Unresolved => {
                            let attempts = t.conflict.as_ref().map_or(0, |c| c.attempts) + 1;
                            t.review_requested = false;
                            t.checks.remove("tests_pass");
                            if attempts > MAX_CONFLICT_ATTEMPTS && t.state == IN_PROGRESS {
                                // Out of resolve turns: the developer carries on, triaged.
                                t.conflict = None;
                                exhausted = true;
                            } else {
                                let files = if files.is_empty() {
                                    t.conflict
                                        .as_ref()
                                        .map(|c| c.files.clone())
                                        .unwrap_or_default()
                                } else {
                                    files.clone()
                                };
                                t.conflict = Some(MergeConflict {
                                    files,
                                    attempts,
                                    briefed: false,
                                });
                            }
                        }
                        SyncOutcome::Resolved => {
                            if t.state == IN_REVIEW {
                                t.review_requested = true;
                            } else {
                                t.conflict = None;
                            }
                        }
                        SyncOutcome::Merged | SyncOutcome::TestsFailed => t.conflict = None,
                    }
                }
                if exhausted {
                    triage::record_failure(s, FailureClass::Rebase, Some(id), id, note);
                }
            }
            EventKind::ConflictBriefed { id } => {
                if let Some(c) = s.ticket_mut(id).and_then(|t| t.conflict.as_mut()) {
                    c.briefed = true;
                }
            }
            EventKind::QaPromotionRequested => s.qa_requested = true,
            EventKind::QaRoundStarted { round } => {
                s.qa_requested = false;
                // A retest starts clean: last round's failure is not this round's.
                for id in &round.tickets {
                    if let Some(t) = s.ticket_mut(id) {
                        t.checks.remove("qa_failed");
                    }
                }
                s.qa = Some(round.clone());
            }
            EventKind::EnvCommandRun {
                step,
                ok,
                output_tail,
                alert,
                ..
            } => {
                if let Some(alert) = alert {
                    s.bump_inbox(alert);
                    s.inbox.push(InboxItem::EnvAlert {
                        id: alert.clone(),
                        env: "qa".into(),
                        step: *step,
                        output_tail: output_tail.clone(),
                    });
                    if matches!(step, EnvStep::Up | EnvStep::Test) {
                        triage::record_failure(
                            s,
                            FailureClass::QaEnv,
                            None,
                            &triage::env_subject(*step),
                            output_tail,
                        );
                    }
                } else if *ok && matches!(step, EnvStep::Up | EnvStep::Test) {
                    // The environment came up (or tested clean): nothing left to file.
                    triage::resolve(s, |f| f.class == FailureClass::QaEnv);
                }
                s.env_retry = None;
                if let Some(round) = s.qa.as_mut() {
                    if *step == EnvStep::Up && *ok {
                        round.up_done = true;
                    }
                    match (step, ok) {
                        (EnvStep::Up | EnvStep::Test, false) => {
                            round.status = QaRoundStatus::EnvFailed;
                        }
                        (EnvStep::Up, true) if round.status == QaRoundStatus::EnvFailed => {
                            round.status = QaRoundStatus::Starting;
                        }
                        (EnvStep::Test, true) => {
                            round.status = QaRoundStatus::Testing;
                            round.env_retries.clear();
                        }
                        _ => {}
                    }
                }
            }
            EventKind::QaVerdictRecorded {
                id, verdict, alert, ..
            } => {
                if let Some(round) = s.qa.as_mut() {
                    if *verdict == QaVerdict::Env {
                        *round.env_retries.entry(id.clone()).or_default() += 1;
                        if alert.is_some() {
                            round.status = QaRoundStatus::EnvFailed;
                        }
                    } else {
                        round.verdicts.insert(id.clone(), *verdict);
                    }
                }
                if let Some(alert) = alert {
                    s.bump_inbox(alert);
                    s.inbox.push(InboxItem::EnvAlert {
                        id: alert.clone(),
                        env: "qa".into(),
                        step: EnvStep::Test,
                        output_tail: format!("QA hit the environment twice testing {id}."),
                    });
                }
                if let Some(t) = s.ticket_mut(id) {
                    match verdict {
                        QaVerdict::Pass => {
                            t.checks.insert("qa_passed".into());
                            t.checks.remove("qa_failed");
                        }
                        QaVerdict::Fail => {
                            t.checks.insert("qa_failed".into());
                        }
                        QaVerdict::Env => {}
                    }
                }
            }
            EventKind::BugFiled { story, bug } => {
                let sprint = s.ticket(story).and_then(|t| t.sprint.clone());
                if let Some(sp) = sprint.and_then(|sp| s.sprints.iter_mut().find(|x| x.id == sp)) {
                    sp.tickets.push(bug.clone());
                }
            }
            EventKind::AlertRaised { id, title, detail } => {
                s.bump_inbox(id);
                s.inbox.push(InboxItem::Alert {
                    id: id.clone(),
                    title: title.clone(),
                    detail: detail.clone(),
                });
            }
            EventKind::AlertDismissed { id } => s.inbox.retain(|item| item.id() != id),
            EventKind::TriageBriefed { fingerprint } => {
                if let Some(f) = s
                    .failures
                    .iter_mut()
                    .find(|f| f.fingerprint == *fingerprint)
                {
                    f.attempts += 1;
                }
            }
            EventKind::TriageAbandoned { fingerprint } => {
                if let Some(f) = s
                    .failures
                    .iter_mut()
                    .find(|f| f.fingerprint == *fingerprint)
                {
                    f.status = TriageStatus::Abandoned;
                }
            }
            EventKind::TriageBugFiled {
                fingerprint,
                bug,
                sprint,
            } => {
                if let Some(f) = s
                    .failures
                    .iter_mut()
                    .find(|f| f.fingerprint == *fingerprint)
                {
                    f.status = TriageStatus::Filed;
                    f.bug = Some(bug.clone());
                }
                if let Some(sp) = sprint {
                    if let Some(t) = s.ticket_mut(bug) {
                        t.sprint = Some(sp.clone());
                    }
                    if let Some(sp) = s.sprints.iter_mut().find(|x| x.id == *sp) {
                        sp.tickets.push(bug.clone());
                    }
                }
            }
            EventKind::QaRoundClosed { .. } => {
                if let Some(round) = s.qa.as_mut() {
                    round.status = QaRoundStatus::Closed;
                }
            }
            EventKind::AppsConfigured { apps } => {
                let old = std::mem::take(&mut s.apps);
                s.apps = apps
                    .iter()
                    .map(|info| {
                        let mut app = old
                            .iter()
                            .find(|a| a.env == info.env)
                            .cloned()
                            .unwrap_or_else(|| crate::apps::RunningApp::new(info));
                        app.port = info.port;
                        app.url = info.url.clone();
                        app
                    })
                    .collect();
            }
            EventKind::AppReported {
                env,
                url,
                port,
                ticket,
                employee,
                worktree,
                note,
                down,
            } => {
                s.reported_apps
                    .retain(|a| !(a.employee == *employee && a.label == *env));
                if !*down {
                    s.reported_apps.push(crate::apps::ReportedApp {
                        label: env.clone(),
                        url: url.clone(),
                        port: *port,
                        ticket: ticket.clone(),
                        employee: employee.clone(),
                        worktree: worktree.clone(),
                        note: note.clone(),
                        since: event.ts,
                    });
                }
            }
            EventKind::AppRequested { env, want } => {
                if let Some(app) = s.app_mut(env) {
                    match want {
                        AppWant::Start | AppWant::Restart => {
                            app.wanted = true;
                            app.restart = true;
                            app.status = AppStatus::Starting;
                            app.reason = None;
                        }
                        AppWant::Stop => {
                            app.wanted = false;
                            app.restart = false;
                        }
                    }
                    app.since = Some(event.ts);
                }
            }
            EventKind::AppStarted {
                env,
                port,
                url,
                commit,
            } => {
                if let Some(app) = s.app_mut(env) {
                    app.status = AppStatus::Running;
                    app.port = *port;
                    app.url = url.clone();
                    app.commit = Some(commit.clone());
                    app.restart = false;
                    app.reason = None;
                    app.since = Some(event.ts);
                }
            }
            EventKind::AppStopped { env, reason } => {
                if let Some(app) = s.app_mut(env) {
                    app.status = AppStatus::Stopped;
                    app.restart = false;
                    app.reason = (!reason.is_empty()).then(|| reason.clone());
                    app.since = Some(event.ts);
                }
            }
            EventKind::AppCrashed {
                env,
                exit,
                will_retry,
                commit,
                ..
            } => {
                if let Some(app) = s.app_mut(env) {
                    app.status = if *will_retry {
                        AppStatus::Starting
                    } else {
                        AppStatus::Crashed
                    };
                    app.restart = false;
                    if !commit.is_empty() {
                        app.commit = Some(commit.clone());
                    }
                    app.reason = Some(match exit {
                        Some(code) => format!("exited with {code}"),
                        None => "could not run".to_string(),
                    });
                    app.since = Some(event.ts);
                }
            }
            EventKind::AppRestarted { env, .. } => {
                if let Some(app) = s.app_mut(env) {
                    app.status = AppStatus::Starting;
                    app.restarts += 1;
                    app.since = Some(event.ts);
                }
            }
            EventKind::EnvRetryRequested { env, step } => {
                // The alert it answers is done with; a new failure raises a new one.
                s.inbox.retain(|item| {
                    !matches!(item, InboxItem::EnvAlert { env: e, step: st, .. } if e == env && st == step)
                });
                s.env_retry = Some(EnvRetry {
                    env: env.clone(),
                    step: *step,
                });
            }
            EventKind::ReleaseProposed { release, gate } => {
                s.bump_inbox(gate);
                s.inbox.push(InboxItem::ReleaseGate {
                    id: gate.clone(),
                    release: release.id.clone(),
                });
                s.releases.push(release.clone());
            }
            EventKind::ReleaseApproved { id, version, notes } => {
                if let Some(r) = s.releases.iter_mut().find(|r| r.id == *id) {
                    r.version = version.clone();
                    r.notes = notes.clone();
                    r.status = ReleaseStatus::Approved;
                }
                s.inbox.retain(
                    |item| !matches!(item, InboxItem::ReleaseGate { release, .. } if release == id),
                );
            }
            EventKind::ReleaseRejected { id, .. } => {
                if let Some(r) = s.releases.iter_mut().find(|r| r.id == *id) {
                    r.status = ReleaseStatus::Rejected;
                }
                s.inbox.retain(
                    |item| !matches!(item, InboxItem::ReleaseGate { release, .. } if release == id),
                );
            }
            EventKind::ReleaseTagged { id, tag, commit } => {
                let Some(r) = s.releases.iter_mut().find(|r| r.id == *id) else {
                    return;
                };
                r.status = ReleaseStatus::Tagged;
                r.tag = Some(tag.clone());
                r.commit = Some(commit.clone());
                let shipped = r.tickets.clone();
                let sprint = r.sprint.clone();
                for t in s.tickets.iter_mut().filter(|t| shipped.contains(&t.id)) {
                    t.checks.insert("released".into());
                }
                // A sprint is released once all its work is: bugs QA found late keep
                // it open. Any active sprint whose work has all shipped (or was dropped)
                // is released, not only this release's own, so one left open by an
                // earlier release does not stay stuck.
                let done: Vec<SprintId> = s
                    .sprints
                    .iter()
                    .filter(|sp| sp.status == SprintStatus::Active)
                    .filter(|sp| {
                        let own = sprint.as_deref() == Some(sp.id.as_str());
                        let live: Vec<&Ticket> = s.sprint_tickets(sp).collect();
                        (own || !live.is_empty())
                            && live
                                .iter()
                                .all(|t| s.is_finished(t) && (own || t.checks.contains("released")))
                    })
                    .map(|sp| sp.id.clone())
                    .collect();
                for sp in done {
                    set_sprint_status(s, &sp, SprintStatus::Released);
                }
            }
            EventKind::DeployRun {
                id,
                ok,
                output_tail,
                alert,
                ..
            } => {
                s.env_retry = None;
                if let Some(alert) = alert {
                    s.bump_inbox(alert);
                    s.inbox.push(InboxItem::EnvAlert {
                        id: alert.clone(),
                        env: "prod".into(),
                        step: EnvStep::Deploy,
                        output_tail: output_tail.clone(),
                    });
                }
                if let Some(r) = s.releases.iter_mut().find(|r| r.id == *id) {
                    r.status = if *ok {
                        ReleaseStatus::Deployed
                    } else {
                        ReleaseStatus::DeployFailed
                    };
                }
            }
            EventKind::DispatchPaused => s.dispatch_paused = true,
            EventKind::DispatchResumed => s.dispatch_paused = false,
            EventKind::DispatchKicked
            | EventKind::ProblemResolved { .. }
            | EventKind::ActionRetryRequested { .. }
            | EventKind::UsageRefreshRequested
            | EventKind::UsageRefreshed => {}
            EventKind::BackendLimited {
                backend,
                until,
                message,
            } => {
                s.backend_limits.insert(
                    backend.clone(),
                    BackendLimit {
                        until: *until,
                        message: message.clone(),
                    },
                );
            }
            EventKind::BackendRecovered { backend } => {
                s.backend_limits.remove(backend);
            }
            EventKind::AgentPlanUpdated { employee, entries } => {
                s.live.entry(employee.clone()).or_default().plan = entries.clone();
            }
            EventKind::AgentToolCallContent {
                employee,
                tool_call_id,
                content,
            } => {
                if let Some(tool) = s.live_tool(employee, tool_call_id) {
                    tool.content = content.clone();
                }
                if let Some(live) = s.live.get_mut(employee) {
                    live.trim_content();
                }
            }
        }
    }

    fn unique_employee_id(&self, name: &str) -> EmployeeId {
        let base = slug(name);
        let base = if base.is_empty() {
            "employee".into()
        } else {
            base
        };
        let mut id = base.clone();
        let mut n = 2;
        while self.state.employee(&id).is_some() {
            id = format!("{base}-{n}");
            n += 1;
        }
        id
    }

    /// Why `ticket` cannot be moved to `to` by hand, when the dev loop owns that move.
    fn loop_owned(&self, ticket: &Ticket, to: &str) -> Option<&'static str> {
        if Some(ticket.state.as_str()) == self.workflow.planned_state() && to == IN_PROGRESS {
            Some("Chillax starts work itself once the ticket is assigned in an approved sprint")
        } else if ticket.state == IN_PROGRESS && to == IN_REVIEW {
            Some("call request_review; Chillax runs the tests and finds a reviewer")
        } else if ticket.state == IN_REVIEW && to == IN_QA {
            Some("tickets reach QA when their branch is merged into dev")
        } else if ticket.state == IN_QA && to == self.workflow.terminal_state() {
            Some("QA signs tickets off with submit_qa, after proving every criterion")
        } else {
            None
        }
    }

    /// True when `ticket` is blocked, directly or through other tickets, by `other`.
    fn depends_on(&self, ticket: &str, other: &str) -> bool {
        let mut seen = Vec::new();
        let mut stack = vec![ticket.to_string()];
        while let Some(id) = stack.pop() {
            let Some(t) = self.state.ticket(&id) else {
                continue;
            };
            for b in &t.blocked_by {
                if b == other {
                    return true;
                }
                if !seen.contains(b) {
                    seen.push(b.clone());
                    stack.push(b.clone());
                }
            }
        }
        false
    }

    /// The employee in `role` (id or tag) with the fewest open tickets, leaving out
    /// `exclude`. Ties go to whoever was hired first.
    fn require_open_request(&self, id: &str) -> Result<&Request, CommandError> {
        let request = self
            .state
            .request(id)
            .ok_or_else(|| CommandError::Invalid(format!("unknown request {id}")))?;
        if request.status.is_open() {
            Ok(request)
        } else {
            Err(CommandError::Invalid(format!(
                "request {id} is already {}",
                request.status.label()
            )))
        }
    }

    /// Link a ticket to a request and, for a `start_now` request, plan it: the request is
    /// the stakeholder's approval of the `sprint_start` gate.
    fn link_request_events(&self, request: &Request, ticket: &Ticket) -> Vec<EventKind> {
        use crate::intake::StartPlan;
        let s = &self.state;
        let plan = crate::intake::start_plan(self, request, ticket);
        let mut events = vec![EventKind::RequestTicketLinked {
            request: request.id.clone(),
            ticket: ticket.id.clone(),
            sprint: match &plan {
                StartPlan::Join(sp) => Some(sp.clone()),
                _ => None,
            },
        }];
        if plan == StartPlan::NewSprint {
            let id = format!("SP-{}", s.sprints.len() + 1);
            events.push(EventKind::SprintProposed {
                sprint: Sprint {
                    id: id.clone(),
                    name: format!("Request {}", request.id),
                    goal: request.text.lines().next().unwrap_or("").trim().to_string(),
                    tickets: vec![ticket.id.clone()],
                    status: SprintStatus::Proposed,
                    external: None,
                },
                gate: s.next_inbox_id(),
            });
            events.push(EventKind::SprintApproved { id });
        }
        if plan != StartPlan::Backlog
            && let Some(planned) = self.workflow.planned_state()
        {
            events.push(EventKind::TicketTransitioned {
                id: ticket.id.clone(),
                from: ticket.state.clone(),
                to: planned.to_string(),
            });
        }
        events
    }

    pub fn least_loaded(&self, role: &str, exclude: Option<&str>) -> Option<EmployeeId> {
        self.state
            .employees
            .iter()
            .filter(|e| Some(e.employee.id.as_str()) != exclude)
            .filter(|e| {
                e.employee.role == role
                    || self
                        .roles
                        .get(&e.employee.role)
                        .is_some_and(|r| r.tags.iter().any(|t| t == role))
            })
            .min_by_key(|e| {
                self.state
                    .assignments(&e.employee.id)
                    .iter()
                    .filter(|t| t.state != self.workflow.terminal_state())
                    .count()
            })
            .map(|e| e.employee.id.clone())
    }

    /// A workflow transition for the dev loop, checked against the ticket's recorded
    /// facts plus `also` (facts the same command is about to record).
    /// The events of a sync with `dev` or a conflict check. A conflict keeps
    /// the ticket where it is (an approval stands) and gives the assignee a resolve turn;
    /// past [`MAX_CONFLICT_ATTEMPTS`] it is triaged as before: a ticket in review fails
    /// its merge, one in progress records a rebase failure.
    fn decide_sync(
        &self,
        ticket: &Ticket,
        outcome: SyncOutcome,
        files: Vec<String>,
        note: String,
    ) -> Result<Vec<EventKind>, CommandError> {
        let id = ticket.id.clone();
        if ticket.state != IN_REVIEW && ticket.state != IN_PROGRESS {
            return Err(CommandError::Invalid(format!(
                "{id} is {}, not in progress or in review",
                ticket.state
            )));
        }
        let synced = EventKind::BranchSynced {
            id: id.clone(),
            outcome,
            files: files.clone(),
            note: note.clone(),
        };
        let mut events = vec![synced];
        match outcome {
            SyncOutcome::Conflict | SyncOutcome::Unresolved => {
                let attempt = ticket.conflict.as_ref().map_or(0, |c| c.attempts) + 1;
                let what = if files.is_empty() {
                    String::new()
                } else {
                    format!(" in {}", files.join(", "))
                };
                if attempt > MAX_CONFLICT_ATTEMPTS {
                    if ticket.state == IN_REVIEW {
                        let reason = if note.is_empty() {
                            format!("conflicts with dev{what}")
                        } else {
                            note
                        };
                        events.push(EventKind::MergeFailed {
                            id: id.clone(),
                            reason: reason.clone(),
                        });
                        events.push(self.dev_transition(ticket, IN_PROGRESS, &[])?);
                        events.push(comment(
                            &id,
                            &format!("Could not merge into dev.\n\n{reason}"),
                        ));
                    } else {
                        events.push(comment(
                            &id,
                            &format!(
                                "Still conflicts with dev{what} after \
                                 {MAX_CONFLICT_ATTEMPTS} attempts; the project manager is told."
                            ),
                        ));
                    }
                } else {
                    let who = ticket.assignee.as_deref().unwrap_or("the developer");
                    let lead = match outcome {
                        SyncOutcome::Unresolved => "Still conflicts",
                        _ => "Conflicts",
                    };
                    let stands = if ticket.state == IN_REVIEW {
                        "; the approval stands"
                    } else {
                        ""
                    };
                    events.push(comment(
                        &id,
                        &format!(
                            "{lead} with dev{what}; {who} is resolving them (attempt \
                             {attempt}/{MAX_CONFLICT_ATTEMPTS}){stands}."
                        ),
                    ));
                }
            }
            SyncOutcome::TestsFailed => {
                if ticket.state == IN_REVIEW {
                    events.push(self.dev_transition(ticket, IN_PROGRESS, &[])?);
                }
                events.push(EventKind::TestsRun {
                    id: id.clone(),
                    passed: false,
                    skipped: false,
                    output_tail: note.clone(),
                });
                events.push(comment(
                    &id,
                    &format!("Tests failed after merging dev into the branch.\n\n{note}"),
                ));
            }
            SyncOutcome::Merged => {
                events.push(comment(
                    &id,
                    "Merged dev into the branch; the tests still pass.",
                ));
            }
            SyncOutcome::Resolved => {
                if ticket.conflict.is_none() {
                    return Err(CommandError::Invalid(format!("{id} has no conflict")));
                }
            }
        }
        Ok(events)
    }

    fn dev_transition(
        &self,
        ticket: &Ticket,
        to: &str,
        also: &[&str],
    ) -> Result<EventKind, CommandError> {
        let transition = self.workflow.check_transition(&ticket.state, to)?;
        let mut checks = ticket.checks.clone();
        checks.extend(also.iter().map(|c| c.to_string()));
        let missing = transition.unmet(&checks);
        if !missing.is_empty() {
            return Err(CommandError::RequirementsUnmet {
                from: ticket.state.clone(),
                to: to.to_string(),
                missing,
            });
        }
        Ok(EventKind::TicketTransitioned {
            id: ticket.id.clone(),
            from: ticket.state.clone(),
            to: to.to_string(),
        })
    }

    /// True when `employee` is the only one whose role owns review, so their own work has
    /// no other reviewer to go to.
    pub fn sole_reviewer(&self, employee: &str) -> bool {
        let reviews = |e: &EmployeeView| {
            self.roles
                .get(&e.employee.role)
                .is_some_and(|r| self.workflow.role_owns(r, IN_REVIEW))
        };
        self.state.employee(employee).is_some_and(reviews)
            && !self
                .state
                .employees
                .iter()
                .any(|e| e.employee.id != employee && reviews(e))
    }

    fn require_failure(&self, fingerprint: &str) -> Result<&triage::Failure, CommandError> {
        self.state
            .failures
            .iter()
            .find(|f| f.fingerprint == fingerprint)
            .ok_or_else(|| CommandError::Invalid(format!("unknown failure {fingerprint}")))
    }

    /// The employee behind `actor` when their role owns the planner state (the workflow's
    /// first state, as for every project-manager duty); anyone else is refused.
    fn require_planner(
        &self,
        actor: &Actor,
        what: &'static str,
    ) -> Result<EmployeeId, CommandError> {
        let id = employee_only(actor, what)?;
        let planner = self.workflow.initial_state();
        let owns = self
            .require_employee(&id)
            .ok()
            .and_then(|e| self.roles.get(&e.employee.role))
            .is_some_and(|r| self.workflow.role_owns(r, planner));
        if owns {
            Ok(id)
        } else {
            Err(CommandError::Invalid(format!(
                "only the project manager can {what}"
            )))
        }
    }

    fn require_employee(&self, id: &str) -> Result<&EmployeeView, CommandError> {
        self.state
            .employee(id)
            .ok_or_else(|| CommandError::UnknownEmployee(id.to_string()))
    }

    /// Refuses a build assignment to `employee` (of `role_id`) when the role does not own
    /// `in_progress` and the ticket has not been built yet. Names the roles that can.
    fn check_can_build(
        &self,
        ticket: &str,
        role_id: &str,
        employee: &str,
    ) -> Result<(), CommandError> {
        let t = self.require_ticket(ticket)?;
        let unbuilt = t.state == self.workflow.initial_state()
            || Some(t.state.as_str()) == self.workflow.planned_state();
        let Some(role) = self.roles.get(role_id) else {
            return Ok(());
        };
        if !unbuilt || self.workflow.role_owns(role, IN_PROGRESS) {
            return Ok(());
        }
        let can: Vec<&str> = self
            .roles
            .iter()
            .filter(|r| self.workflow.role_owns(r, IN_PROGRESS))
            .map(|r| r.id.as_str())
            .collect();
        Err(CommandError::Invalid(format!(
            "{employee} ({role_id}) cannot build {ticket}: their role does not own {IN_PROGRESS}. Assign it to a {}",
            if can.is_empty() {
                "role that owns in_progress".to_string()
            } else {
                can.join(" or ")
            }
        )))
    }

    fn require_ticket(&self, id: &str) -> Result<&Ticket, CommandError> {
        self.state
            .ticket(id)
            .ok_or_else(|| CommandError::UnknownTicket(id.to_string()))
    }

    fn require_gated_release(&self, id: &str) -> Result<&Release, CommandError> {
        match self.state.release(id) {
            Some(r) if r.status == ReleaseStatus::Gated => Ok(r),
            Some(_) => Err(CommandError::Invalid(format!(
                "release {id} is not awaiting approval"
            ))),
            None => Err(CommandError::Invalid(format!("unknown release {id}"))),
        }
    }

    /// The release gate's candidate: what QA signed off since the last release.
    fn propose_release(&self, force: bool) -> Result<EventKind, CommandError> {
        let s = &self.state;
        if let Some(r) = s.open_release() {
            return Err(CommandError::Invalid(format!(
                "release {} is still in progress",
                r.id
            )));
        }
        let shipping = release::candidates(s);
        let failing = release::known_issues(s);
        // A forced gate can ship known issues alone (every ticket in the round failed);
        // otherwise there must be signed-off work.
        if shipping.is_empty() && (!force || failing.is_empty()) {
            return Err(CommandError::Invalid("nothing is ready to release".into()));
        }
        if !failing.is_empty() && !force {
            return Err(CommandError::Invalid(format!(
                "{} failed QA; force the gate to release with known issues",
                failing
                    .iter()
                    .map(|t| t.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        let round = s.qa.as_ref();
        let version = release::suggest_version(s, &shipping);
        Ok(EventKind::ReleaseProposed {
            release: Release {
                id: format!("R-{}", s.releases.len() + 1),
                notes: release::notes(s, &version, &shipping, &failing, round),
                version: version.to_string(),
                sprint: round.and_then(|r| r.sprint.clone()).or_else(|| {
                    shipping
                        .first()
                        .or(failing.first())
                        .and_then(|t| t.sprint.clone())
                }),
                round: round.map_or(0, |r| r.number),
                tickets: shipping.iter().map(|t| t.id.clone()).collect(),
                known_issues: failing.iter().map(|t| t.id.clone()).collect(),
                status: ReleaseStatus::Gated,
                tag: None,
                commit: None,
            },
            gate: s.next_inbox_id(),
        })
    }

    /// A new round needs the last one closed and no release in flight: a release ships
    /// what QA tested, so `qa` must not move until it is out.
    fn require_no_open_round(&self) -> Result<(), CommandError> {
        if let Some(r) = self.state.open_round() {
            return Err(CommandError::Invalid(format!(
                "QA round {} is still open",
                r.number
            )));
        }
        if let Some(r) = self.state.open_release() {
            return Err(CommandError::Invalid(format!(
                "release {} is still in progress; qa moves again once it is out",
                r.id
            )));
        }
        Ok(())
    }

    /// QA's verdict on a ticket of the testing round.
    fn qa_verdict(
        &self,
        actor: &Actor,
        id: TicketId,
        verdict: QaVerdict,
        criteria: Vec<CriterionResult>,
        notes: String,
        bug: Option<NewBug>,
    ) -> Result<Vec<EventKind>, CommandError> {
        let s = &self.state;
        let employee = employee_only(actor, "give QA verdicts")?;
        let owns_qa = self
            .require_employee(&employee)
            .ok()
            .and_then(|e| self.roles.get(&e.employee.role))
            .is_some_and(|role| self.workflow.role_owns(role, IN_QA));
        if !owns_qa {
            return Err(CommandError::Invalid(
                "only a role that owns QA gives QA verdicts".into(),
            ));
        }
        let ticket = self.require_ticket(&id)?;
        let round = match s.open_round() {
            Some(r) if r.status == QaRoundStatus::Testing => r,
            _ => return Err(CommandError::Invalid("no QA round is testing".into())),
        };
        if !round.tickets.contains(&id) || ticket.state != IN_QA {
            return Err(CommandError::Invalid(format!(
                "{id} is not under test in QA round {}",
                round.number
            )));
        }
        if round.verdicts.contains_key(&id) {
            return Err(CommandError::Invalid(format!(
                "{id} already has a verdict in QA round {}",
                round.number
            )));
        }
        let notes = notes.trim().to_string();
        let recorded = |alert| EventKind::QaVerdictRecorded {
            id: id.clone(),
            employee: employee.clone(),
            verdict,
            criteria: criteria.clone(),
            notes: notes.clone(),
            alert,
        };
        match verdict {
            QaVerdict::Pass => {
                // Every criterion proven with evidence: a bare "looks fine" is refused.
                if criteria.len() != ticket.acceptance_criteria.len() {
                    return Err(CommandError::Invalid(format!(
                        "{id} has {} acceptance criteria; report a result for each ({} given)",
                        ticket.acceptance_criteria.len(),
                        criteria.len()
                    )));
                }
                if let Some(bad) = criteria
                    .iter()
                    .find(|c| !c.passed || c.evidence.trim().is_empty())
                {
                    return Err(CommandError::Invalid(format!(
                        "cannot pass {id}: \"{}\" is not shown to pass with evidence",
                        bad.criterion
                    )));
                }
                let moved =
                    self.dev_transition(ticket, self.workflow.terminal_state(), &["qa_passed"])?;
                Ok(vec![
                    recorded(None),
                    moved,
                    comment(&id, &format!("QA passed. {notes}")),
                ])
            }
            QaVerdict::Fail => {
                let bug = bug.ok_or_else(|| {
                    CommandError::Invalid("a failing verdict must file a bug".into())
                })?;
                let failed: Vec<&CriterionResult> = criteria.iter().filter(|c| !c.passed).collect();
                if failed.is_empty() || failed.iter().any(|c| c.evidence.trim().is_empty()) {
                    return Err(CommandError::Invalid(
                        "a failing verdict needs the failed criteria, with evidence".into(),
                    ));
                }
                let bug_id = format!("T-{}", s.tickets.len() + 1);
                let sprint_active = ticket
                    .sprint
                    .as_deref()
                    .and_then(|sp| s.sprint(sp))
                    .is_some_and(|sp| sp.status == SprintStatus::Active);
                // The story's sprint was approved already, so its bugs skip the gate.
                let state = match self.workflow.planned_state() {
                    Some(planned) if sprint_active => planned,
                    _ => self.workflow.initial_state(),
                };
                let description = format!(
                    "{}\n\nFound in {id} during QA round {}.",
                    non_empty(bug.description, "bug description")?,
                    round.number
                );
                Ok(vec![
                    EventKind::TicketCreated {
                        ticket: Ticket {
                            id: bug_id.clone(),
                            kind: TicketKind::Bug,
                            title: non_empty(bug.title, "bug title")?,
                            description,
                            acceptance_criteria: failed
                                .iter()
                                .map(|c| c.criterion.clone())
                                .collect(),
                            estimate: None,
                            epic: ticket.epic.clone(),
                            state: state.to_string(),
                            assignee: ticket.assignee.clone(),
                            sprint: ticket.sprint.clone(),
                            comments: Vec::new(),
                            external: None,
                            blocked_by: Vec::new(),
                            branch: None,
                            worktree: None,
                            reviewer: None,
                            checks: Default::default(),
                            needs_rebase: false,
                            review_requested: false,
                            found_in: Some(id.clone()),
                            pr_number: None,
                            pr_url: None,
                            dropped: None,
                            urgent: false,
                            conflict: None,
                        },
                    },
                    EventKind::BugFiled {
                        story: id.clone(),
                        bug: bug_id.clone(),
                    },
                    recorded(None),
                    comment(&id, &format!("QA failed this. Bug {bug_id} filed. {notes}")),
                ])
            }
            QaVerdict::Env => {
                let retries = round.env_retries.get(&id).copied().unwrap_or(0) + 1;
                // The first environment problem is retried; the second goes to the
                // stakeholder.
                let alert = (retries >= 2).then(|| s.next_inbox_id());
                Ok(vec![
                    recorded(alert),
                    comment(&id, &format!("QA hit an environment problem. {notes}")),
                ])
            }
        }
    }

    fn require_proposed_sprint(&self, id: &str) -> Result<&Sprint, CommandError> {
        match self.state.sprint(id) {
            Some(sp) if sp.status == SprintStatus::Proposed => Ok(sp),
            Some(_) => Err(CommandError::Invalid(format!(
                "sprint {id} is not awaiting approval"
            ))),
            None => Err(CommandError::UnknownSprint(id.to_string())),
        }
    }
}

/// The ticket a command acts on, for commands that must not touch a dropped ticket.
fn live_ticket_of(cmd: &Command) -> Option<&str> {
    match cmd {
        Command::AssignTicket { id, .. }
        | Command::TransitionTicket { id, .. }
        | Command::SetBlockers { id, .. }
        | Command::Handoff { id, .. }
        | Command::StartWork { id, .. }
        | Command::RequestReview { id, .. }
        | Command::RecordTests { id, .. }
        | Command::RecordPr { id, .. }
        | Command::StartReview { id, .. }
        | Command::SubmitReview { id, .. }
        | Command::RecordMerge { id, .. }
        | Command::RecordMergeFailed { id, .. }
        | Command::RecordRebase { id, .. }
        | Command::RecordSync { id, .. }
        | Command::RecordConflictBriefed { id } => Some(id),
        _ => None,
    }
}

fn set_sprint_status(s: &mut State, id: &str, status: SprintStatus) {
    if let Some(sp) = s.sprints.iter_mut().find(|sp| sp.id == id) {
        sp.status = status;
    }
    s.inbox
        .retain(|i| !matches!(i, InboxItem::SprintGate { sprint, .. } if sprint == id));
}

fn tool_text(title: &str, status: &str) -> String {
    if status.is_empty() {
        title.to_string()
    } else {
        format!("{title} [{status}]")
    }
}

fn split_tool_text(text: &str) -> (&str, &str) {
    match text.strip_suffix(']').and_then(|t| t.rsplit_once(" [")) {
        Some((title, status)) => (title, status),
        None => (text, ""),
    }
}

fn system_only(actor: &Actor, what: &'static str) -> Result<(), CommandError> {
    match actor {
        Actor::System => Ok(()),
        _ => Err(CommandError::Invalid(format!("only Chillax can {what}"))),
    }
}

/// A ticket comment the dev loop writes, so the history reaches the tracker mirror.
fn comment(id: &str, body: &str) -> EventKind {
    EventKind::TicketCommented {
        id: id.to_string(),
        body: body.trim().to_string(),
        external_id: None,
        attachments: Vec::new(),
    }
}

fn not_stakeholder(actor: &Actor, what: &'static str) -> Result<(), CommandError> {
    if *actor == Actor::Stakeholder {
        Err(CommandError::Invalid(format!(
            "only the project manager or Chillax can {what}"
        )))
    } else {
        Ok(())
    }
}

fn stakeholder_only(actor: &Actor, what: &'static str) -> Result<(), CommandError> {
    match actor {
        Actor::Stakeholder => Ok(()),
        _ => Err(CommandError::StakeholderOnly(what)),
    }
}

fn employee_only(actor: &Actor, what: &'static str) -> Result<EmployeeId, CommandError> {
    actor
        .employee()
        .map(str::to_string)
        .ok_or(CommandError::EmployeeOnly(what))
}

fn non_empty(value: String, field: &str) -> Result<String, CommandError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        Err(CommandError::Invalid(format!("{field} must not be empty")))
    } else {
        Ok(trimmed.to_string())
    }
}

fn non_blank(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// The backend and model a hire into `role` gets. The stakeholder's pick wins, then
/// their defaults, then the role's own. A model is kept only on the backend it was
/// chosen for, so a Claude model never lands on another agent. Likewise an effort from
/// the defaults is kept only for the backend and model it was set with.
pub fn hire_choice(
    defaults: &HiringDefaults,
    role: &Role,
    backend: Option<String>,
    model: Option<String>,
    effort: Option<String>,
) -> (String, Option<String>, Option<String>) {
    let role_backend = role.default_backend.as_deref().unwrap_or("claude");
    let backend = backend
        .or_else(|| defaults.backend.clone())
        .unwrap_or_else(|| role_backend.to_string());
    let model = model.or_else(|| {
        let from_defaults = defaults
            .model
            .clone()
            .filter(|_| defaults.backend.as_deref() == Some(backend.as_str()));
        from_defaults.or_else(|| role.model.clone().filter(|_| backend == role_backend))
    });
    let effort = effort.or_else(|| {
        defaults.effort.clone().filter(|_| {
            defaults.backend.as_deref() == Some(backend.as_str()) && defaults.model == model
        })
    });
    (backend, model, effort)
}

/// The agent an existing hire switches to. A blank backend keeps the current one; model
/// and effort are replaced outright (blank = the backend's default), and an effort is
/// kept only alongside a model.
pub fn employee_agent_choice(
    current: &Employee,
    backend: Option<String>,
    model: Option<String>,
    effort: Option<String>,
) -> (String, Option<String>, Option<String>) {
    let backend = backend
        .and_then(|b| non_blank(&b))
        .unwrap_or_else(|| current.backend.clone());
    let model = model.and_then(|m| non_blank(&m));
    let effort = effort
        .and_then(|e| non_blank(&e))
        .filter(|_| model.is_some());
    (backend, model, effort)
}

/// `Alice Smith` → `alice-smith`.
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    #[test]
    fn stale_agents_lists_employees_that_look_alive() {
        let mut engine = Engine::new("Acme", RoleCatalog::default(), Workflow::default());
        for (id, status, turn_open) in [
            ("idle", EmployeeStatus::Idle, false),
            ("working", EmployeeStatus::Working, true),
            ("failed", EmployeeStatus::Failed, false),
            ("open", EmployeeStatus::Offline, true),
            ("off", EmployeeStatus::Offline, false),
        ] {
            engine.state.employees.push(EmployeeView {
                employee: Employee {
                    id: id.into(),
                    name: id.into(),
                    role: "pm".into(),
                    role_title: "PM".into(),
                    backend: "fake".into(),
                    model: None,
                    effort: None,
                },
                status: EmployeeStatus::Offline,
                last_progress: None,
                turn_open: false,
                stopped: false,
                reply_pending: false,
                pending_agent: None,
            });
            let e = engine.state.employee_mut(id).expect("hired");
            e.status = status;
            e.turn_open = turn_open;
        }
        assert_eq!(engine.stale_agents(), ["idle", "working", "failed", "open"]);
    }

    #[test]
    fn stakeholder_moves_skip_chillax_owned_transitions() {
        let mut engine = Engine::new("Acme", RoleCatalog::default(), Workflow::default());
        engine.state.sprints = vec![Sprint {
            id: "SP-1".into(),
            name: "Sprint 1".into(),
            goal: String::new(),
            tickets: vec![],
            status: SprintStatus::Active,
            external: None,
        }];
        let moves = |from: &str| {
            let t = ticket(from, Some("SP-1"));
            engine
                .state
                .stakeholder_moves(&t)
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        };
        assert_eq!(moves("backlog"), ["ready"]);
        assert_eq!(moves("ready"), ["backlog"]);
        assert!(moves("in_progress").is_empty());
        assert_eq!(moves("in_review"), ["in_progress"]);
        assert_eq!(moves("in_qa"), ["in_progress"]);
        assert!(moves("done").is_empty());
    }

    #[test]
    fn stakeholder_moves_omit_ready_when_the_sprint_gate_would_reject() {
        let mut engine = Engine::new("Acme", RoleCatalog::default(), Workflow::default());
        engine.state.sprints = vec![Sprint {
            id: "SP-1".into(),
            name: "Sprint 1".into(),
            goal: String::new(),
            tickets: vec![],
            status: SprintStatus::Proposed,
            external: None,
        }];
        for sprint in [None, Some("SP-1")] {
            let t = ticket("backlog", sprint);
            assert!(engine.state.stakeholder_moves(&t).is_empty(), "{sprint:?}");
            assert!(!engine.state.sprint_gate_allows(&t, "ready"));
        }
    }

    fn sprint_with(id: &str, status: SprintStatus) -> Sprint {
        Sprint {
            id: id.into(),
            name: format!("Sprint {id}"),
            goal: String::new(),
            tickets: vec![],
            status,
            external: None,
        }
    }

    fn board_ids(state: &State, view: &SprintView) -> Vec<String> {
        state
            .board_tickets(view)
            .iter()
            .map(|t| t.id.clone())
            .collect()
    }

    /// Tickets T-1..: done in SP-1 (released), T-2 in SP-2 (closed), T-3 unsprinted,
    /// T-4 dropped unsprinted, T-5 in SP-3 (active).
    fn sprint_board_state() -> Engine {
        let mut engine = Engine::new("Acme", RoleCatalog::default(), Workflow::default());
        engine.state.sprints = vec![
            sprint_with("SP-1", SprintStatus::Released),
            sprint_with("SP-2", SprintStatus::Closed),
            sprint_with("SP-3", SprintStatus::Active),
        ];
        for (id, sprint) in [
            ("T-1", Some("SP-1")),
            ("T-2", Some("SP-2")),
            ("T-3", None),
            ("T-4", None),
            ("T-5", Some("SP-3")),
        ] {
            let mut t = ticket("done", sprint);
            t.id = id.into();
            if id == "T-4" {
                t.dropped = Some("no".into());
            }
            engine.state.tickets.push(t);
        }
        engine
    }

    #[test]
    fn board_active_shows_the_sprint_in_flight() {
        let e = sprint_board_state();
        assert_eq!(board_ids(&e.state, &SprintView::Active), ["T-5"]);
    }

    #[test]
    fn board_active_is_empty_between_sprints() {
        let mut e = sprint_board_state();
        e.state.sprints[2].status = SprintStatus::Released;
        assert!(board_ids(&e.state, &SprintView::Active).is_empty());
    }

    #[test]
    fn board_released_sprint_lists_its_tickets() {
        let e = sprint_board_state();
        assert_eq!(
            board_ids(&e.state, &SprintView::Sprint("SP-1".into())),
            ["T-1"]
        );
    }

    #[test]
    fn board_closed_sprint_lists_finished_tickets_and_carried_ones_are_backlog() {
        let mut e = sprint_board_state();
        // A carried ticket left SP-2: its sprint is cleared.
        let mut carried = ticket("backlog", None);
        carried.id = "T-6".into();
        e.state.tickets.push(carried);
        assert_eq!(
            board_ids(&e.state, &SprintView::Sprint("SP-2".into())),
            ["T-2"]
        );
        assert!(board_ids(&e.state, &SprintView::Backlog).contains(&"T-6".to_string()));
    }

    #[test]
    fn board_backlog_excludes_dropped() {
        let e = sprint_board_state();
        assert_eq!(board_ids(&e.state, &SprintView::Backlog), ["T-3"]);
    }

    #[test]
    fn board_leftover_moved_to_a_new_sprint_appears_only_there() {
        let mut e = sprint_board_state();
        // SP-2 still lists T-3 (stale), but T-3 now belongs to SP-3.
        e.state.sprints[1].tickets = vec!["T-2".into(), "T-3".into()];
        e.state.tickets[2].sprint = Some("SP-3".into());
        assert_eq!(board_ids(&e.state, &SprintView::Active), ["T-3", "T-5"]);
        assert_eq!(
            board_ids(&e.state, &SprintView::Sprint("SP-2".into())),
            ["T-2"]
        );
    }

    #[test]
    fn past_sprints_omit_rejected_and_in_flight() {
        let mut e = sprint_board_state();
        e.state
            .sprints
            .push(sprint_with("SP-4", SprintStatus::Rejected));
        let ids: Vec<_> = e
            .state
            .past_sprints()
            .iter()
            .map(|s| s.id.clone())
            .collect();
        assert_eq!(ids, ["SP-1", "SP-2"]);
    }

    #[test]
    fn sprint_views_cycle_newest_past_first_then_backlog() {
        let s = sprint_board_state().state;
        let mut v = SprintView::Active;
        let mut seen = vec![];
        for _ in 0..4 {
            v = s.next_sprint_view(&v);
            seen.push(v.clone());
        }
        assert_eq!(
            seen,
            [
                SprintView::Sprint("SP-2".into()),
                SprintView::Sprint("SP-1".into()),
                SprintView::Backlog,
                SprintView::Active
            ]
        );
        assert_eq!(s.prev_sprint_view(&SprintView::Active), SprintView::Backlog);
        assert!(!s.sprint_view_exists(&SprintView::Sprint("SP-9".into())));
        assert!(SprintView::Backlog.is_read_only() && !SprintView::Active.is_read_only());
    }

    fn ticket(state: &str, sprint: Option<&str>) -> Ticket {
        Ticket {
            id: "T-1".into(),
            kind: TicketKind::Story,
            title: "Login".into(),
            description: String::new(),
            acceptance_criteria: vec![],
            estimate: None,
            epic: None,
            state: state.into(),
            assignee: None,
            sprint: sprint.map(String::from),
            comments: vec![],
            external: None,
            blocked_by: vec![],
            branch: None,
            worktree: None,
            reviewer: None,
            checks: Default::default(),
            needs_rebase: false,
            review_requested: false,
            found_in: None,
            pr_number: None,
            pr_url: None,
            dropped: None,
            urgent: false,
            conflict: None,
        }
    }

    use super::*;
    use crate::role::Role;

    fn engine() -> Engine {
        let pm = Role::parse("---\nid: project-manager\ntitle: Project Manager\n---\nPM\n")
            .expect("valid role");
        Engine::new("Acme", RoleCatalog::new([pm]), Workflow::default())
    }

    /// Decide and apply, like the hub does.
    fn run(engine: &mut Engine, actor: Actor, cmd: Command) -> Vec<EventKind> {
        let kinds = engine.decide(&actor, cmd).expect("command accepted");
        for kind in &kinds {
            record(engine, actor.clone(), kind.clone());
        }
        kinds
    }

    fn record(engine: &mut Engine, actor: Actor, kind: EventKind) {
        let event = Event {
            seq: engine.state.last_seq + 1,
            ts: chrono::DateTime::from_timestamp(0, 0).expect("epoch is valid"),
            actor,
            kind,
        };
        engine.apply(&event);
    }

    fn hire(engine: &mut Engine, name: &str) {
        run(
            engine,
            Actor::Stakeholder,
            Command::HireEmployee {
                role: "project-manager".into(),
                name: Some(name.into()),
                backend: None,
                model: None,
                effort: None,
                keep_defaults: false,
            },
        );
    }

    fn story(engine: &mut Engine, title: &str) {
        run(
            engine,
            Actor::Employee("pat".into()),
            Command::CreateTicket {
                kind: TicketKind::Story,
                title: title.into(),
                description: String::new(),
                acceptance_criteria: vec!["works".into()],
                estimate: Some(3),
                epic: None,
                external: None,
            },
        );
    }

    fn ext_ref(id: &str, key: &str) -> ExternalRef {
        ExternalRef {
            id: id.into(),
            key: Some(key.into()),
            url: None,
        }
    }

    #[test]
    fn clear_activity_is_stakeholder_only_and_records_the_seq() {
        let mut engine = engine();
        assert!(matches!(
            engine.decide(&Actor::Employee("pat".into()), Command::ClearActivity),
            Err(CommandError::StakeholderOnly(_))
        ));
        hire(&mut engine, "Pat");
        let kinds = run(&mut engine, Actor::Stakeholder, Command::ClearActivity);
        assert_eq!(kinds, vec![EventKind::ActivityCleared]);
        assert_eq!(engine.state.activity_cleared_seq, engine.state.last_seq);

        // Events after the clear are not hidden, and clearing again moves the mark.
        let mark = engine.state.activity_cleared_seq;
        story(&mut engine, "After");
        assert!(engine.state.last_seq > engine.state.activity_cleared_seq);
        assert_eq!(engine.state.activity_cleared_seq, mark);
        run(&mut engine, Actor::Stakeholder, Command::ClearActivity);
        assert_eq!(engine.state.activity_cleared_seq, engine.state.last_seq);
    }

    #[test]
    fn company_chat_records_people_but_not_system() {
        let mut e = engine();
        hire(&mut e, "Pat Lee");
        hire(&mut e, "Sam");
        let say = |to: &str, text: &str| Command::SendChat {
            to: to.into(),
            text: text.into(),
        };
        run(
            &mut e,
            Actor::Employee("pat-lee".into()),
            say("sam", "ping"),
        );
        run(&mut e, Actor::Stakeholder, say("sam", "hello"));
        run(&mut e, Actor::System, say("sam", "briefing"));
        let feed = &e.state.company_chat;
        assert_eq!(feed.len(), 2);
        assert_eq!(feed[0].from.as_deref(), Some("pat-lee"));
        assert_eq!(feed[0].from_name, "Pat Lee");
        assert_eq!(feed[0].to.as_deref(), Some("sam"));
        assert_eq!(feed[1].from, None);
        assert_eq!(feed[1].from_name, "Stakeholder");
    }

    #[test]
    fn company_chat_is_capped_and_survives_clears() {
        let mut e = engine();
        hire(&mut e, "Pat");
        for i in 0..COMPANY_CHAT_KEEP + 3 {
            run(
                &mut e,
                Actor::Stakeholder,
                Command::PostCompanyChat {
                    text: format!("m{i}"),
                    to: None,
                },
            );
        }
        assert_eq!(e.state.company_chat.len(), COMPANY_CHAT_KEEP);
        assert_eq!(e.state.company_chat[0].text, "m3");
        run(
            &mut e,
            Actor::Stakeholder,
            Command::ClearChat {
                employee: "pat".into(),
            },
        );
        assert_eq!(e.state.company_chat.len(), COMPANY_CHAT_KEEP);
    }

    #[test]
    fn post_company_chat_to_all_wakes_only_mentions() {
        let mut e = engine();
        hire(&mut e, "Pat Lee");
        hire(&mut e, "Sam");
        let kinds = run(
            &mut e,
            Actor::Stakeholder,
            Command::PostCompanyChat {
                text: "standup at 10, @pat and @ALL, @pat again, email@sam.dev".into(),
                to: None,
            },
        );
        assert!(matches!(
            &kinds[..],
            [EventKind::CompanyChatPosted { mentions, .. }] if mentions == &["pat-lee".to_string()]
        ));
        assert!(e.state.employee("pat-lee").unwrap().reply_pending);
        assert!(!e.state.employee("sam").unwrap().reply_pending);
        assert!(
            e.state.chats["pat-lee"]
                .last()
                .unwrap()
                .text
                .starts_with("(company chat) standup")
        );
        assert!(
            e.state.chats["sam"]
                .iter()
                .all(|l| !l.text.contains("standup"))
        );
        assert_eq!(e.state.company_chat[0].to, None);
    }

    #[test]
    fn mentions_resolve_by_id_or_first_name() {
        let mut e = engine();
        hire(&mut e, "Pat Lee");
        hire(&mut e, "Sam");
        let m = |t| resolve_mentions(&e.state, t);
        assert_eq!(m("@sam, @Pat-Lee."), vec!["sam", "pat-lee"]);
        assert_eq!(m("@pat"), vec!["pat-lee"]);
        assert!(m("@all @nobody a@sam").is_empty());
    }

    #[test]
    fn post_company_chat_with_target_is_a_normal_chat_message() {
        let mut e = engine();
        hire(&mut e, "Pat");
        let kinds = run(
            &mut e,
            Actor::Stakeholder,
            Command::PostCompanyChat {
                text: "hi".into(),
                to: Some("pat".into()),
            },
        );
        assert_eq!(
            kinds,
            vec![EventKind::ChatMessageSent {
                to: "pat".into(),
                text: "hi".into()
            }]
        );
        assert_eq!(e.state.company_chat.len(), 1);
    }

    #[test]
    fn post_company_chat_is_stakeholder_only_and_needs_text() {
        let mut e = engine();
        hire(&mut e, "Pat");
        let post = |text: &str, to: Option<&str>| Command::PostCompanyChat {
            text: text.into(),
            to: to.map(Into::into),
        };
        assert!(matches!(
            e.decide(&Actor::Employee("pat".into()), post("x", None)),
            Err(CommandError::StakeholderOnly(_))
        ));
        assert!(e.decide(&Actor::Stakeholder, post("  ", None)).is_err());
        assert!(
            e.decide(&Actor::Stakeholder, post("x", Some("ghost")))
                .is_err()
        );
    }

    #[test]
    fn company_chat_replay_is_deterministic() {
        let mut a = engine();
        let mut kinds = run(
            &mut a,
            Actor::Stakeholder,
            Command::HireEmployee {
                role: "project-manager".into(),
                name: Some("Pat".into()),
                backend: None,
                model: None,
                effort: None,
                keep_defaults: false,
            },
        );
        kinds.extend(run(
            &mut a,
            Actor::Stakeholder,
            Command::PostCompanyChat {
                text: "@pat hi".into(),
                to: None,
            },
        ));
        let mut b = engine();
        for kind in kinds {
            record(&mut b, Actor::Stakeholder, kind);
        }
        assert_eq!(a.state.company_chat, b.state.company_chat);
        assert_eq!(a.state.chats, b.state.chats);
    }

    #[test]
    fn clear_chat_empties_log_and_is_stakeholder_only() {
        let mut e = engine();
        hire(&mut e, "Pat");
        run(
            &mut e,
            Actor::Stakeholder,
            Command::SendChat {
                to: "pat".into(),
                text: "hi".into(),
            },
        );
        assert!(!e.state.chats["pat"].is_empty());
        let clear = || Command::ClearChat {
            employee: "pat".into(),
        };
        assert!(matches!(
            e.decide(&Actor::Employee("pat".into()), clear()),
            Err(CommandError::StakeholderOnly(_))
        ));
        let kinds = run(&mut e, Actor::Stakeholder, clear());
        assert_eq!(
            kinds,
            vec![EventKind::ChatCleared {
                employee: "pat".into()
            }]
        );
        assert!(e.state.chats.get("pat").is_none_or(|c| c.is_empty()));
        assert!(
            e.decide(
                &Actor::Stakeholder,
                Command::ClearChat {
                    employee: "nobody".into()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn employee_context_cleared_empties_transcript_keeps_plan() {
        let mut e = engine();
        hire(&mut e, "Pat");
        record(
            &mut e,
            Actor::Employee("pat".into()),
            EventKind::AgentToolCall {
                employee: "pat".into(),
                tool_call_id: "c1".into(),
                title: "Read file".into(),
                tool_kind: "read".into(),
                status: "completed".into(),
            },
        );
        record(
            &mut e,
            Actor::Employee("pat".into()),
            EventKind::AgentPlanUpdated {
                employee: "pat".into(),
                entries: vec![PlanEntry {
                    content: "step".into(),
                    status: "pending".into(),
                }],
            },
        );
        assert!(!e.state.transcript("pat").is_empty());
        let cmd = || Command::ClearEmployeeContext {
            employee: "pat".into(),
            ticket: "T-1".into(),
        };
        assert!(e.decide(&Actor::Stakeholder, cmd()).is_err());
        assert!(e.decide(&Actor::Employee("pat".into()), cmd()).is_err());
        let kinds = run(&mut e, Actor::System, cmd());
        assert_eq!(kinds.len(), 1);
        assert_eq!(kinds[0].name(), "employee_context_cleared");
        assert!(e.state.transcript("pat").is_empty());
        assert_eq!(e.state.live["pat"].plan.len(), 1);
    }

    #[test]
    fn clear_chat_empties_transcript_with_tool_calls() {
        let mut e = engine();
        hire(&mut e, "Pat");
        record(
            &mut e,
            Actor::Employee("pat".into()),
            EventKind::AgentToolCall {
                employee: "pat".into(),
                tool_call_id: "c1".into(),
                title: "Read file".into(),
                tool_kind: "read".into(),
                status: "completed".into(),
            },
        );
        record(
            &mut e,
            Actor::Employee("pat".into()),
            EventKind::AgentTurnEnded {
                employee: "pat".into(),
                stop_reason: "end_turn".into(),
                ticket: None,
            },
        );
        assert!(!e.state.transcript("pat").is_empty());
        run(
            &mut e,
            Actor::Stakeholder,
            Command::ClearChat {
                employee: "pat".into(),
            },
        );
        assert!(e.state.transcript("pat").is_empty());
    }

    fn change_agent(
        e: &mut Engine,
        actor: Actor,
        backend: Option<&str>,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Result<Vec<EventKind>, CommandError> {
        e.decide(
            &actor,
            Command::SetEmployeeAgent {
                when_idle: false,
                employee: "pat".into(),
                backend: backend.map(String::from),
                model: model.map(String::from),
                effort: effort.map(String::from),
            },
        )
    }

    #[test]
    fn set_employee_agent_rules() {
        let mut e = engine();
        hire_with(&mut e, Some("claude"), Some("sonnet"));
        let defaults = e.state.hiring.clone();
        assert!(matches!(
            change_agent(&mut e, Actor::Employee("pat".into()), None, None, None),
            Err(CommandError::StakeholderOnly(_))
        ));
        assert!(
            e.decide(
                &Actor::Stakeholder,
                Command::SetEmployeeAgent {
                    when_idle: false,
                    employee: "nobody".into(),
                    backend: None,
                    model: None,
                    effort: None
                }
            )
            .is_err()
        );
        // Unchanged is a no-op.
        assert!(
            change_agent(&mut e, Actor::Stakeholder, None, Some("sonnet"), None)
                .expect("ok")
                .is_empty()
        );
        // Switching the backend replaces model and effort.
        let kinds = run(
            &mut e,
            Actor::Stakeholder,
            Command::SetEmployeeAgent {
                when_idle: false,
                employee: "pat".into(),
                backend: Some("codex".into()),
                model: Some("gpt-5".into()),
                effort: Some("high".into()),
            },
        );
        assert_eq!(kinds.len(), 1);
        let pat = &e.state.employees[0].employee;
        assert_eq!(
            (
                pat.backend.as_str(),
                pat.model.as_deref(),
                pat.effort.as_deref()
            ),
            ("codex", Some("gpt-5"), Some("high"))
        );
        assert!(
            e.state.chats["pat"]
                .iter()
                .any(|l| l.text.starts_with("Now running on codex · gpt-5 · high"))
        );
        // Clearing model drops effort too; blank backend keeps codex.
        run(
            &mut e,
            Actor::Stakeholder,
            Command::SetEmployeeAgent {
                when_idle: false,
                employee: "pat".into(),
                backend: Some(" ".into()),
                model: None,
                effort: Some("high".into()),
            },
        );
        let pat = &e.state.employees[0].employee;
        assert_eq!(
            (
                pat.backend.as_str(),
                pat.model.as_deref(),
                pat.effort.as_deref()
            ),
            ("codex", None, None)
        );
        assert_eq!(e.state.hiring, defaults, "hiring defaults are untouched");
    }

    #[test]
    fn set_employee_agent_refused_while_replying() {
        let mut e = engine();
        hire(&mut e, "Pat");
        record(
            &mut e,
            Actor::System,
            EventKind::AgentMessageChunk {
                employee: "pat".into(),
                text: "thinking".into(),
            },
        );
        let err = change_agent(&mut e, Actor::Stakeholder, Some("codex"), None, None)
            .expect_err("refused");
        assert!(err.to_string().contains("is replying"));
    }

    #[test]
    fn agent_changed_event_round_trips_and_reads_old_json() {
        let kind = EventKind::EmployeeAgentChanged {
            employee: "pat".into(),
            backend: "codex".into(),
            model: None,
            effort: None,
        };
        let json = serde_json::to_string(&kind).expect("serialises");
        assert!(!json.contains("model"));
        assert_eq!(
            serde_json::from_str::<EventKind>(&json).expect("reads"),
            kind
        );
        assert_eq!(kind.summary(), "pat now runs on codex");
        let cmd: Command =
            serde_json::from_str(r#"{"type":"set_employee_agent","employee":"pat"}"#)
                .expect("defaults");
        assert!(matches!(
            cmd,
            Command::SetEmployeeAgent { backend: None, .. }
        ));
    }

    #[test]
    fn cancel_turn_is_stakeholder_only_and_a_noop_when_idle() {
        let mut e = engine();
        hire(&mut e, "Pat");
        let cancel = || Command::CancelTurn {
            employee: "pat".into(),
        };
        assert!(
            e.decide(&Actor::Stakeholder, cancel())
                .expect("accepted")
                .is_empty()
        );
        assert!(matches!(
            e.decide(&Actor::Employee("pat".into()), cancel()),
            Err(CommandError::StakeholderOnly(_))
        ));
        assert!(
            e.decide(
                &Actor::Stakeholder,
                Command::CancelTurn {
                    employee: "nobody".into()
                }
            )
            .is_err()
        );
    }

    fn prioritize(id: &str, urgent: bool) -> Command {
        Command::PrioritizeTicket {
            id: id.into(),
            urgent,
        }
    }

    #[test]
    fn prioritize_is_stakeholder_only_idempotent_and_refuses_settled_tickets() {
        let mut e = engine();
        hire(&mut e, "Pat");
        story(&mut e, "Login");
        assert!(matches!(
            e.decide(&Actor::Employee("pat".into()), prioritize("T-1", true)),
            Err(CommandError::StakeholderOnly(_))
        ));
        assert!(matches!(
            e.decide(&Actor::Stakeholder, prioritize("T-9", true)),
            Err(CommandError::UnknownTicket(_))
        ));
        // Clearing a flag that is not set changes nothing.
        assert!(
            e.decide(&Actor::Stakeholder, prioritize("T-1", false))
                .expect("accepted")
                .is_empty()
        );
        run(&mut e, Actor::Stakeholder, prioritize("T-1", true));
        assert!(
            e.decide(&Actor::Stakeholder, prioritize("T-1", true))
                .expect("accepted")
                .is_empty()
        );
        run(
            &mut e,
            Actor::Stakeholder,
            Command::DropTicket {
                id: "T-1".into(),
                reason: "no".into(),
            },
        );
        assert!(matches!(
            e.decide(&Actor::Stakeholder, prioritize("T-1", false)),
            Err(CommandError::Invalid(_))
        ));
        story(&mut e, "Other");
        let terminal = e.state.terminal_state().to_string();
        e.state.ticket_mut("T-2").expect("exists").state = terminal;
        assert!(matches!(
            e.decide(&Actor::Stakeholder, prioritize("T-2", true)),
            Err(CommandError::Invalid(_))
        ));
    }

    #[test]
    fn prioritize_sets_the_flag_alerts_only_when_turning_on_and_tells_the_pm() {
        let mut e = engine();
        hire(&mut e, "Pat");
        story(&mut e, "Login");
        let on = run(&mut e, Actor::Stakeholder, prioritize("T-1", true));
        assert_eq!(on.len(), 2);
        assert_eq!(
            on[0],
            EventKind::TicketPrioritized {
                id: "T-1".into(),
                urgent: true
            }
        );
        assert!(
            matches!(&on[1], EventKind::AlertRaised { title, detail, .. }
                if title == "T-1 is urgent: Login" && detail.contains("hire a developer"))
        );
        assert!(e.state.ticket("T-1").is_some_and(|t| t.urgent));
        assert!(
            e.state
                .chats
                .get("pat")
                .is_some_and(|c| c.iter().any(|l| l.text.contains("T-1 urgent")))
        );
        let lines = e.state.chats.get("pat").map_or(0, Vec::len);
        let off = run(&mut e, Actor::Stakeholder, prioritize("T-1", false));
        assert_eq!(
            off,
            vec![EventKind::TicketPrioritized {
                id: "T-1".into(),
                urgent: false
            }]
        );
        assert!(e.state.ticket("T-1").is_some_and(|t| !t.urgent));
        assert_eq!(e.state.chats.get("pat").map_or(0, Vec::len), lines);
        assert_eq!(off[0].summary(), "T-1 no longer urgent");
    }

    #[test]
    fn urgent_is_optional_in_old_ticket_json_and_omitted_when_false() {
        let mut e = engine();
        hire(&mut e, "Pat");
        story(&mut e, "Login");
        let t = e.state.ticket("T-1").expect("exists").clone();
        let json = serde_json::to_string(&t).expect("serialises");
        assert!(!json.contains("urgent"));
        let back: Ticket = serde_json::from_str(&json).expect("reads");
        assert!(!back.urgent);
        let on = Ticket { urgent: true, ..t };
        let json = serde_json::to_string(&on).expect("serialises");
        assert!(json.contains("\"urgent\":true"));
        let cmd: Command =
            serde_json::from_str(r#"{"type":"prioritize_ticket","id":"T-1","urgent":true}"#)
                .expect("reads");
        assert_eq!(cmd, prioritize("T-1", true));
    }

    #[test]
    fn cancel_turn_while_reply_pending_stops_and_holds_until_stakeholder_writes() {
        let mut e = engine();
        hire(&mut e, "Pat");
        let say = |to: &str| Command::SendChat {
            to: to.into(),
            text: "hi".into(),
        };
        run(&mut e, Actor::Stakeholder, say("pat"));
        assert!(e.state.employee("pat").is_some_and(|v| v.reply_pending));
        record(
            &mut e,
            Actor::System,
            EventKind::PermissionRequested {
                id: "PR-1".into(),
                employee: "pat".into(),
                title: "run".into(),
                choices: vec![],
            },
        );
        let kinds = run(
            &mut e,
            Actor::Stakeholder,
            Command::CancelTurn {
                employee: "pat".into(),
            },
        );
        assert_eq!(
            kinds,
            vec![EventKind::TurnCancelled {
                employee: "pat".into()
            }]
        );
        let v = e.state.employee("pat").expect("hired");
        assert!(v.stopped && !v.reply_pending && !v.turn_open);
        assert_eq!(v.status, EmployeeStatus::Idle);
        assert!(e.state.inbox.is_empty());
        assert_eq!(
            e.state.chats["pat"].last().map(|l| l.text.as_str()),
            Some("Stopped.")
        );
        // A teammate's message does not lift the hold; the stakeholder's does.
        hire(&mut e, "Quinn");
        run(
            &mut e,
            Actor::Employee("quinn".into()),
            Command::SendChat {
                to: "pat".into(),
                text: "yo".into(),
            },
        );
        assert!(e.state.employee("pat").is_some_and(|v| v.stopped));
        run(&mut e, Actor::Stakeholder, say("pat"));
        assert!(e.state.employee("pat").is_some_and(|v| !v.stopped));
    }

    #[test]
    fn cancel_applied_after_the_turn_ended_does_not_hold() {
        let mut e = engine();
        hire(&mut e, "Pat");
        record(
            &mut e,
            Actor::System,
            EventKind::TurnCancelled {
                employee: "pat".into(),
            },
        );
        assert!(e.state.employee("pat").is_some_and(|v| !v.stopped));
    }

    #[test]
    fn cancel_turn_while_working_emits_the_event() {
        let mut e = engine();
        hire(&mut e, "Pat");
        record(
            &mut e,
            Actor::System,
            EventKind::AgentMessageChunk {
                employee: "pat".into(),
                text: "x".into(),
            },
        );
        assert_eq!(
            e.decide(
                &Actor::Stakeholder,
                Command::CancelTurn {
                    employee: "pat".into()
                }
            )
            .expect("accepted")
            .len(),
            1
        );
    }

    #[test]
    fn clear_chat_refused_while_replying() {
        let mut e = engine();
        hire(&mut e, "Pat");
        record(
            &mut e,
            Actor::System,
            EventKind::AgentMessageChunk {
                employee: "pat".into(),
                text: "thinking".into(),
            },
        );
        let err = e
            .decide(
                &Actor::Stakeholder,
                Command::ClearChat {
                    employee: "pat".into(),
                },
            )
            .expect_err("refused");
        assert!(err.to_string().contains("is replying"), "{err}");
    }

    #[test]
    fn hiring_assigns_unique_slugs_and_defaults() {
        let mut e = engine();
        hire(&mut e, "Pat");
        hire(&mut e, "Pat");
        let ids: Vec<_> = e
            .state
            .employees
            .iter()
            .map(|v| v.employee.id.as_str())
            .collect();
        assert_eq!(ids, ["pat", "pat-2"]);
        assert_eq!(e.state.hires_by_role.get("project-manager"), Some(&2));
        assert_eq!(e.state.employees[0].employee.backend, "claude");
        assert_eq!(e.state.employees[0].status, EmployeeStatus::Offline);
    }

    fn hire_with(engine: &mut Engine, backend: Option<&str>, model: Option<&str>) -> Employee {
        let kinds = run(
            engine,
            Actor::Stakeholder,
            Command::HireEmployee {
                role: "project-manager".into(),
                name: Some("Pat".into()),
                backend: backend.map(String::from),
                model: model.map(String::from),
                effort: None,
                keep_defaults: false,
            },
        );
        match &kinds[0] {
            EventKind::EmployeeHired { employee } => employee.clone(),
            other => panic!("expected a hire, got {other:?}"),
        }
    }

    #[test]
    fn a_picked_agent_becomes_the_default_for_later_hires() {
        let mut e = engine();
        assert_eq!(hire_with(&mut e, None, None).backend, "claude");
        assert_eq!(e.state.hiring, HiringDefaults::default());

        let first = hire_with(&mut e, Some("codex"), Some("gpt-5"));
        assert_eq!(
            (first.backend.as_str(), first.model.as_deref()),
            ("codex", Some("gpt-5"))
        );
        assert_eq!(e.state.hiring.backend.as_deref(), Some("codex"));
        assert_eq!(e.state.hiring.model.as_deref(), Some("gpt-5"));

        let next = hire_with(&mut e, None, None);
        assert_eq!(
            (next.backend.as_str(), next.model.as_deref()),
            ("codex", Some("gpt-5"))
        );

        // The same pick again records nothing new.
        let kinds = e
            .decide(
                &Actor::Stakeholder,
                Command::HireEmployee {
                    role: "project-manager".into(),
                    name: Some("Sam".into()),
                    backend: Some("codex".into()),
                    model: Some("gpt-5".into()),
                    effort: None,
                    keep_defaults: false,
                },
            )
            .expect("accepted");
        assert_eq!(kinds.len(), 1);
    }

    #[test]
    fn models_stay_with_their_backend() {
        let role = Role::parse(
            "---\nid: dev\ntitle: Dev\ndefault_backend: claude\nmodel: claude-sonnet-5-5\n---\nDev\n",
        )
        .expect("valid role");
        let none = HiringDefaults::default();
        assert_eq!(
            hire_choice(&none, &role, None, None, None),
            ("claude".into(), Some("claude-sonnet-5-5".into()), None)
        );
        assert_eq!(
            hire_choice(&none, &role, Some("codex".into()), None, None),
            ("codex".into(), None, None)
        );
        let gemini = HiringDefaults {
            backend: Some("gemini".into()),
            model: Some("gemini-3-pro".into()),
            effort: None,
        };
        assert_eq!(
            hire_choice(&gemini, &role, None, None, None),
            ("gemini".into(), Some("gemini-3-pro".into()), None)
        );
        // Another backend picked: neither the default model nor the role's applies.
        assert_eq!(
            hire_choice(&gemini, &role, Some("codex".into()), None, None),
            ("codex".into(), None, None)
        );
    }

    fn hire_with_effort(engine: &mut Engine, backend: &str, model: &str, effort: &str) -> Employee {
        let kinds = run(
            engine,
            Actor::Stakeholder,
            Command::HireEmployee {
                role: "project-manager".into(),
                name: Some("Pat".into()),
                backend: Some(backend.into()),
                model: Some(model.into()),
                effort: Some(effort.into()),
                keep_defaults: false,
            },
        );
        match &kinds[0] {
            EventKind::EmployeeHired { employee } => employee.clone(),
            other => panic!("expected a hire, got {other:?}"),
        }
    }

    #[test]
    fn a_duplicate_copies_the_agent_without_changing_the_defaults() {
        let mut e = engine();
        let first = hire_with_effort(&mut e, "claude", "sonnet", "high");
        // Move the defaults elsewhere, then duplicate the first hire.
        run(
            &mut e,
            Actor::Stakeholder,
            Command::SetHiringDefaults {
                backend: Some("claude".into()),
                model: Some("opus".into()),
                effort: None,
            },
        );
        let before = e.state.hiring.clone();
        let kinds = run(&mut e, Actor::Stakeholder, first.duplicate_command());
        let [EventKind::EmployeeHired { employee }] = &kinds[..] else {
            panic!("expected one hire, got {kinds:?}");
        };
        assert_eq!(employee.model.as_deref(), Some("sonnet"));
        assert_eq!(employee.effort.as_deref(), Some("high"));
        assert_ne!(employee.name, first.name);
        assert_eq!(e.state.hiring, before);
    }

    #[test]
    fn effort_survives_a_hire_and_becomes_the_default() {
        let mut e = engine();
        let first = hire_with_effort(&mut e, "claude", "sonnet", "high");
        assert_eq!(first.effort.as_deref(), Some("high"));
        assert_eq!(e.state.hiring.effort.as_deref(), Some("high"));
        // A plain hire inherits backend, model and effort.
        assert_eq!(
            hire_with(&mut e, None, None).effort.as_deref(),
            Some("high")
        );
        // Blank effort counts as none.
        let kinds = run(
            &mut e,
            Actor::Stakeholder,
            Command::SetHiringDefaults {
                backend: Some("claude".into()),
                model: Some("sonnet".into()),
                effort: Some("  ".into()),
            },
        );
        assert!(matches!(
            &kinds[..],
            [EventKind::HiringDefaultsSet { effort: None, .. }]
        ));
    }

    #[test]
    fn effort_stays_with_its_backend_and_model() {
        let role = Role::parse("---\nid: dev\ntitle: Dev\ndefault_backend: claude\n---\nDev\n")
            .expect("valid role");
        let defaults = HiringDefaults {
            backend: Some("claude".into()),
            model: Some("sonnet".into()),
            effort: Some("high".into()),
        };
        let same = hire_choice(&defaults, &role, None, None, None);
        assert_eq!(same.2.as_deref(), Some("high"));
        let other_backend = hire_choice(&defaults, &role, Some("codex".into()), None, None);
        assert_eq!(other_backend.2, None);
        let other_model = hire_choice(&defaults, &role, None, Some("opus".into()), None);
        assert_eq!(other_model.2, None);
        // An explicit pick always wins.
        let picked = hire_choice(
            &defaults,
            &role,
            None,
            Some("opus".into()),
            Some("low".into()),
        );
        assert_eq!(picked.2.as_deref(), Some("low"));
    }

    #[test]
    fn a_default_effort_needs_a_default_backend() {
        let e = engine();
        let err = e.decide(
            &Actor::Stakeholder,
            Command::SetHiringDefaults {
                backend: None,
                model: None,
                effort: Some("high".into()),
            },
        );
        assert!(err.is_err());
    }

    #[test]
    fn old_events_without_effort_still_load() {
        let hired: EventKind = serde_json::from_str(
            r#"{"type":"employee_hired","employee":{"id":"a","name":"A","role":"r","role_title":"R","backend":"claude"}}"#,
        )
        .expect("old hire");
        assert!(
            matches!(hired, EventKind::EmployeeHired { employee } if employee.effort.is_none())
        );
        let set: EventKind = serde_json::from_str(
            r#"{"type":"hiring_defaults_set","backend":"claude","model":"m"}"#,
        )
        .expect("old defaults");
        assert!(matches!(
            set,
            EventKind::HiringDefaultsSet { effort: None, .. }
        ));
        let cmd: Command =
            serde_json::from_str(r#"{"type":"hire_employee","role":"r","name":"n"}"#).expect("cmd");
        assert!(matches!(cmd, Command::HireEmployee { effort: None, .. }));
    }

    #[test]
    fn hiring_defaults_are_the_stakeholders_to_set() {
        let mut e = engine();
        let set = Command::SetHiringDefaults {
            backend: Some("gemini".into()),
            model: None,
            effort: None,
        };
        assert!(e.decide(&Actor::Employee("x".into()), set.clone()).is_err());
        assert!(
            e.decide(
                &Actor::Stakeholder,
                Command::SetHiringDefaults {
                    backend: None,
                    model: Some("m".into()),
                    effort: None,
                }
            )
            .is_err()
        );
        run(&mut e, Actor::Stakeholder, set);
        assert_eq!(e.state.hiring.backend.as_deref(), Some("gemini"));
        assert_eq!(hire_with(&mut e, None, None).backend, "gemini");
    }

    #[test]
    fn only_the_stakeholder_hires() {
        let e = engine();
        let err = e.decide(
            &Actor::Employee("x".into()),
            Command::HireEmployee {
                role: "project-manager".into(),
                name: Some("Eve".into()),
                backend: None,
                model: None,
                effort: None,
                keep_defaults: false,
            },
        );
        assert_eq!(err, Err(CommandError::StakeholderOnly("hire")));
    }

    #[test]
    fn unknown_role_is_rejected() {
        let e = engine();
        let err = e.decide(
            &Actor::Stakeholder,
            Command::HireEmployee {
                role: "astronaut".into(),
                name: Some("Neil".into()),
                backend: None,
                model: None,
                effort: None,
                keep_defaults: false,
            },
        );
        assert_eq!(err, Err(CommandError::UnknownRole("astronaut".into())));
    }

    #[test]
    fn sprint_gate_blocks_planning_until_approved() {
        let mut e = engine();
        hire(&mut e, "Pat");
        story(&mut e, "Login");
        story(&mut e, "Logout");

        let early = e.decide(
            &Actor::Employee("pat".into()),
            Command::TransitionTicket {
                id: "T-1".into(),
                to: "ready".into(),
            },
        );
        assert_eq!(
            early,
            Err(CommandError::Invalid(
                "T-1 is not in an approved sprint".into()
            ))
        );

        run(
            &mut e,
            Actor::Employee("pat".into()),
            Command::ProposeSprint {
                name: "Sprint 1".into(),
                goal: "auth".into(),
                tickets: vec!["T-1".into(), "T-2".into()],
            },
        );
        assert_eq!(
            e.state.inbox,
            vec![InboxItem::SprintGate {
                id: "IN-1".into(),
                sprint: "SP-1".into()
            }]
        );
        assert_eq!(
            e.decide(
                &Actor::Employee("pat".into()),
                Command::ApproveSprint { id: "SP-1".into() }
            ),
            Err(CommandError::StakeholderOnly("approve sprints"))
        );

        let events = run(
            &mut e,
            Actor::Stakeholder,
            Command::ApproveSprint { id: "SP-1".into() },
        );
        assert_eq!(events.len(), 3, "approval plus one transition per ticket");
        assert!(e.state.inbox.is_empty());
        assert!(e.state.tickets.iter().all(|t| t.state == "ready"));
        assert_eq!(e.state.sprints[0].status, SprintStatus::Active);
    }

    #[test]
    fn rejected_sprint_frees_its_tickets() {
        let mut e = engine();
        story(&mut e, "Login");
        let propose = Command::ProposeSprint {
            name: "S1".into(),
            goal: String::new(),
            tickets: vec!["T-1".into()],
        };
        run(&mut e, Actor::System, propose.clone());
        assert!(matches!(
            e.decide(&Actor::System, propose.clone()),
            Err(CommandError::Invalid(_))
        ));
        run(
            &mut e,
            Actor::Stakeholder,
            Command::RejectSprint {
                id: "SP-1".into(),
                reason: None,
            },
        );
        assert_eq!(e.state.tickets[0].sprint, None);
        assert!(e.decide(&Actor::System, propose).is_ok());
    }

    #[test]
    fn tracker_links_attach_to_items_but_not_from_agents() {
        let mut e = engine();
        story(&mut e, "Login");
        let link = Command::LinkExternal {
            entity: ExternalEntity::Ticket,
            id: "T-1".into(),
            external: ext_ref("uuid-1", "SHOP-1"),
        };
        assert!(matches!(
            e.decide(&Actor::Employee("pat".into()), link.clone()),
            Err(CommandError::Invalid(_))
        ));
        run(&mut e, Actor::System, link);
        assert_eq!(
            e.state.tickets[0]
                .external
                .as_ref()
                .and_then(|x| x.key.as_deref()),
            Some("SHOP-1")
        );
        let missing = Command::LinkExternal {
            entity: ExternalEntity::Sprint,
            id: "SP-9".into(),
            external: ext_ref("uuid-2", "c"),
        };
        assert!(e.decide(&Actor::System, missing).is_err());
    }

    #[test]
    fn comments_carry_attachments_and_may_be_only_a_picture() {
        let mut e = engine();
        run(
            &mut e,
            Actor::Stakeholder,
            Command::CreateTicket {
                kind: TicketKind::Bug,
                title: "Typo".into(),
                description: String::new(),
                acceptance_criteria: Vec::new(),
                estimate: None,
                epic: None,
                external: None,
            },
        );
        let shot = |n: usize| crate::attachment::Attachment {
            id: format!("{n:064x}"),
            name: "shot.png".into(),
            mime: "image/png".into(),
            bytes: 10,
        };
        let comment = |body: &str, attachments| Command::CommentTicket {
            id: "T-1".into(),
            body: body.into(),
            external_id: None,
            attachments,
        };
        assert!(e.decide(&Actor::Stakeholder, comment("", vec![])).is_err());
        run(&mut e, Actor::Stakeholder, comment("", vec![shot(1)]));
        let too_many: Vec<_> = (0..7).map(shot).collect();
        assert!(
            e.decide(&Actor::Stakeholder, comment("x", too_many))
                .is_err()
        );
        let mut svg = shot(2);
        svg.mime = "image/svg+xml".into();
        assert!(
            e.decide(&Actor::Stakeholder, comment("x", vec![svg]))
                .is_err()
        );
        assert_eq!(e.state.tickets[0].comments[0].attachments, vec![shot(1)]);
    }

    #[test]
    fn old_comment_events_decode_without_attachments() {
        let old = r#"{"type":"ticket_commented","id":"T-1","body":"hi"}"#;
        let kind: EventKind = serde_json::from_str(old).expect("old event decodes");
        assert!(
            matches!(kind, EventKind::TicketCommented { attachments, .. } if attachments.is_empty())
        );
    }

    #[test]
    fn imported_tickets_and_comments_keep_their_external_ids() {
        let mut e = engine();
        run(
            &mut e,
            Actor::Stakeholder,
            Command::CreateTicket {
                kind: TicketKind::Bug,
                title: "Typo".into(),
                description: String::new(),
                acceptance_criteria: Vec::new(),
                estimate: None,
                epic: None,
                external: Some(ext_ref("uuid-1", "SHOP-7")),
            },
        );
        assert_eq!(
            e.state.tickets[0].external.as_ref().map(|x| x.id.as_str()),
            Some("uuid-1")
        );
        let comment = Command::CommentTicket {
            id: "T-1".into(),
            body: "seen in prod".into(),
            external_id: Some("c-1".into()),
            attachments: Vec::new(),
        };
        run(&mut e, Actor::Stakeholder, comment.clone());
        // A second import of the same comment is a no-op, not an error.
        assert_eq!(e.decide(&Actor::Stakeholder, comment), Ok(Vec::new()));
        assert_eq!(e.state.tickets[0].comments.len(), 1);
    }

    #[test]
    fn chunks_coalesce_within_a_turn() {
        let mut e = engine();
        hire(&mut e, "Pat");
        let chunk = |t: &str| EventKind::AgentMessageChunk {
            employee: "pat".into(),
            text: t.into(),
        };
        let pat = || Actor::Employee("pat".into());
        record(&mut e, pat(), chunk("Hel"));
        record(&mut e, pat(), chunk("lo"));
        record(
            &mut e,
            pat(),
            EventKind::AgentToolCall {
                employee: "pat".into(),
                tool_call_id: "c1".into(),
                title: "Read file".into(),
                tool_kind: "read".into(),
                status: "pending".into(),
            },
        );
        record(
            &mut e,
            pat(),
            EventKind::AgentToolCallUpdated {
                employee: "pat".into(),
                tool_call_id: "c1".into(),
                title: None,
                status: Some("completed".into()),
            },
        );
        record(&mut e, pat(), chunk("Done"));
        record(
            &mut e,
            pat(),
            EventKind::AgentTurnEnded {
                employee: "pat".into(),
                stop_reason: "end_turn".into(),
                ticket: None,
            },
        );
        record(&mut e, pat(), chunk("Next"));

        let texts: Vec<_> = e.state.chats["pat"]
            .iter()
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(texts, ["Hello", "Read file [completed]", "Done", "Next"]);
        assert_eq!(e.state.employees[0].status, EmployeeStatus::Working);
    }

    #[test]
    fn permission_flow_updates_inbox_and_status() {
        let mut e = engine();
        hire(&mut e, "Pat");
        record(
            &mut e,
            Actor::Employee("pat".into()),
            EventKind::PermissionRequested {
                id: "IN-1".into(),
                employee: "pat".into(),
                title: "rm -rf build".into(),
                choices: vec![PermissionChoice {
                    id: "allow".into(),
                    name: "Allow".into(),
                    kind: "allow_once".into(),
                }],
            },
        );
        assert_eq!(
            e.state.employees[0].status,
            EmployeeStatus::WaitingOnStakeholder
        );
        assert!(
            e.decide(
                &Actor::Stakeholder,
                Command::ResolvePermission {
                    id: "IN-1".into(),
                    choice: Some("nope".into())
                }
            )
            .is_err()
        );
        run(
            &mut e,
            Actor::Stakeholder,
            Command::ResolvePermission {
                id: "IN-1".into(),
                choice: Some("allow".into()),
            },
        );
        assert!(e.state.inbox.is_empty());
        assert_eq!(e.state.next_inbox_id(), "IN-2");
        assert_eq!(e.state.employees[0].status, EmployeeStatus::Working);
    }

    #[test]
    fn employees_cannot_assign_build_work_to_a_role_that_cannot_build() {
        let roles = RoleCatalog::new([
            Role::parse("---\nid: project-manager\ntitle: PM\ntags: [manager]\n---\nPM\n").unwrap(),
            Role::parse("---\nid: dev\ntitle: Dev\ntags: [developer]\n---\nDev\n").unwrap(),
            Role::parse("---\nid: qa-engineer\ntitle: QA\ntags: [qa]\n---\nQA\n").unwrap(),
        ]);
        let mut e = Engine::new("Acme", roles, Workflow::default());
        for (role, name) in [
            ("project-manager", "Pam"),
            ("dev", "Dee"),
            ("qa-engineer", "Quinn"),
        ] {
            run(
                &mut e,
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
        story(&mut e, "Login");
        let assign = |who: &str| Command::AssignTicket {
            id: "T-1".into(),
            assignee: Some(who.into()),
        };
        let err = e
            .decide(&Actor::Employee("pam".into()), assign("quinn"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("dev"), "{err}");
        assert!(
            e.decide(&Actor::Employee("pam".into()), assign("dee"))
                .is_ok()
        );
        assert!(e.decide(&Actor::Stakeholder, assign("quinn")).is_ok());
        assert!(e.decide(&Actor::System, assign("quinn")).is_ok());
    }

    #[test]
    fn letting_go_unassigns_tickets() {
        let mut e = engine();
        hire(&mut e, "Pat");
        story(&mut e, "Login");
        run(
            &mut e,
            Actor::Stakeholder,
            Command::AssignTicket {
                id: "T-1".into(),
                assignee: Some("pat".into()),
            },
        );
        run(
            &mut e,
            Actor::Stakeholder,
            Command::LetGo {
                employee: "pat".into(),
            },
        );
        assert!(e.state.employees.is_empty());
        assert_eq!(e.state.tickets[0].assignee, None);
    }

    fn staffed(max: u32) -> Engine {
        let roles = RoleCatalog::new([
            Role::parse("---\nid: project-manager\ntitle: Project Manager\n---\nPM\n")
                .expect("valid role"),
            Role::parse("---\nid: dev\ntitle: Developer\n---\nDev\n").expect("valid role"),
            Role::parse("---\nid: lead\ntitle: Lead\n---\nLead\n").expect("valid role"),
        ]);
        let mut e = Engine::new("Acme", roles, Workflow::default()).with_team(TeamConfig {
            hiring_roles: vec!["project-manager".into()],
            max_team_size: max,
            ..TeamConfig::default()
        });
        hire(&mut e, "Pat");
        e
    }

    fn pm() -> Actor {
        Actor::Employee("pat".into())
    }

    fn teammate(role: &str, name: Option<&str>, reason: &str) -> Command {
        Command::HireTeammate {
            role: role.into(),
            name: name.map(Into::into),
            reason: reason.into(),
        }
    }

    fn invalid(e: &Engine, actor: &Actor, cmd: Command) -> String {
        match e.decide(actor, cmd) {
            Err(CommandError::Invalid(m)) => m,
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    fn hire_named(engine: &mut Engine, name: Option<&str>) -> Employee {
        let kinds = run(
            engine,
            Actor::Stakeholder,
            Command::HireEmployee {
                role: "project-manager".into(),
                name: name.map(String::from),
                backend: None,
                model: None,
                effort: None,
                keep_defaults: false,
            },
        );
        match &kinds[0] {
            EventKind::EmployeeHired { employee } => employee.clone(),
            other => panic!("expected a hire, got {other:?}"),
        }
    }

    #[test]
    fn a_hire_without_a_name_gets_the_first_list_name() {
        let mut e = engine();
        assert_eq!(hire_named(&mut e, None).name, "Ada");
    }

    #[test]
    fn a_blank_name_is_treated_as_missing() {
        let mut e = engine();
        assert_eq!(hire_named(&mut e, Some("  ")).name, "Ada");
    }

    #[test]
    fn default_names_skip_names_already_taken() {
        let mut e = engine();
        assert_eq!(hire_named(&mut e, Some("ada")).name, "ada");
        assert_eq!(hire_named(&mut e, None).name, "Grace");
        assert_eq!(hire_named(&mut e, None).name, "Linus");
    }

    #[test]
    fn a_hire_command_without_a_name_field_still_deserializes() {
        let cmd: Command =
            serde_json::from_str(r#"{"type":"hire_employee","role":"project-manager"}"#)
                .expect("name is optional");
        assert!(matches!(cmd, Command::HireEmployee { name: None, .. }));
    }

    #[test]
    fn an_unnamed_teammate_gets_a_first_name_not_the_role_title() {
        let mut e = staffed(8);
        let kinds = run(&mut e, pm(), teammate("dev", None, "T-1 needs a dev"));
        assert!(
            matches!(&kinds[0], EventKind::EmployeeHired { employee } if employee.name == "Ada" && employee.role_title == "Developer")
        );
        let kinds = run(&mut e, pm(), teammate("dev", None, "T-2 needs a dev"));
        assert!(
            matches!(&kinds[0], EventKind::EmployeeHired { employee } if employee.name == "Grace")
        );
    }

    #[test]
    fn a_hiring_role_hires_directly_and_the_stakeholder_is_notified() {
        let mut e = staffed(8);
        let kinds = run(
            &mut e,
            pm(),
            teammate("dev", Some("Dana"), "T-1 and T-2 need a dev"),
        );
        assert!(
            matches!(&kinds[0], EventKind::EmployeeHired { employee } if employee.id == "dana" && employee.role == "dev")
        );
        match &kinds[1] {
            EventKind::AlertRaised { id, title, detail } => {
                assert_eq!(id, "IN-1");
                assert_eq!(title, "Pat hired Dana (Developer)");
                assert_eq!(detail, "T-1 and T-2 need a dev");
            }
            other => panic!("expected alert, got {other:?}"),
        }
        assert_eq!(kinds.len(), 2);
        let kinds = run(&mut e, pm(), teammate("lead", Some("Lee"), "review"));
        assert!(matches!(&kinds[1], EventKind::AlertRaised { id, .. } if id == "IN-2"));
        assert_eq!(e.state.employees.len(), 3);
    }

    #[test]
    fn other_actors_and_roles_may_not_hire_teammates() {
        let mut e = staffed(8);
        run(&mut e, pm(), teammate("dev", Some("Dana"), "x"));
        let m = invalid(
            &e,
            &Actor::Employee("dana".into()),
            teammate("dev", None, "x"),
        );
        assert!(m.contains("may not hire"), "{m}");
        let m = invalid(&e, &Actor::Stakeholder, teammate("dev", None, "x"));
        assert!(m.contains("HireEmployee"), "{m}");
        let m = invalid(&e, &Actor::System, teammate("dev", None, "x"));
        assert!(m.contains("HireEmployee"), "{m}");
        assert!(matches!(
            e.decide(&Actor::Employee("ghost".into()), teammate("dev", None, "x")),
            Err(CommandError::UnknownEmployee(_))
        ));
    }

    #[test]
    fn the_cap_binds_employees_but_not_the_stakeholder() {
        let mut e = staffed(2);
        run(&mut e, pm(), teammate("dev", Some("Dana"), "x"));
        let m = invalid(&e, &pm(), teammate("dev", Some("Dee"), "x"));
        assert!(m.contains('2') && m.contains("ask the stakeholder"), "{m}");
        run(
            &mut e,
            Actor::Stakeholder,
            Command::HireEmployee {
                role: "dev".into(),
                name: Some("Sam".into()),
                backend: None,
                model: None,
                effort: None,
                keep_defaults: false,
            },
        );
        assert_eq!(e.state.employees.len(), 3);
        run(
            &mut e,
            Actor::Stakeholder,
            Command::LetGo {
                employee: "sam".into(),
            },
        );
        run(
            &mut e,
            Actor::Stakeholder,
            Command::LetGo {
                employee: "dana".into(),
            },
        );
        run(&mut e, pm(), teammate("dev", Some("Dee"), "x"));
        assert!(
            staffed(0)
                .decide(&pm(), teammate("dev", None, "x"))
                .is_err()
        );
    }

    #[test]
    fn hiring_roles_cannot_be_hired_and_unknown_roles_fail() {
        let mut e = staffed(8);
        let m = invalid(&e, &pm(), teammate("project-manager", None, "x"));
        assert!(m.contains("project-manager"), "{m}");
        assert!(matches!(
            e.decide(&pm(), teammate("nope", None, "x")),
            Err(CommandError::UnknownRole(_))
        ));
        // A role added to `hiring_roles` can hire, and is itself off limits.
        e.team.hiring_roles.push("lead".into());
        run(
            &mut e,
            Actor::Stakeholder,
            Command::HireEmployee {
                role: "lead".into(),
                name: Some("Lee".into()),
                backend: None,
                model: None,
                effort: None,
                keep_defaults: false,
            },
        );
        run(
            &mut e,
            Actor::Employee("lee".into()),
            teammate("dev", None, "x"),
        );
        invalid(&e, &pm(), teammate("lead", None, "x"));
        // Nobody may hire when the list is empty.
        e.team.hiring_roles.clear();
        invalid(&e, &pm(), teammate("dev", None, "x"));
    }

    #[test]
    fn reason_is_required_names_default_and_defaults_are_untouched() {
        let mut e = staffed(8);
        let m = invalid(&e, &pm(), teammate("dev", None, "   "));
        assert!(m.contains("reason"), "{m}");
        let kinds = run(&mut e, pm(), teammate("dev", Some("  "), "x"));
        assert!(
            matches!(&kinds[0], EventKind::EmployeeHired { employee } if employee.name == "Ada" && employee.id == "ada")
        );
        let kinds = run(&mut e, pm(), teammate("dev", None, "x"));
        assert!(
            matches!(&kinds[0], EventKind::EmployeeHired { employee } if employee.id == "grace")
        );
        assert!(
            !kinds
                .iter()
                .any(|k| matches!(k, EventKind::HiringDefaultsSet { .. }))
        );
    }

    #[test]
    fn slugs() {
        assert_eq!(slug("Alice Smith"), "alice-smith");
        assert_eq!(slug("  Ünï  Bob!! "), "n-bob");
    }
}
