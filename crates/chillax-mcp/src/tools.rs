//! The company handbook as tools. Independent of the MCP transport so
//! the logic is testable on its own.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chillax_core::{
    Actor, Attachment, AttachmentStore, Command, CompanyHandle, CriterionResult, EventKind, NewBug,
    QaVerdict, ReviewVerdict, Role, SuggestionKind, TicketKind,
};
use chillax_tracker::{
    Assignee, ItemFilter, NewEpic, NewSprint, NewWorkItem, Tracker, WorkItemPatch,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

/// A tool's name, description and JSON Schema for its arguments.
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: fn() -> Value,
}

pub const TOOLS: &[ToolDef] = &[
    ToolDef {
        name: "my_assignments",
        description: "Tickets I should act on now, oldest first: assigned to me in a state my \
                      role works in, plus tickets I am reviewing.",
        schema: empty,
    },
    ToolDef {
        name: "get_ticket",
        description: "Full ticket: description, acceptance criteria, state, comments.",
        schema: || object(json!({"id": string("Ticket key, e.g. T-3")}), &["id"]),
    },
    ToolDef {
        name: "list_tickets",
        description: "List tickets, optionally filtered by workflow state, assignee or epic.",
        schema: || {
            object(
                json!({
                    "state": string("Workflow state key, e.g. backlog"),
                    "assignee": string("Employee id"),
                    "epic": string("Epic key, e.g. EP-1"),
                }),
                &[],
            )
        },
    },
    ToolDef {
        name: "list_epics",
        description: "List all epics.",
        schema: empty,
    },
    ToolDef {
        name: "list_team",
        description: "Everyone on the team: id, name, role and current status.",
        schema: empty,
    },
    ToolDef {
        name: "board_digest",
        description: "The board at a glance: team and load, open work per state, urgent and \
                      unassigned tickets, blocked ones, which independent tickets could start \
                      now, free developers and room to hire. Read it before staffing.",
        schema: empty,
    },
    ToolDef {
        name: "list_roles",
        description: "Roles you can hire, with id, title, description and tags, plus the \
                      current team size and the cap. Call this before hire_teammate.",
        schema: empty,
    },
    ToolDef {
        name: "hire_teammate",
        description: "Hire a teammate directly, with no approval step; the stakeholder is \
                      notified in their inbox. The team has a size cap, so hire only for \
                      work that exists. Call list_roles first for role ids.",
        schema: || {
            object(
                json!({
                    "role": string("Role id from list_roles"),
                    "reason": string("One sentence: why, naming the tickets"),
                    "name": string("A first name; Chillax picks one if omitted"),
                }),
                &["role", "reason"],
            )
        },
    },
    ToolDef {
        name: "report_app",
        description: "Tell the stakeholder where an app you started is running (a dev \
                      server in your worktree, a preview). localhost only. Call again to \
                      update; down=true when you stop it.",
        schema: || {
            object(
                json!({
                    "url": string("http://localhost:PORT/ URL; give this or port"),
                    "port": {"type": "integer", "minimum": 1, "maximum": 65535,
                        "description": "Local port, when you do not have a URL"},
                    "env": string("Label, e.g. dev or preview; defaults to your ticket"),
                    "ticket": string("Ticket key the app shows; defaults to your current one"),
                    "note": string("One line, e.g. what to look at (max 200 chars)"),
                    "down": {"type": "boolean", "description": "The app stopped; remove it"},
                }),
                &[],
            )
        },
    },
    ToolDef {
        name: "create_epic",
        description: "Create an epic (a large body of work grouping stories).",
        schema: || {
            object(
                json!({"title": string("Epic title"), "description": string("What and why")}),
                &["title"],
            )
        },
    },
    ToolDef {
        name: "create_story",
        description: "Create a story, bug or task in the backlog. Always include testable \
                      acceptance criteria.",
        schema: || {
            object(
                json!({
                    "title": string("Short imperative title"),
                    "description": string("User story / details (markdown)"),
                    "acceptance_criteria": {"type": "array", "items": {"type": "string"}},
                    "estimate": {"type": "integer", "minimum": 0, "description": "Story points"},
                    "epic": string("Epic key, e.g. EP-1"),
                    "blocked_by": {"type": "array", "items": {"type": "string"},
                        "description": "Ticket keys that must be merged before work on this starts"},
                    "kind": {"type": "string", "enum": ["story", "bug", "task"], "default": "story"},
                    "assignee": string("Employee id or name, or a role id, to assign it to \
                        right away"),
                    "fingerprint": string("Failure triage only: the fingerprint from Chillax's \
                        briefing. Links this bug to that failure so it is filed once; Chillax \
                        then puts the bug in the active sprint"),
                    "request_id": string("Request intake only: the request key (R-1) from \
                        Chillax's briefing. Links this ticket to the request; for a request \
                        that starts now it also joins the active sprint, ready"),
                }),
                &["title", "acceptance_criteria"],
            )
        },
    },
    ToolDef {
        name: "finish_request",
        description: "Request intake: you are done with a stakeholder request. Say what you \
                      filed and who got each ticket, or why nothing was needed. The \
                      stakeholder gets a note in their Inbox.",
        schema: || {
            object(
                json!({
                    "request_id": string("The request key, e.g. R-1"),
                    "summary": string("Tickets filed and their owners, with the reason"),
                }),
                &["request_id", "summary"],
            )
        },
    },
    ToolDef {
        name: "problem_resolved",
        description: "Problem handling: you dealt with a problem Chillax briefed you about \
                      (the key after `P-`). Say in one line what you did. The stakeholder \
                      gets a note in their Inbox; without it they are told you did not \
                      resolve it.",
        schema: || {
            object(
                json!({
                    "key": string("The problem key from the briefing, without `P-`"),
                    "summary": string("One line: what you did"),
                }),
                &["key", "summary"],
            )
        },
    },
    ToolDef {
        name: "retry_action",
        description: "Problem handling: ask Chillax to try a failed action again now, \
                      skipping its wait. `key` is the problem key from the briefing.",
        schema: || {
            object(
                json!({"key": string("The problem key from the briefing, without `P-`")}),
                &["key"],
            )
        },
    },
    ToolDef {
        name: "restart_teammate",
        description: "Problem handling: restart a teammate whose agent session failed. \
                      Refused for a teammate the stakeholder stopped, and for yourself.",
        schema: || {
            object(
                json!({"employee": string("The teammate's id, from list_team")}),
                &["employee"],
            )
        },
    },
    ToolDef {
        name: "create_sprint",
        description: "Propose a sprint from backlog tickets. It waits for the stakeholder's \
                      approval (sprint_start gate) before its tickets become ready.",
        schema: || {
            object(
                json!({
                    "name": string("e.g. Sprint 1"),
                    "goal": string("One-sentence sprint goal"),
                    "tickets": {"type": "array", "items": {"type": "string"}},
                }),
                &["name", "tickets"],
            )
        },
    },
    ToolDef {
        name: "assign",
        description: "Assign a ticket to an employee (or unassign with employee = null).",
        schema: || {
            object(
                json!({"ticket": string("Ticket key"), "employee": {"type": ["string", "null"]}}),
                &["ticket"],
            )
        },
    },
    ToolDef {
        name: "update_ticket",
        description: "Edit a ticket's title, description, acceptance criteria or estimate.",
        schema: || {
            object(
                json!({
                    "id": string("Ticket key"),
                    "title": {"type": "string"},
                    "description": {"type": "string"},
                    "acceptance_criteria": {"type": "array", "items": {"type": "string"}},
                    "estimate": {"type": "integer", "minimum": 0},
                    "blocked_by": {"type": "array", "items": {"type": "string"},
                        "description": "Replace the ticket keys that must be merged first"},
                }),
                &["id"],
            )
        },
    },
    ToolDef {
        name: "transition",
        description: "Move a ticket to another workflow state (validated against workflow.yaml).",
        schema: || {
            object(
                json!({"id": string("Ticket key"), "to": string("Target state key")}),
                &["id", "to"],
            )
        },
    },
    ToolDef {
        name: "comment",
        description: "Comment on a ticket. Attach images (png, jpeg, gif or webp; at most 6, \
                      10 MB each) by file path, e.g. a QA screenshot saved under your scratch \
                      folder; paths must be inside your worktree or scratch folder.",
        schema: || {
            object(
                json!({
                    "id": string("Ticket key"),
                    "body": string("Markdown; may be empty when images are attached"),
                    "attachments": {"type": "array", "items": {"type": "string"},
                        "maxItems": 6, "description": "Image file paths"},
                }),
                &["id"],
            )
        },
    },
    ToolDef {
        name: "request_review",
        description: "Say the work on my ticket is done. Chillax commits my worktree, runs the \
                      project's tests and, if they pass, hands the branch to a reviewer. I get \
                      the result as a message; stop working on the ticket until then.",
        schema: || {
            object(
                json!({
                    "id": string("Ticket key"),
                    "summary": string("What changed and how I tested it"),
                }),
                &["id", "summary"],
            )
        },
    },
    ToolDef {
        name: "submit_review",
        description: "Approve the branch I am reviewing, or ask for changes. Approval lets \
                      Chillax merge it into dev; requested changes go back to the developer.",
        schema: || {
            object(
                json!({
                    "id": string("Ticket key"),
                    "verdict": {"type": "string", "enum": ["approve", "request_changes"]},
                    "body": string("Review notes; required when requesting changes"),
                }),
                &["id", "verdict"],
            )
        },
    },
    ToolDef {
        name: "submit_qa",
        description: "Give my verdict on a ticket I am testing in the QA round. `pass` needs a \
                      result with evidence for every acceptance criterion and signs the ticket \
                      off. `fail` needs the failed criteria with evidence and a bug (title; \
                      steps to reproduce, expected and actual); Chillax files it for the \
                      developer. `env` says the environment, not the product, is broken: it is \
                      retried once, then goes to the stakeholder. One verdict per ticket.",
        schema: || {
            object(
                json!({
                    "id": string("Ticket key"),
                    "verdict": {"type": "string", "enum": ["pass", "fail", "env"]},
                    "criteria": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "criterion": string("The acceptance criterion"),
                                "passed": {"type": "boolean"},
                                "evidence": string("What I ran or saw"),
                            },
                            "required": ["criterion", "passed", "evidence"],
                        },
                    },
                    "notes": string("Test notes, shown on the ticket"),
                    "bug": {
                        "type": "object",
                        "description": "Required with fail",
                        "properties": {
                            "title": string("Short title of the bug"),
                            "description": string(
                                "Steps to reproduce, expected behaviour, actual behaviour"
                            ),
                        },
                        "required": ["title", "description"],
                    },
                }),
                &["id", "verdict"],
            )
        },
    },
    ToolDef {
        name: "handoff",
        description: "Pass a ticket to the least-loaded teammate in another role.",
        schema: || {
            object(
                json!({
                    "id": string("Ticket key"),
                    "role": string("Role id or tag, e.g. qa-engineer"),
                    "note": string("What the next person needs to know"),
                }),
                &["id", "role"],
            )
        },
    },
    ToolDef {
        name: "ask_stakeholder",
        description: "Put a question in the stakeholder's inbox. Their answer arrives later \
                      as a new message; don't wait for it, carry on with other work.",
        schema: || {
            object(
                json!({"question": string("A clear, specific question")}),
                &["question"],
            )
        },
    },
    ToolDef {
        name: "report_progress",
        description: "Short status update shown on the dashboard and in standups.",
        schema: || {
            object(
                json!({"summary": string("One or two sentences")}),
                &["summary"],
            )
        },
    },
    ToolDef {
        name: "suggest_work",
        description: "Tell the project manager about a bug or an improvement you noticed \
                      outside your current ticket. Be specific: where, how to reproduce it, \
                      why it matters. The project manager files it or declines it; keep \
                      working on your own ticket. For a blocker on your own ticket use \
                      message_teammate; for a decision only the stakeholder can make use \
                      ask_stakeholder.",
        schema: || {
            object(
                json!({
                    "kind": {"type": "string", "enum": ["bug", "improvement"]},
                    "summary": string("What you saw and why it matters (markdown)"),
                    "ticket": string("The ticket you were on when you noticed it, e.g. T-3"),
                }),
                &["kind", "summary"],
            )
        },
    },
    ToolDef {
        name: "message_teammate",
        description: "Send a direct message to another employee.",
        schema: || {
            object(
                json!({"employee": string("Employee id"), "text": string("Message")}),
                &["employee", "text"],
            )
        },
    },
];

