//! Domain types shared by events, the projection, and the public API.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// Employee slug, e.g. `alice`. Unique within a company.
pub type EmployeeId = String;
/// Ticket key, e.g. `T-3`.
pub type TicketId = String;
/// Epic key, e.g. `EP-1`.
pub type EpicId = String;
/// Sprint key, e.g. `SP-1`.
pub type SprintId = String;
pub type RequestId = String;
/// Inbox item key, e.g. `IN-4`.
pub type InboxId = String;

/// A hired instance of a role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Employee {
    pub id: EmployeeId,
    pub name: String,
    pub role: String,
    pub role_title: String,
    pub backend: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The reasoning effort (thought level) the agent runs at, when one was picked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

impl Employee {
    /// The command that hires another employee just like this one: same role, agent, model
    /// and effort, with the name left for the engine to pick. It keeps the hiring defaults
    /// as they are, since the agent is copied rather than chosen.
    pub fn duplicate_command(&self) -> crate::command::Command {
        crate::command::Command::HireEmployee {
            role: self.role.clone(),
            name: None,
            backend: Some(self.backend.clone()),
            model: self.model.clone(),
            effort: self.effort.clone(),
            keep_defaults: true,
        }
    }
}

/// How many ticket sessions may run at once and the most this machine can take.
/// Both come from events, so replay is deterministic on any machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentLimit {
    pub limit: usize,
    pub hardware_max: usize,
}

/// The coding agent and model new hires (and persona revisions) use unless the
/// stakeholder picks others. Set from Settings, or by hiring with a different pick.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HiringDefaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// Applies only to `backend`: model ids are backend-specific.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Applies only to `backend` and `model`: effort levels differ per model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

/// What an employee's agent session is doing right now. Derived from agent events.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmployeeStatus {
    /// No agent session yet; one starts on the first message.
    #[default]
    Offline,
    Starting,
    Idle,
    Working,
    WaitingOnStakeholder,
    Failed,
}

impl EmployeeStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Offline => "offline",
            Self::Starting => "starting",
            Self::Idle => "idle",
            Self::Working => "working",
            Self::WaitingOnStakeholder => "waiting on you",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TicketKind {
    Story,
    Bug,
    Task,
}

impl TicketKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Story => "story",
            Self::Bug => "bug",
            Self::Task => "task",
        }
    }
}