/// Tools a role may see, in catalog order.
pub fn tools_for(role: &Role) -> impl Iterator<Item = &'static ToolDef> + '_ {
    TOOLS.iter().filter(|t| role.may_use_tool(t.name))
}

#[derive(Debug, PartialEq, Eq)]
pub enum ToolError {
    /// The tool exists but the caller's role may not use it.
    Forbidden(String),
    UnknownTool(String),
    BadArguments(String),
    /// The company or tracker rejected the request; the agent should read and adapt.
    Rejected(String),
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Forbidden(t) => write!(f, "your role may not use `{t}`"),
            Self::UnknownTool(t) => write!(f, "unknown tool `{t}`"),
            Self::BadArguments(e) => write!(f, "invalid arguments: {e}"),
            Self::Rejected(e) => f.write_str(e),
        }
    }
}

/// Where agents' images go: the store, and the only folders a path may come
/// from (worktrees and QA scratch folders).
#[derive(Clone)]
pub struct AttachmentAccess {
    pub store: Arc<dyn AttachmentStore>,
    pub roots: Vec<PathBuf>,
}

pub struct ToolContext {
    pub company: Arc<dyn CompanyHandle>,
    pub tracker: Arc<dyn Tracker>,
    /// `None` refuses attachments.
    pub attachments: Option<AttachmentAccess>,
}