/// Where an epic, ticket or sprint lives in an external tracker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalRef {
    /// The tracker's own id, e.g. a UUID.
    pub id: String,
    /// Human-readable key, e.g. `SHOP-12`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// What a [`ExternalRef`] is attached to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalEntity {
    Epic,
    Ticket,
    Sprint,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Epic {
    pub id: EpicId,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external: Option<ExternalRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comment {
    pub author: String,
    pub body: String,
    /// Set when the comment was imported from an external tracker, so it is
    /// imported once and never mirrored back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    /// Images on the comment; the bytes are in the attachment store.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<crate::attachment::Attachment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ticket {
    pub id: TicketId,
    pub kind: TicketKind,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimate: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epic: Option<EpicId>,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<EmployeeId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sprint: Option<SprintId>,
    #[serde(default)]
    pub comments: Vec<Comment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external: Option<ExternalRef>,
    /// Tickets that must be merged to dev (or done) before work on this one starts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<TicketId>,
    /// Git branch of the work, set when a developer starts it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Worktree directory of the work, relative to the repository root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    /// The employee reviewing the work while it is in review.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<EmployeeId>,
    /// Facts Chillax has recorded about the work (`tests_pass`, `review_approved`,
    /// `merged_to_dev`, …). Workflow transitions list the facts they `require`.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub checks: BTreeSet<String>,
    /// The branch fell behind `dev` and has not been rebased yet.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub needs_rebase: bool,
    /// The assignee asked for a review and Chillax has not run the tests yet.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub review_requested: bool,
    /// For a bug QA filed: the story it was found in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub found_in: Option<TicketId>,
    /// The pull request of the work, when the project uses GitHub.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    /// Why the stakeholder dropped the ticket (won't do). A dropped ticket stays in the
    /// log and the tracker but leaves the Board, sprints and the dispatcher.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped: Option<String>,
    /// The stakeholder asked for this work to be taken up first. The
    /// dispatcher picks urgent tickets before others; sprint approval and blockers
    /// still apply.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub urgent: bool,
    /// The branch conflicts with `dev` and the assignee is resolving it. An
    /// approval stands meanwhile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict: Option<MergeConflict>,
}

impl Ticket {
    /// Where this ticket's work lives: its recorded worktree under `root`, else the default
    /// `.chillax/worktrees/<id>`. Pure, so dispatch and the CLI agree on one path.
    pub fn worktree_path(&self, root: &std::path::Path) -> std::path::PathBuf {
        match &self.worktree {
            Some(rel) => root.join(rel),
            None => root.join(".chillax/worktrees").join(&self.id),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SprintStatus {
    /// Waiting behind the `sprint_start` gate.
    Proposed,
    Active,
    Rejected,
    /// Its work shipped in a tagged release.
    Released,
    /// The stakeholder closed it before it finished; unfinished tickets left it.
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sprint {
    pub id: SprintId,
    pub name: String,
    #[serde(default)]
    pub goal: String,
    pub tickets: Vec<TicketId>,
    pub status: SprintStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external: Option<ExternalRef>,
}

/// Which tickets the Board shows. Membership comes from `ticket.sprint`, not from
/// `Sprint::tickets`, which can keep stale entries.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SprintView {
    /// The sprint in flight (proposed or running); empty between sprints.
    #[default]
    Active,
    /// A finished sprint (released or closed), read-only.
    Sprint(SprintId),
    /// Tickets with no sprint, read-only.
    Backlog,
}

impl SprintView {
    /// Only the active sprint takes changes from the Board.
    pub fn is_read_only(&self) -> bool {
        !matches!(self, SprintView::Active)
    }
}

/// One choice an agent offers in a permission request (ACP `PermissionOption`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionChoice {
    pub id: String,
    pub name: String,
    /// `allow_once`, `allow_always`, `reject_once`, `reject_always`.
    pub kind: String,
}

/// Something that needs the stakeholder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InboxItem {
    Permission {
        id: InboxId,
        employee: EmployeeId,
        title: String,
        choices: Vec<PermissionChoice>,
    },
    Question {
        id: InboxId,
        employee: EmployeeId,
        question: String,
    },
    SprintGate {
        id: InboxId,
        sprint: SprintId,
    },
    /// An environment command failed; the stakeholder can retry it.
    EnvAlert {
        id: InboxId,
        env: String,
        step: EnvStep,
        output_tail: String,
    },
    /// A release candidate waiting for the stakeholder's approval (`release` gate).
    ReleaseGate {
        id: InboxId,
        release: String,
    },
    /// Something Chillax could not do on its own (a diverged branch, a merge
    /// conflict, a busy checkout) and the stakeholder has to look at.
    Alert {
        id: InboxId,
        title: String,
        detail: String,
    },
}

impl InboxItem {
    pub fn id(&self) -> &str {
        match self {
            Self::Permission { id, .. }
            | Self::Question { id, .. }
            | Self::SprintGate { id, .. }
            | Self::EnvAlert { id, .. }
            | Self::ReleaseGate { id, .. }
            | Self::Alert { id, .. } => id,
        }
    }
}

/// Who wrote a chat line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "name", rename_all = "snake_case")]
pub enum Speaker {
    Stakeholder,
    Agent,
    Teammate(String),
    Tool,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatLine {
    pub speaker: Speaker,
    pub text: String,
    /// Tool call id, so later status updates can find the line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// One message in the company chat feed: every stakeholder and employee
/// message, in the order it was sent. `from` is `None` for the stakeholder; the sender's
/// name is stored so it still shows after that employee is let go.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompanyChatEntry {
    pub seq: u64,
    pub ts: chrono::DateTime<chrono::Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<EmployeeId>,
    pub from_name: String,
    /// `None` is a post to everyone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<EmployeeId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mentions: Vec<EmployeeId>,
    pub text: String,
}

/// A reviewer's decision on a ticket's branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    Approve,
    RequestChanges,
}

/// What happened when Chillax tried to rebase an open branch onto `dev`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RebaseOutcome {
    Rebased,
    /// The agent is mid-turn or the tree is dirty; Chillax tries again later.
    Deferred,
    /// The rebase conflicted and was aborted; the developer gets a turn to merge `dev`
    /// and resolve it.
    Conflict,
}

/// What happened when Chillax merged `dev` into a ticket's branch, or checked an
/// agent's conflict resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncOutcome {
    /// `dev` merged in cleanly and the tests passed on the result.
    Merged,
    /// `dev` merged in cleanly but the tests failed on the result.
    TestsFailed,
    /// Merging `dev` in (or into `dev`) conflicted and was aborted.
    Conflict,
    /// The assignee finished resolving a conflict; the tests run next.
    Resolved,
    /// The assignee's turn ended with the conflict still unresolved.
    Unresolved,
}