impl ToolContext {
    /// Copy the agent's image files into the store. Paths are resolved (symlinks too) and
    /// must lie inside the allowed folders.
    async fn import_images(&self, paths: Vec<String>) -> Result<Vec<Attachment>, ToolError> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let Some(access) = self.attachments.clone() else {
            return Err(ToolError::Rejected("attachments are not enabled".into()));
        };
        tokio::task::spawn_blocking(move || {
            paths
                .iter()
                .map(|p| {
                    access
                        .store
                        .import(Path::new(p), Some(&access.roots))
                        .map_err(|e| ToolError::Rejected(format!("{p}: {e}")))
                })
                .collect()
        })
        .await
        .map_err(|e| ToolError::Rejected(e.to_string()))?
    }

    /// Run a tool for an employee. Returns JSON for the agent to read.
    pub async fn call(
        &self,
        employee: &str,
        role: &Role,
        name: &str,
        args: Value,
    ) -> Result<Value, ToolError> {
        if !TOOLS.iter().any(|t| t.name == name) {
            return Err(ToolError::UnknownTool(name.to_string()));
        }
        if !role.may_use_tool(name) {
            return Err(ToolError::Forbidden(name.to_string()));
        }
        let me = Actor::Employee(employee.to_string());
        let tracker = &self.tracker;
        let value = match name {
            "my_assignments" => {
                let state = self.company.snapshot().await;
                let mut open: Vec<_> = state
                    .assignments(employee)
                    .into_iter()
                    .filter(|t| t.state != "done")
                    .filter(|t| role.owns_states.is_empty() || role.owns_states.contains(&t.state))
                    .cloned()
                    .collect();
                let reviewing = state.tickets.iter().filter(|t| {
                    t.state == "in_review"
                        && t.dropped.is_none()
                        && t.reviewer.as_deref() == Some(employee)
                });
                for ticket in reviewing {
                    if !open.iter().any(|t| t.id == ticket.id) {
                        open.push(ticket.clone());
                    }
                }
                // Testers are not assigned tickets: they test what the round covers.
                if role.owns_states.iter().any(|s| s == "in_qa")
                    && let Some(round) = state
                        .open_round()
                        .filter(|r| r.status == chillax_core::QaRoundStatus::Testing)
                {
                    for ticket in round
                        .tickets
                        .iter()
                        .filter(|id| !round.verdicts.contains_key(*id))
                        .filter_map(|id| state.ticket(id))
                        .filter(|t| t.state == "in_qa")
                    {
                        if !open.iter().any(|t| t.id == ticket.id) {
                            open.push(ticket.clone());
                        }
                    }
                }
                json!(open)
            }
            "get_ticket" => {
                let a: IdArgs = parse(args)?;
                json!(tracker.get_item(&a.id).await.map_err(rejected)?)
            }
            "list_tickets" => {
                let filter: ItemFilter = parse(args)?;
                json!(tracker.list_items(filter).await.map_err(rejected)?)
            }
            "list_epics" => json!(tracker.list_epics().await.map_err(rejected)?),
            "list_team" => {
                let state = self.company.snapshot().await;
                let team: Vec<_> = state
                    .employees
                    .iter()
                    .map(|e| {
                        json!({
                            "id": e.employee.id,
                            "name": e.employee.name,
                            "role": e.employee.role,
                            "title": e.employee.role_title,
                            "status": e.status,
                            "you": e.employee.id == employee,
                        })
                    })
                    .collect();
                json!(team)
            }
            "board_digest" => {
                let engine = self.company.engine_snapshot().await;
                match chillax_core::checkin::digest(&engine, &Default::default(), None) {
                    Some(text) => json!({ "digest": text }),
                    None => json!({ "digest": "No open work." }),
                }
            }
            "list_roles" => {
                let policy = self.company.team_policy();
                let state = self.company.snapshot().await;
                let roles: Vec<_> = self
                    .company
                    .roles()
                    .into_iter()
                    .filter(|r| !policy.hiring_roles.contains(&r.id))
                    .map(|r| {
                        json!({
                            "id": r.id,
                            "title": r.title,
                            "description": r.description,
                            "tags": r.tags,
                        })
                    })
                    .collect();
                json!({
                    "roles": roles,
                    "team_size": state.employees.len(),
                    "max_team_size": policy.max_team_size,
                })
            }
            "hire_teammate" => {
                let a: HireTeammateArgs = parse(args)?;
                let events = self
                    .company
                    .send(
                        me.clone(),
                        Command::HireTeammate {
                            role: a.role,
                            name: a.name,
                            reason: a.reason,
                        },
                    )
                    .await
                    .map_err(|e| ToolError::Rejected(e.to_string()))?;
                let hired = events.iter().find_map(|e| match &e.kind {
                    EventKind::EmployeeHired { employee } => Some(employee),
                    _ => None,
                });
                match hired {
                    Some(e) => json!({
                        "ok": true,
                        "employee": e.id,
                        "name": e.name,
                        "role": e.role,
                    }),
                    None => ok(),
                }
            }
            "report_app" => {
                let a: ReportAppArgs = parse(args)?;
                let events = self
                    .company
                    .send(
                        me.clone(),
                        Command::ReportApp {
                            env: a.env,
                            url: a.url,
                            port: a.port,
                            ticket: a.ticket,
                            note: a.note,
                            down: a.down,
                        },
                    )
                    .await
                    .map_err(|e| ToolError::Rejected(e.to_string()))?;
                let url = events.iter().find_map(|e| match &e.kind {
                    EventKind::AppReported { url, down, .. } if !down => Some(url.clone()),
                    _ => None,
                });
                json!({"ok": true, "url": url})
            }
            "create_epic" => {
                let a: NewEpic = parse(args)?;
                json!(tracker.create_epic(&me, a).await.map_err(rejected)?)
            }
            "create_story" => {
                let a: StoryArgs = parse(args)?;
                let assignee = match &a.assignee {
                    Some(who) => Some(self.resolve_employee(who).await?),
                    None => None,
                };
                if let Some(fingerprint) = &a.fingerprint {
                    // Refuse before creating anything, so a duplicate leaves no stray ticket.
                    self.check_triage(fingerprint, a.kind).await?;
                }
                if let Some(request) = &a.request_id {
                    self.check_request(request).await?;
                }
                let item = NewWorkItem {
                    kind: a.kind.unwrap_or(TicketKind::Story),
                    title: a.title,
                    description: a.description,
                    acceptance_criteria: a.acceptance_criteria,
                    estimate: a.estimate,
                    epic: a.epic,
                };
                let ticket = tracker.create_item(&me, item).await.map_err(rejected)?;
                if !a.blocked_by.is_empty() {
                    let blocked_by = a.blocked_by;
                    self.send(
                        &me,
                        Command::SetBlockers {
                            id: ticket.id.clone(),
                            blocked_by,
                        },
                    )
                    .await?;
                }
                if let Some(employee) = assignee {
                    tracker
                        .assign(&me, &ticket.id, Assignee::Agent(employee))
                        .await
                        .map_err(rejected)?;
                }
                if let Some(request) = a.request_id {
                    self.send(
                        &me,
                        Command::LinkRequestTicket {
                            request,
                            ticket: ticket.id.clone(),
                        },
                    )
                    .await?;
                }
                if let Some(fingerprint) = a.fingerprint {
                    self.send(
                        &me,
                        Command::LinkTriageBug {
                            fingerprint,
                            bug: ticket.id.clone(),
                        },
                    )
                    .await?;
                }
                json!(
                    self.company
                        .snapshot()
                        .await
                        .ticket(&ticket.id)
                        .unwrap_or(&ticket)
                )
            }
            "finish_request" => {
                let a: FinishRequestArgs = parse(args)?;
                self.check_request(&a.request_id).await?;
                self.send(
                    &me,
                    Command::CompleteRequest {
                        id: a.request_id,
                        summary: a.summary,
                    },
                )
                .await?;
                ok()
            }
            "problem_resolved" => {
                let a: ProblemArgs = parse(args)?;
                self.send(
                    &me,
                    Command::ResolveProblem {
                        key: a.key.trim_start_matches("P-").to_string(),
                        summary: a.summary,
                    },
                )
                .await?;
                ok()
            }
            "retry_action" => {
                let a: ProblemArgs = parse(args)?;
                self.send(
                    &me,
                    Command::RetryAction {
                        key: a.key.trim_start_matches("P-").to_string(),
                    },
                )
                .await?;
                ok()
            }
            "restart_teammate" => {
                let a: RestartArgs = parse(args)?;
                self.send(
                    &me,
                    Command::RestartEmployee {
                        employee: a.employee,
                    },
                )
                .await?;
                ok()
            }
            "create_sprint" => {
                let a: SprintArgs = parse(args)?;
                let sprint = NewSprint {
                    name: a.name,
                    goal: a.goal,
                    items: a.tickets,
                };
                let sprint = tracker.create_sprint(&me, sprint).await.map_err(rejected)?;
                json!({
                    "sprint": sprint,
                    "note": "Proposed. The stakeholder must approve it before work starts."
                })
            }
            "assign" => {
                let a: AssignArgs = parse(args)?;
                let to = a.employee.map_or(Assignee::Unassigned, Assignee::Agent);
                tracker.assign(&me, &a.ticket, to).await.map_err(rejected)?;
                ok()
            }
            "update_ticket" => {
                let a: UpdateArgs = parse(args)?;
                let patch = WorkItemPatch {
                    title: a.title,
                    description: a.description,
                    acceptance_criteria: a.acceptance_criteria,
                    estimate: a.estimate,
                    epic: None,
                };
                tracker
                    .update_item(&me, &a.id, patch)
                    .await
                    .map_err(rejected)?;
                if let Some(blocked_by) = a.blocked_by {
                    self.send(
                        &me,
                        Command::SetBlockers {
                            id: a.id,
                            blocked_by,
                        },
                    )
                    .await?;
                }
                ok()
            }
            "transition" => {
                let a: TransitionArgs = parse(args)?;
                tracker
                    .transition(&me, &a.id, &a.to)
                    .await
                    .map_err(rejected)?;
                ok()
            }
            "comment" => {
                let a: CommentArgs = parse(args)?;
                let attachments = self.import_images(a.attachments).await?;
                tracker
                    .comment_with(&me, &a.id, &a.body, attachments)
                    .await
                    .map_err(rejected)?;
                ok()
            }
            "request_review" => {
                let a: ReviewRequestArgs = parse(args)?;
                self.send(
                    &me,
                    Command::RequestReview {
                        id: a.id,
                        summary: a.summary,
                    },
                )
                .await?;
                json!({
                    "ok": true,
                    "note": "Chillax will commit your work, run the tests and find a reviewer. \
                             You will get the result as a message; stop working on this \
                             ticket until then."
                })
            }
            "submit_review" => {
                let a: ReviewArgs = parse(args)?;
                let approved = a.verdict == ReviewVerdict::Approve;
                self.send(
                    &me,
                    Command::SubmitReview {
                        id: a.id,
                        verdict: a.verdict,
                        body: a.body,
                    },
                )
                .await?;
                json!({
                    "ok": true,
                    "note": if approved {
                        "Approved. Chillax merges the branch into dev."
                    } else {
                        "Sent back to the developer with your notes."
                    }
                })
            }
            "submit_qa" => {
                let a: QaArgs = parse(args)?;
                let verdict = a.verdict;
                self.send(
                    &me,
                    Command::SubmitQaVerdict {
                        id: a.id,
                        verdict,
                        criteria: a.criteria,
                        notes: a.notes,
                        bug: a.bug,
                    },
                )
                .await?;
                json!({
                    "ok": true,
                    "note": match verdict {
                        QaVerdict::Pass => "Signed off. Chillax takes the ticket from here.",
                        QaVerdict::Fail => "Failed. Chillax filed your bug for the developer; \
                                            the ticket is retested once it is fixed.",
                        QaVerdict::Env => "Noted. The first environment problem is retried, \
                                           the second goes to the stakeholder.",
                    }
                })
            }
            "handoff" => {
                let a: HandoffArgs = parse(args)?;
                self.send(
                    &me,
                    Command::Handoff {
                        id: a.id,
                        role: a.role,
                        note: a.note,
                    },
                )
                .await?;
                ok()
            }
            "ask_stakeholder" => {
                let a: QuestionArgs = parse(args)?;
                self.send(
                    &me,
                    Command::AskStakeholder {
                        question: a.question,
                    },
                )
                .await?;
                json!({"ok": true, "note": "Queued in the stakeholder's inbox."})
            }
            "report_progress" => {
                let a: ProgressArgs = parse(args)?;
                self.send(&me, Command::ReportProgress { summary: a.summary })
                    .await?;
                ok()
            }
            "suggest_work" => {
                let a: SuggestArgs = parse(args)?;
                let events = self
                    .company
                    .send(
                        me.clone(),
                        Command::SuggestWork {
                            kind: a.kind,
                            text: a.summary,
                            ticket: a.ticket,
                        },
                    )
                    .await
                    .map_err(|e| ToolError::Rejected(e.to_string()))?;
                let request = events.iter().find_map(|e| match &e.kind {
                    EventKind::RequestSubmitted { id, .. } => Some(id.clone()),
                    _ => None,
                });
                json!({"ok": true, "request": request})
            }
            "message_teammate" => {
                let a: MessageArgs = parse(args)?;
                self.send(
                    &me,
                    Command::SendChat {
                        to: a.employee,
                        text: a.text,
                    },
                )
                .await?;
                ok()
            }
            other => return Err(ToolError::UnknownTool(other.to_string())),
        };
        Ok(value)
    }

    /// An employee by id, then by name, then the least loaded of a role id or tag.
    async fn resolve_employee(&self, who: &str) -> Result<String, ToolError> {
        let state = self.company.snapshot().await;
        let wanted = who.trim().to_lowercase();
        if let Some(e) = state
            .employees
            .iter()
            .find(|e| e.employee.id == wanted || e.employee.name.to_lowercase() == wanted)
        {
            return Ok(e.employee.id.clone());
        }
        state
            .employees
            .iter()
            .filter(|e| e.employee.role == wanted)
            .min_by_key(|e| state.assignments(&e.employee.id).len())
            .map(|e| e.employee.id.clone())
            .ok_or_else(|| ToolError::Rejected(format!("no employee or role `{who}`")))
    }

    /// A request key must name a request that is still waiting for the project manager.
    async fn check_request(&self, id: &str) -> Result<(), ToolError> {
        let state = self.company.snapshot().await;
        match state.request(id) {
            None => Err(ToolError::Rejected(format!("unknown request {id}"))),
            Some(r) if !r.status.is_open() => Err(ToolError::Rejected(format!(
                "request {id} is already {}; do not file more for it",
                r.status.label()
            ))),
            Some(_) => Ok(()),
        }
    }

    /// A triage bug must be a bug for a failure that still needs one.
    async fn check_triage(
        &self,
        fingerprint: &str,
        kind: Option<TicketKind>,
    ) -> Result<(), ToolError> {
        if kind != Some(TicketKind::Bug) {
            return Err(ToolError::Rejected(
                "a fingerprint goes with `kind` bug".into(),
            ));
        }
        let state = self.company.snapshot().await;
        let failure = state
            .failures
            .iter()
            .find(|f| f.fingerprint == fingerprint)
            .ok_or_else(|| ToolError::Rejected(format!("unknown failure {fingerprint}")))?;
        let open = failure
            .bug
            .as_deref()
            .and_then(|b| state.ticket(b))
            .filter(|t| state.workflow_states.last() != Some(&t.state));
        match (failure.status, open) {
            (chillax_core::TriageStatus::Filed, Some(bug)) => Err(ToolError::Rejected(format!(
                "this failure already has an open bug, {}; do not file another",
                bug.id
            ))),
            _ => Ok(()),
        }
    }

    async fn send(&self, actor: &Actor, cmd: Command) -> Result<(), ToolError> {
        self.company
            .send(actor.clone(), cmd)
            .await
            .map(|_| ())
            .map_err(|e| ToolError::Rejected(e.to_string()))
    }
}