/// A ticket whose branch conflicts with `dev` and is being resolved by its assignee.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MergeConflict {
    /// The conflicted paths, when git named them.
    #[serde(default)]
    pub files: Vec<String>,
    /// Conflicts recorded so far; past [`crate::engine::MAX_CONFLICT_ATTEMPTS`] the ticket
    /// is triaged instead.
    pub attempts: u32,
    /// The assignee was given the resolve turn for the latest conflict.
    #[serde(default)]
    pub briefed: bool,
}

/// One step of an agent's plan (ACP plan entry).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanEntry {
    pub content: String,
    /// `pending`, `in_progress` or `completed`.
    pub status: String,
}

/// Detail an agent's tool call produced, as ACP reports it. Capped before it is stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolContent {
    Diff {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        old: Option<String>,
        new: String,
    },
    Text {
        text: String,
    },
    Terminal {
        output: String,
    },
}

/// A tool call as the live view shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveTool {
    pub tool_call_id: String,
    pub title: String,
    /// ACP tool kind (`read`, `edit`, `execute`, `search`, `fetch`, `think`, `other`, …).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tool_kind: String,
    #[serde(default)]
    pub status: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<ToolContent>,
}

/// What one employee's agent is doing right now (the Agent live view).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveView {
    #[serde(default)]
    pub plan: Vec<PlanEntry>,
    /// The most recent tool calls, oldest first.
    #[serde(default)]
    pub tools: Vec<LiveTool>,
}

impl LiveView {
    /// Drop the oldest calls' output until the rest fits `LIVE_CONTENT_BYTES`.
    pub fn trim_content(&mut self) {
        let size = |t: &LiveTool| {
            t.content
                .iter()
                .map(|c| match c {
                    ToolContent::Diff { path, old, new } => {
                        path.len() + old.as_deref().map_or(0, str::len) + new.len()
                    }
                    ToolContent::Text { text } | ToolContent::Terminal { output: text } => {
                        text.len()
                    }
                })
                .sum::<usize>()
        };
        let mut total: usize = self.tools.iter().map(size).sum();
        for tool in &mut self.tools {
            if total <= LIVE_CONTENT_BYTES {
                break;
            }
            total -= size(tool);
            tool.content.clear();
        }
    }
}

/// How many tool calls the live view keeps per employee.
pub const LIVE_TOOLS_KEPT: usize = 200;

/// Bytes of tool output kept per employee. Past this the oldest calls lose their output
/// (the call itself stays), so memory per employee is bounded by this plus the call list.
pub const LIVE_CONTENT_BYTES: usize = 512 * 1024;

/// One command of an environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvStep {
    Up,
    Test,
    Down,
    Deploy,
}

/// QA's decision on one ticket of a round.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QaVerdict {
    Pass,
    /// A product failure: a bug is filed.
    Fail,
    /// The environment failed, not the product. Retried once, then escalated.
    Env,
}

/// What QA found for one acceptance criterion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CriterionResult {
    pub criterion: String,
    pub passed: bool,
    /// What QA ran or saw. Required, so a verdict cannot be a bare "looks fine".
    pub evidence: String,
}

/// The bug QA files with a failing verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewBug {
    pub title: String,
    /// Steps to reproduce, expected and actual.
    pub description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QaRoundStatus {
    /// `dev` is promoted; the environment is coming up and being tested.
    Starting,
    /// Agents are testing tickets.
    Testing,
    /// `up` or `test` failed (or QA hit the environment twice); waiting for a retry.
    EnvFailed,
    Closed,
}

/// One pass of QA over a snapshot of `dev`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QaRound {
    pub number: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sprint: Option<SprintId>,
    /// The `dev` commit that `qa` was advanced to.
    pub commit: String,
    pub status: QaRoundStatus,
    /// Tickets under test in this round.
    pub tickets: Vec<TicketId>,
    #[serde(default)]
    pub verdicts: BTreeMap<TicketId, QaVerdict>,
    /// Environment verdicts per ticket so far.
    #[serde(default)]
    pub env_retries: BTreeMap<TicketId, u32>,
    /// `up` succeeded (or was skipped), so a retry resumes at `test`.
    #[serde(default)]
    pub up_done: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseStatus {
    /// Waiting behind the `release` gate.
    Gated,
    /// Approved; Chillax is merging and tagging.
    Approved,
    Tagged,
    /// Tagged and the deploy (if any) ran.
    Deployed,
    /// Tagged, but the deploy failed. The release stands; the deploy can be retried.
    DeployFailed,
    Rejected,
}