#[derive(Deserialize)]
struct ReportAppArgs {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    env: Option<String>,
    #[serde(default)]
    ticket: Option<String>,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    down: bool,
}

#[derive(Deserialize)]
struct HireTeammateArgs {
    role: String,
    reason: String,
    #[serde(default)]
    name: Option<String>,
}

fn rejected(err: chillax_tracker::TrackerError) -> ToolError {
    ToolError::Rejected(err.to_string())
}

fn ok() -> Value {
    json!({"ok": true})
}

fn parse<T: DeserializeOwned>(args: Value) -> Result<T, ToolError> {
    let args = if args.is_null() { json!({}) } else { args };
    serde_json::from_value(args).map_err(|e| ToolError::BadArguments(e.to_string()))
}

fn empty() -> Value {
    object(json!({}), &[])
}

fn string(description: &str) -> Value {
    json!({"type": "string", "description": description})
}

fn object(properties: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": properties, "required": required})
}

#[derive(Deserialize)]
struct IdArgs {
    id: String,
}

#[derive(Deserialize)]
struct StoryArgs {
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    acceptance_criteria: Vec<String>,
    estimate: Option<u32>,
    epic: Option<String>,
    kind: Option<TicketKind>,
    #[serde(default)]
    blocked_by: Vec<String>,
    assignee: Option<String>,
    fingerprint: Option<String>,
    request_id: Option<String>,
}

#[derive(Deserialize)]
struct FinishRequestArgs {
    request_id: String,
    #[serde(default)]
    summary: String,
}

#[derive(Deserialize)]
struct ProblemArgs {
    key: String,
    #[serde(default)]
    summary: String,
}

#[derive(Deserialize)]
struct RestartArgs {
    employee: String,
}

#[derive(Deserialize)]
struct SprintArgs {
    name: String,
    #[serde(default)]
    goal: String,
    tickets: Vec<String>,
}

#[derive(Deserialize)]
struct AssignArgs {
    ticket: String,
    employee: Option<String>,
}

#[derive(Deserialize)]
struct UpdateArgs {
    id: String,
    title: Option<String>,
    description: Option<String>,
    acceptance_criteria: Option<Vec<String>>,
    estimate: Option<u32>,
    blocked_by: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct ReviewRequestArgs {
    id: String,
    #[serde(default)]
    summary: String,
}

#[derive(Deserialize)]
struct ReviewArgs {
    id: String,
    verdict: ReviewVerdict,
    #[serde(default)]
    body: String,
}

#[derive(Deserialize)]
struct QaArgs {
    id: String,
    verdict: QaVerdict,
    #[serde(default)]
    criteria: Vec<CriterionResult>,
    #[serde(default)]
    notes: String,
    #[serde(default)]
    bug: Option<NewBug>,
}

#[derive(Deserialize)]
struct HandoffArgs {
    id: String,
    role: String,
    #[serde(default)]
    note: String,
}

#[derive(Deserialize)]
struct TransitionArgs {
    id: String,
    to: String,
}

#[derive(Deserialize)]
struct CommentArgs {
    id: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    attachments: Vec<String>,
}

#[derive(Deserialize)]
struct QuestionArgs {
    question: String,
}

#[derive(Deserialize)]
struct ProgressArgs {
    summary: String,
}

#[derive(Deserialize)]
struct SuggestArgs {
    kind: SuggestionKind,
    summary: String,
    #[serde(default)]
    ticket: Option<String>,
}

#[derive(Deserialize)]
struct MessageArgs {
    employee: String,
    text: String,
}

#[cfg(test)]
mod role_tool_tests {
    use super::*;
    use chillax_core::Role;

    /// A role file that grants a tool the server does not have fails the agent's first call.
    #[test]
    fn every_tool_a_built_in_role_lists_exists_and_only_the_pm_handles_problems() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../roles");
        let mut roles = 0;
        for entry in std::fs::read_dir(dir).expect("roles dir") {
            let path = entry.expect("entry").path();
            if path.extension().is_none_or(|e| e != "md") {
                continue;
            }
            let role =
                Role::parse(&std::fs::read_to_string(&path).expect("reads")).expect("parses");
            roles += 1;
            for tool in &role.mcp_tools {
                assert!(
                    TOOLS.iter().any(|t| t.name == tool),
                    "{} lists unknown tool {tool}",
                    role.id
                );
            }
            for tool in ["problem_resolved", "retry_action", "restart_teammate"] {
                assert_eq!(
                    role.may_use_tool(tool),
                    role.id == "project-manager",
                    "{} and {tool}",
                    role.id
                );
            }
        }
        assert!(roles >= 4);
    }
}