/// A release: the work QA signed off since the last one, a version and notes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    pub id: String,
    /// `v1.2.3`; the stakeholder can change it at the gate.
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sprint: Option<SprintId>,
    /// The QA round that produced it.
    pub round: u32,
    /// Tickets that ship in it.
    pub tickets: Vec<TicketId>,
    /// Tickets that failed QA but ship anyway because the stakeholder forced the gate.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub known_issues: Vec<TicketId>,
    pub notes: String,
    pub status: ReleaseStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

/// A retry the stakeholder asked for and the dispatcher has not run yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvRetry {
    /// `qa` or `prod`.
    pub env: String,
    pub step: EnvStep,
}

/// A backend whose usage limit was hit and has not been seen lifted yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendLimit {
    /// When the agent said the limit resets, if it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub message: String,
}

/// The stakeholder asked for the next sprint to be planned.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanningRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The project manager has been briefed.
    #[serde(default)]
    pub briefed: bool,
}

/// Where a stakeholder request stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestStatus {
    /// Waiting for a project manager who is free and whose agent works.
    Submitted,
    /// The project manager was briefed and has not finished yet.
    Briefed,
    /// The project manager filed what was needed (possibly nothing) and said so.
    Triaged,
    /// Briefed too often without an answer; the stakeholder was alerted.
    Abandoned,
}

impl RequestStatus {
    /// Still waiting for the project manager.
    pub fn is_open(self) -> bool {
        matches!(self, Self::Submitted | Self::Briefed)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Submitted => "queued",
            Self::Briefed => "with the PM",
            Self::Triaged => "triaged",
            Self::Abandoned => "abandoned",
        }
    }
}

/// What an employee noticed and handed to the project manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuggestionKind {
    Bug,
    Improvement,
}

impl SuggestionKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Bug => "bug",
            Self::Improvement => "improvement",
        }
    }
}

/// Something the stakeholder asked for in their own words: the project manager turns it
/// into work items and assigns them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub id: RequestId,
    pub text: String,
    /// The stakeholder wants the work to start now: filing counts as approval of the
    /// `sprint_start` gate for the tickets it produces.
    #[serde(default)]
    pub start_now: bool,
    pub status: RequestStatus,
    /// Times the project manager was briefed.
    #[serde(default)]
    pub attempts: u32,
    /// Tickets filed for it.
    #[serde(default)]
    pub tickets: Vec<TicketId>,
    /// The project manager's closing summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// The employee who suggested it; `None` for the stakeholder's own requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<EmployeeId>,
    /// Set for an employee's suggestion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<SuggestionKind>,
    /// The ticket the suggester was working on when they noticed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<TicketId>,
}

#[cfg(test)]
mod worktree_path_tests {
    use super::*;
    use std::path::Path;

    fn ticket(worktree: Option<&str>) -> Ticket {
        let mut json = serde_json::json!({
            "id": "T-7", "kind": "task", "title": "x", "state": "In Progress"
        });
        if let Some(w) = worktree {
            json["worktree"] = w.into();
        }
        serde_json::from_value(json).expect("ticket")
    }

    #[test]
    fn recorded_worktree_is_joined_to_the_root() {
        let t = ticket(Some("custom/wt"));
        assert_eq!(
            t.worktree_path(Path::new("/repo")),
            Path::new("/repo/custom/wt")
        );
    }

    #[test]
    fn without_one_it_defaults_to_the_ticket_id() {
        assert_eq!(
            ticket(None).worktree_path(Path::new("/repo")),
            Path::new("/repo/.chillax/worktrees/T-7")
        );
    }
}

#[cfg(test)]
mod duplicate_tests {
    use super::*;
    use crate::command::Command;

    #[test]
    fn duplicate_copies_the_agent_and_leaves_the_name_to_the_engine() {
        let e = Employee {
            id: "ada".into(),
            name: "Ada".into(),
            role: "dev".into(),
            role_title: "Dev".into(),
            backend: "codex".into(),
            model: Some("gpt".into()),
            effort: None,
        };
        assert_eq!(
            e.duplicate_command(),
            Command::HireEmployee {
                role: "dev".into(),
                name: None,
                backend: Some("codex".into()),
                model: Some("gpt".into()),
                effort: None,
                keep_defaults: true,
            }
        );
    }
}
