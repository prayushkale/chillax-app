//! The event schema. Part of the public `/api/v1` contract (AGENTS.md rule 5): add
//! variants and optional fields freely, never rename or remove them.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::model::*;
use crate::role::RoleSource;

/// Who caused an event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum Actor {
    Stakeholder,
    Employee(EmployeeId),
    System,
}

impl Actor {
    pub fn employee(&self) -> Option<&str> {
        match self {
            Self::Employee(id) => Some(id),
            _ => None,
        }
    }
}

/// A persisted, broadcast fact. `seq` is gap-free and starts at 1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub seq: u64,
    pub ts: DateTime<Utc>,
    pub actor: Actor,
    #[serde(flatten)]
    pub kind: EventKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
// `TicketCreated` carries a whole ticket. Boxing it would change every construction and
// match site for no gain: events are short-lived values.
#[allow(clippy::large_enum_variant)]
pub enum EventKind {
    // Team
    EmployeeHired {
        employee: Employee,
    },
    EmployeeLetGo {
        employee: EmployeeId,
    },
    /// An existing hire now runs on another agent backend, model or effort.
    EmployeeAgentChanged {
        employee: EmployeeId,
        backend: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
    },
    /// The stakeholder asked for another agent while the employee was replying; it is
    /// applied when the turn ends. A newer one replaces an older one.
    EmployeeAgentChangeQueued {
        employee: EmployeeId,
        backend: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
    },
    /// The default agent backend, model and effort for new hires changed.
    HiringDefaultsSet {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        backend: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
    },
    /// A catalog persona became a project role. `markdown` is the full role file, so
    /// the adopted prompt is auditable and survives a deleted file.
    RoleAdopted {
        id: String,
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base_role: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<RoleSource>,
        #[serde(default)]
        revisions: Vec<String>,
        markdown: String,
    },

    // Conversation. The actor is the sender: the stakeholder or a teammate.
    ChatMessageSent {
        to: EmployeeId,
        text: String,
    },
    /// The stakeholder posted to everyone in the company chat. `mentions` are
    /// the employees named with `@`, resolved when the post was decided.
    CompanyChatPosted {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        mentions: Vec<EmployeeId>,
    },

    // Agent runtime observations
    AgentStarted {
        employee: EmployeeId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
    },
    AgentMessageChunk {
        employee: EmployeeId,
        text: String,
    },
    AgentToolCall {
        employee: EmployeeId,
        tool_call_id: String,
        title: String,
        #[serde(default)]
        tool_kind: String,
        #[serde(default)]
        status: String,
    },
    AgentToolCallUpdated {
        employee: EmployeeId,
        tool_call_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
    },
    AgentTurnEnded {
        employee: EmployeeId,
        stop_reason: String,
        /// The ticket whose session ended the turn; none for a chat session.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ticket: Option<TicketId>,
    },
    AgentFailed {
        employee: EmployeeId,
        error: String,
        /// The ticket whose session failed; none for a chat session.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ticket: Option<TicketId>,
    },
    AgentStopped {
        employee: EmployeeId,
    },
    /// The project manager restarted a teammate: their sessions end and a failure clears
    /// (ADR 0032).
    EmployeeRestarted {
        employee: EmployeeId,
    },
    /// The project manager handled a deferred problem (ADR 0032).
    ProblemResolved {
        key: String,
        summary: String,
    },
    /// The project manager asked for a failed action to be tried again now (ADR 0032).
    ActionRetryRequested {
        key: String,
    },
    /// Tokens and cost one turn (or the tail of a session) used. Costs are millionths of
    /// `currency`; no floats are logged (ADR 0023).
    UsageRecorded {
        employee: EmployeeId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ticket: Option<TicketId>,
        #[serde(default)]
        session: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default)]
        input_tokens: u64,
        /// Output plus thought tokens.
        #[serde(default)]
        output_tokens: u64,
        #[serde(default)]
        cached_tokens: u64,
        #[serde(default)]
        cache_write_tokens: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost_micros: Option<u64>,
        #[serde(default = "default_currency", skip_serializing_if = "is_usd")]
        currency: String,
        /// The cost came from the price table, not from the backend.
        #[serde(default)]
        estimated: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_used: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_size: Option<u64>,
    },

    // Inbox
    PermissionRequested {
        id: InboxId,
        employee: EmployeeId,
        title: String,
        choices: Vec<PermissionChoice>,
    },
    PermissionResolved {
        id: InboxId,
        /// `None` means the request was cancelled.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        choice: Option<String>,
        /// True when the role policy decided without asking the stakeholder.
        #[serde(default)]
        automatic: bool,
    },
    QuestionAsked {
        id: InboxId,
        employee: EmployeeId,
        question: String,
    },
    QuestionAnswered {
        id: InboxId,
        answer: String,
    },
    ProgressReported {
        employee: EmployeeId,
        summary: String,
    },
    /// The stakeholder cleared the activity views. A view concern only: the log keeps
    /// every event; activity lists show events after this one.
    ActivityCleared,
    /// The stakeholder emptied the chat with an employee; their chat session ends.
    ChatCleared {
        employee: EmployeeId,
    },

    /// Chillax ended an employee's ticket work (tests passed, review or QA done, merged,
    /// round closed, ticket dropped), so their live transcript is emptied. A view concern:
    /// the log keeps every event, and no session is closed (ADR 0008).
    EmployeeContextCleared {
        employee: EmployeeId,
        ticket: TicketId,
    },

    /// The stakeholder stopped an employee's turn; the session stays alive.
    TurnCancelled {
        employee: EmployeeId,
    },

    // Tracker (local tracker is a projection of these; ADR 0002)
    /// The stakeholder marked a ticket urgent or cleared the mark.
    TicketPrioritized {
        id: TicketId,
        urgent: bool,
    },
    /// The agent limit changed, or the hardware maximum did. Carries the
    /// hardware maximum so replaying the log needs no hardware.
    MaxConcurrentAgentsChanged {
        n: usize,
        hardware_max: usize,
    },
    EpicCreated {
        epic: Epic,
    },
    TicketCreated {
        ticket: Ticket,
    },
    TicketUpdated {
        id: TicketId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        acceptance_criteria: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        estimate: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        epic: Option<EpicId>,
    },
    /// Won't do. The ticket stays in the log but leaves the Board and sprints.
    TicketDropped {
        id: TicketId,
        reason: String,
    },
    TicketAssigned {
        id: TicketId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        assignee: Option<EmployeeId>,
    },
    TicketTransitioned {
        id: TicketId,
        from: String,
        to: String,
    },
    TicketCommented {
        id: TicketId,
        body: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        external_id: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<crate::attachment::Attachment>,
    },
    SprintProposed {
        sprint: Sprint,
        gate: InboxId,
    },
    SprintApproved {
        id: SprintId,
    },
    SprintRejected {
        id: SprintId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// The stakeholder asked for the next sprint to be planned.
    SprintPlanningRequested {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    /// The project manager was briefed about the planning request.
    SprintPlanningBriefed,
    /// The stakeholder described a need.
    RequestSubmitted {
        id: RequestId,
        text: String,
        #[serde(default)]
        start_now: bool,
        /// Set when an employee suggested it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<EmployeeId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<SuggestionKind>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ticket: Option<TicketId>,
    },
    RequestBriefed {
        id: RequestId,
    },
    RequestAbandoned {
        id: RequestId,
    },
    /// A ticket was filed for the request. `sprint` is the active sprint it joined
    /// because the request said to start now.
    RequestTicketLinked {
        request: RequestId,
        ticket: TicketId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sprint: Option<SprintId>,
    },
    RequestTriaged {
        id: RequestId,
        summary: String,
    },
    /// The stakeholder closed a sprint before it finished. `carried` are the unfinished
    /// tickets that left it.
    SprintClosed {
        id: SprintId,
        #[serde(default)]
        carried: Vec<TicketId>,
    },
    TrackerLinked {
        entity: ExternalEntity,
        id: String,
        external: ExternalRef,
    },

    // Dev loop (ADR 0006)
    TicketBlockersSet {
        id: TicketId,
        blocked_by: Vec<TicketId>,
    },
    TicketHandedOff {
        id: TicketId,
        from: EmployeeId,
        to: EmployeeId,
        #[serde(default)]
        note: String,
    },
    /// A developer began a ticket in its own branch and worktree.
    WorkStarted {
        id: TicketId,
        employee: EmployeeId,
        branch: String,
        worktree: String,
    },
    ReviewRequested {
        id: TicketId,
        employee: EmployeeId,
        #[serde(default)]
        summary: String,
    },
    /// Chillax ran the project's tests on the ticket's branch.
    TestsRun {
        id: TicketId,
        passed: bool,
        #[serde(default)]
        skipped: bool,
        #[serde(default)]
        output_tail: String,
    },
    /// The ticket's branch was pushed and a pull request opened for it.
    PrOpened {
        id: TicketId,
        number: u64,
        url: String,
    },
    ReviewStarted {
        id: TicketId,
        reviewer: EmployeeId,
    },
    ReviewSubmitted {
        id: TicketId,
        reviewer: EmployeeId,
        verdict: ReviewVerdict,
        #[serde(default)]
        body: String,
    },
    BranchMerged {
        id: TicketId,
        commit: String,
    },
    MergeFailed {
        id: TicketId,
        reason: String,
    },
    BranchRebased {
        id: TicketId,
        outcome: RebaseOutcome,
        #[serde(default)]
        note: String,
    },
    /// Chillax merged `dev` into the ticket's branch before merging it, or checked the
    /// assignee's conflict resolution.
    BranchSynced {
        id: TicketId,
        outcome: SyncOutcome,
        #[serde(default)]
        files: Vec<String>,
        #[serde(default)]
        note: String,
    },
    /// The assignee was given the turn to resolve the ticket's conflict.
    ConflictBriefed {
        id: TicketId,
    },
    DispatchPaused,
    DispatchResumed,
    /// The stakeholder asked the dispatcher to refresh: close idle sessions, forget
    /// cooldowns and backoff, probe limited backends now. Changes no state.
    DispatchKicked,
    /// The stakeholder asked for fresh usage numbers. The agent runtime flushes every live
    /// session and answers with [`EventKind::UsageRefreshed`]. Changes no state.
    UsageRefreshRequested,
    /// The runtime finished flushing live sessions after a refresh request. Changes no
    /// state; `/usage/refresh` waits for it.
    UsageRefreshed,
    /// A backend's usage limit was hit. The dispatcher starts no work on it until a
    /// probe succeeds (`BackendRecovered`).
    BackendLimited {
        backend: String,
        /// When the agent said the limit resets, if it did.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        until: Option<chrono::DateTime<chrono::Utc>>,
        #[serde(default)]
        message: String,
    },
    BackendRecovered {
        backend: String,
    },

    // QA
    QaPromotionRequested,
    QaRoundStarted {
        round: QaRound,
    },
    EnvCommandRun {
        step: EnvStep,
        ok: bool,
        #[serde(default)]
        skipped: bool,
        #[serde(default)]
        output_tail: String,
        /// The inbox alert raised when the command failed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        alert: Option<InboxId>,
    },
    QaVerdictRecorded {
        id: TicketId,
        employee: EmployeeId,
        verdict: QaVerdict,
        #[serde(default)]
        criteria: Vec<CriterionResult>,
        #[serde(default)]
        notes: String,
        /// The inbox alert raised when QA hit the environment a second time.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        alert: Option<InboxId>,
    },
    /// QA filed `bug` (already created) against `story`; it joins the story's sprint.
    BugFiled {
        story: TicketId,
        bug: TicketId,
    },
    QaRoundClosed {
        number: u32,
    },
    /// The project manager was briefed about a failure.
    TriageBriefed {
        fingerprint: String,
    },
    /// The project manager filed `bug` for a failure. It joins `sprint` (when there is an
    /// active one) as a planned ticket.
    TriageBugFiled {
        fingerprint: String,
        bug: TicketId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sprint: Option<SprintId>,
    },
    /// No bug was filed after repeated briefings; the stakeholder was told.
    TriageAbandoned {
        fingerprint: String,
    },
    AlertRaised {
        id: InboxId,
        title: String,
        #[serde(default)]
        detail: String,
    },
    AlertDismissed {
        id: InboxId,
    },
    EnvRetryRequested {
        env: String,
        step: EnvStep,
    },

    // Running apps (ADR 0015)
    /// The apps Chillax supervises, from `chillax.toml`. Replaces the list.
    AppsConfigured {
        apps: Vec<crate::apps::AppInfo>,
    },
    /// The stakeholder asked to start, stop or restart an app.
    AppRequested {
        env: String,
        want: crate::apps::AppWant,
    },
    /// An employee tells the stakeholder where an app they started runs, or that it stopped
    /// (`down`). Keyed by (employee, env).
    AppReported {
        env: String,
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ticket: Option<TicketId>,
        employee: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        worktree: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        #[serde(default)]
        down: bool,
    },
    AppStarted {
        env: String,
        port: u16,
        url: String,
        commit: String,
    },
    AppStopped {
        env: String,
        #[serde(default)]
        reason: String,
    },
    /// The app's process ended on its own. `will_retry` is false once Chillax gave up.
    AppCrashed {
        env: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit: Option<i32>,
        /// The end of the app's output, capped.
        #[serde(default)]
        tail: String,
        #[serde(default)]
        will_retry: bool,
        #[serde(default)]
        commit: String,
    },
    /// Chillax is restarting the app: the code moved, it was asked to, or it crashed.
    AppRestarted {
        env: String,
        #[serde(default)]
        reason: String,
        #[serde(default)]
        attempt: u32,
    },

    // Release
    ReleaseProposed {
        release: Release,
        gate: InboxId,
    },
    ReleaseApproved {
        id: String,
        version: String,
        notes: String,
    },
    ReleaseRejected {
        id: String,
        #[serde(default)]
        reason: String,
    },
    ReleaseTagged {
        id: String,
        tag: String,
        commit: String,
    },
    DeployRun {
        id: String,
        ok: bool,
        #[serde(default)]
        skipped: bool,
        #[serde(default)]
        output_tail: String,
        /// The inbox alert raised when the deploy failed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        alert: Option<InboxId>,
    },

    // Agent live view (additive detail next to the agent events above)
    AgentPlanUpdated {
        employee: EmployeeId,
        entries: Vec<PlanEntry>,
    },
    AgentToolCallContent {
        employee: EmployeeId,
        tool_call_id: String,
        content: Vec<ToolContent>,
    },
}

fn default_currency() -> String {
    crate::usage::USD.to_string()
}

fn is_usd(currency: &String) -> bool {
    currency == crate::usage::USD
}

impl EventKind {
    /// The snake_case `type` tag, for logs and filters.
    pub fn name(&self) -> &'static str {
        match self {
            Self::EmployeeHired { .. } => "employee_hired",
            Self::EmployeeLetGo { .. } => "employee_let_go",
            Self::EmployeeAgentChanged { .. } => "employee_agent_changed",
            Self::EmployeeAgentChangeQueued { .. } => "employee_agent_change_queued",
            Self::HiringDefaultsSet { .. } => "hiring_defaults_set",
            Self::RoleAdopted { .. } => "role_adopted",
            Self::ChatMessageSent { .. } => "chat_message_sent",
            Self::CompanyChatPosted { .. } => "company_chat_posted",
            Self::AgentStarted { .. } => "agent_started",
            Self::AgentMessageChunk { .. } => "agent_message_chunk",
            Self::AgentToolCall { .. } => "agent_tool_call",
            Self::AgentToolCallUpdated { .. } => "agent_tool_call_updated",
            Self::AgentTurnEnded { .. } => "agent_turn_ended",
            Self::AgentFailed { .. } => "agent_failed",
            Self::AgentStopped { .. } => "agent_stopped",
            Self::EmployeeRestarted { .. } => "employee_restarted",
            Self::ProblemResolved { .. } => "problem_resolved",
            Self::ActionRetryRequested { .. } => "action_retry_requested",
            Self::UsageRecorded { .. } => "usage_recorded",
            Self::PermissionRequested { .. } => "permission_requested",
            Self::PermissionResolved { .. } => "permission_resolved",
            Self::QuestionAsked { .. } => "question_asked",
            Self::QuestionAnswered { .. } => "question_answered",
            Self::ProgressReported { .. } => "progress_reported",
            Self::ActivityCleared => "activity_cleared",
            Self::ChatCleared { .. } => "chat_cleared",
            Self::EmployeeContextCleared { .. } => "employee_context_cleared",
            Self::TurnCancelled { .. } => "turn_cancelled",
            Self::TicketPrioritized { .. } => "ticket_prioritized",
            Self::MaxConcurrentAgentsChanged { .. } => "max_concurrent_agents_changed",
            Self::EpicCreated { .. } => "epic_created",
            Self::TicketCreated { .. } => "ticket_created",
            Self::TicketUpdated { .. } => "ticket_updated",
            Self::TicketDropped { .. } => "ticket_dropped",
            Self::TicketAssigned { .. } => "ticket_assigned",
            Self::TicketTransitioned { .. } => "ticket_transitioned",
            Self::TicketCommented { .. } => "ticket_commented",
            Self::SprintProposed { .. } => "sprint_proposed",
            Self::SprintApproved { .. } => "sprint_approved",
            Self::SprintRejected { .. } => "sprint_rejected",
            Self::SprintPlanningRequested { .. } => "sprint_planning_requested",
            Self::SprintPlanningBriefed => "sprint_planning_briefed",
            Self::SprintClosed { .. } => "sprint_closed",
            Self::RequestSubmitted { .. } => "request_submitted",
            Self::RequestBriefed { .. } => "request_briefed",
            Self::RequestAbandoned { .. } => "request_abandoned",
            Self::RequestTicketLinked { .. } => "request_ticket_linked",
            Self::RequestTriaged { .. } => "request_triaged",
            Self::TrackerLinked { .. } => "tracker_linked",
            Self::TicketBlockersSet { .. } => "ticket_blockers_set",
            Self::TicketHandedOff { .. } => "ticket_handed_off",
            Self::WorkStarted { .. } => "work_started",
            Self::ReviewRequested { .. } => "review_requested",
            Self::TestsRun { .. } => "tests_run",
            Self::PrOpened { .. } => "pr_opened",
            Self::ReviewStarted { .. } => "review_started",
            Self::ReviewSubmitted { .. } => "review_submitted",
            Self::BranchMerged { .. } => "branch_merged",
            Self::MergeFailed { .. } => "merge_failed",
            Self::BranchRebased { .. } => "branch_rebased",
            Self::BranchSynced { .. } => "branch_synced",
            Self::ConflictBriefed { .. } => "conflict_briefed",
            Self::DispatchPaused => "dispatch_paused",
            Self::DispatchResumed => "dispatch_resumed",
            Self::DispatchKicked => "dispatch_kicked",
            Self::UsageRefreshRequested => "usage_refresh_requested",
            Self::UsageRefreshed => "usage_refreshed",
            Self::BackendLimited { .. } => "backend_limited",
            Self::BackendRecovered { .. } => "backend_recovered",
            Self::QaPromotionRequested => "qa_promotion_requested",
            Self::QaRoundStarted { .. } => "qa_round_started",
            Self::EnvCommandRun { .. } => "env_command_run",
            Self::QaVerdictRecorded { .. } => "qa_verdict_recorded",
            Self::BugFiled { .. } => "bug_filed",
            Self::QaRoundClosed { .. } => "qa_round_closed",
            Self::TriageBriefed { .. } => "triage_briefed",
            Self::TriageBugFiled { .. } => "triage_bug_filed",
            Self::TriageAbandoned { .. } => "triage_abandoned",
            Self::AlertRaised { .. } => "alert_raised",
            Self::AlertDismissed { .. } => "alert_dismissed",
            Self::EnvRetryRequested { .. } => "env_retry_requested",
            Self::AppsConfigured { .. } => "apps_configured",
            Self::AppRequested { .. } => "app_requested",
            Self::AppReported { .. } => "app_reported",
            Self::AppStarted { .. } => "app_started",
            Self::AppStopped { .. } => "app_stopped",
            Self::AppCrashed { .. } => "app_crashed",
            Self::AppRestarted { .. } => "app_restarted",
            Self::ReleaseProposed { .. } => "release_proposed",
            Self::ReleaseApproved { .. } => "release_approved",
            Self::ReleaseRejected { .. } => "release_rejected",
            Self::ReleaseTagged { .. } => "release_tagged",
            Self::DeployRun { .. } => "deploy_run",
            Self::AgentPlanUpdated { .. } => "agent_plan_updated",
            Self::AgentToolCallContent { .. } => "agent_tool_call_content",
        }
    }

    /// A one-line human summary for event logs.
    pub fn summary(&self) -> String {
        match self {
            Self::EmployeeHired { employee } => {
                format!("hired {} as {}", employee.name, employee.role_title)
            }
            Self::EmployeeLetGo { employee } => format!("let go {employee}"),
            Self::EmployeeAgentChanged {
                employee,
                backend,
                model,
                effort,
            } => format!(
                "{employee} now runs on {}",
                agent_label(backend, model, effort)
            ),
            Self::EmployeeAgentChangeQueued {
                employee,
                backend,
                model,
                effort,
            } => format!(
                "{employee} will switch to {} after the current turn",
                agent_label(backend, model, effort)
            ),
            Self::HiringDefaultsSet {
                backend,
                model,
                effort,
            } => format!(
                "new hires use {} ({}{})",
                backend.as_deref().unwrap_or("the role's backend"),
                model.as_deref().unwrap_or("default model"),
                effort
                    .as_deref()
                    .map(|e| format!(", {e} effort"))
                    .unwrap_or_default()
            ),
            Self::RoleAdopted {
                title,
                base_role,
                source,
                revisions,
                ..
            } => {
                let from = source
                    .as_ref()
                    .map(|s| format!(" from {}", s.catalog))
                    .unwrap_or_default();
                let on = base_role
                    .as_ref()
                    .map(|b| format!(" on {b}"))
                    .unwrap_or_default();
                let edits = match revisions.len() {
                    0 => String::new(),
                    1 => " (1 revision)".into(),
                    n => format!(" ({n} revisions)"),
                };
                format!("adopted role {title}{from}{on}{edits}")
            }
            Self::ChatMessageSent { to, text } => format!("→ {to}: {}", one_line(text)),
            Self::CompanyChatPosted { text, .. } => format!("→ all: {}", one_line(text)),
            Self::AgentStarted { employee, agent } => match agent {
                Some(agent) => format!("{employee}'s session started ({agent})"),
                None => format!("{employee}'s session started"),
            },
            Self::AgentMessageChunk { employee, text } => format!("{employee}: {}", one_line(text)),
            Self::AgentToolCall {
                employee, title, ..
            } => format!("{employee} ⚙ {title}"),
            Self::AgentToolCallUpdated {
                employee,
                tool_call_id,
                status,
                ..
            } => format!(
                "{employee} ⚙ {tool_call_id} {}",
                status.as_deref().unwrap_or("updated")
            ),
            Self::AgentTurnEnded {
                employee,
                stop_reason,
                ..
            } => format!("{employee} finished ({stop_reason})"),
            Self::AgentFailed {
                employee, error, ..
            } => format!("{employee} failed: {error}"),
            Self::AgentStopped { employee } => format!("{employee}'s session stopped"),
            Self::EmployeeRestarted { employee } => format!("{employee} was restarted"),
            Self::ProblemResolved { key, summary } => format!("handled {key}: {summary}"),
            Self::ActionRetryRequested { key } => format!("retry of {key} requested"),
            Self::UsageRecorded {
                employee,
                input_tokens,
                output_tokens,
                ..
            } => format!(
                "{employee} used {} in / {} out",
                crate::usage::format_tokens(*input_tokens),
                crate::usage::format_tokens(*output_tokens)
            ),
            Self::PermissionRequested {
                id,
                employee,
                title,
                ..
            } => format!("{id} {employee} asks permission: {title}"),
            Self::PermissionResolved {
                id,
                choice,
                automatic,
            } => {
                let by = if *automatic { " by policy" } else { "" };
                match choice {
                    Some(choice) => format!("{id} resolved{by}: {choice}"),
                    None => format!("{id} cancelled"),
                }
            }
            Self::QuestionAsked {
                id,
                employee,
                question,
            } => format!("{id} {employee} asks: {}", one_line(question)),
            Self::QuestionAnswered { id, answer } => format!("{id} answered: {}", one_line(answer)),
            Self::ProgressReported { employee, summary } => {
                format!("{employee} progress: {}", one_line(summary))
            }
            Self::ActivityCleared => "activity cleared".to_string(),
            Self::ChatCleared { employee } => format!("chat with {employee} cleared"),
            Self::EmployeeContextCleared { employee, ticket } if ticket.is_empty() => {
                format!("{employee} is idle; context cleared")
            }
            Self::EmployeeContextCleared { employee, ticket } => {
                format!("{employee} finished {ticket}; context cleared")
            }
            Self::TurnCancelled { employee } => format!("stopped {employee}"),
            Self::TicketPrioritized { id, urgent } => {
                if *urgent {
                    format!("{id} marked urgent")
                } else {
                    format!("{id} no longer urgent")
                }
            }
            Self::MaxConcurrentAgentsChanged { n, hardware_max } => {
                format!("agent limit {n} (this machine fits {hardware_max})")
            }
            Self::EpicCreated { epic } => format!("epic {} {}", epic.id, epic.title),
            Self::TicketCreated { ticket } => format!(
                "{} {} {}",
                ticket.kind.label(),
                ticket.id,
                one_line(&ticket.title)
            ),
            Self::TicketUpdated { id, .. } => format!("{id} updated"),
            Self::TicketDropped { id, reason } => {
                format!("{id} dropped: {}", one_line(reason))
            }
            Self::TicketAssigned { id, assignee } => format!(
                "{id} assigned to {}",
                assignee.as_deref().unwrap_or("nobody")
            ),
            Self::TicketTransitioned { id, from, to } => format!("{id} {from} → {to}"),
            Self::TicketCommented { id, body, .. } => {
                format!("{id} comment: {}", one_line(body))
            }
            Self::SprintProposed { sprint, .. } => format!(
                "sprint {} proposed ({} tickets)",
                sprint.id,
                sprint.tickets.len()
            ),
            Self::SprintApproved { id } => format!("sprint {id} approved"),
            Self::SprintRejected { id, .. } => format!("sprint {id} rejected"),
            Self::SprintPlanningRequested { .. } => "next sprint planning requested".into(),
            Self::SprintPlanningBriefed => "project manager briefed to plan the sprint".into(),
            Self::RequestSubmitted { id, text, .. } => {
                format!("request {id}: {}", one_line(text))
            }
            Self::RequestBriefed { id } => format!("project manager briefed on {id}"),
            Self::RequestAbandoned { id } => format!("request {id} abandoned"),
            Self::RequestTicketLinked {
                request, ticket, ..
            } => format!("{ticket} filed for {request}"),
            Self::RequestTriaged { id, .. } => format!("request {id} triaged"),
            Self::SprintClosed { id, carried } => {
                format!(
                    "sprint {id} closed ({} tickets carried over)",
                    carried.len()
                )
            }
            Self::TrackerLinked { id, external, .. } => format!(
                "{id} linked to {}",
                external.key.as_deref().unwrap_or(&external.id)
            ),
            Self::TicketBlockersSet { id, blocked_by } => {
                if blocked_by.is_empty() {
                    format!("{id} is no longer blocked")
                } else {
                    format!("{id} blocked by {}", blocked_by.join(", "))
                }
            }
            Self::TicketHandedOff { id, from, to, .. } => format!("{id} handed off {from} → {to}"),
            Self::WorkStarted {
                id,
                employee,
                branch,
                ..
            } => format!("{employee} started {id} on {branch}"),
            Self::ReviewRequested {
                id,
                employee,
                summary,
            } => format!("{employee} requests review of {id}: {}", one_line(summary)),
            Self::TestsRun {
                id,
                passed,
                skipped,
                ..
            } => {
                let result = match (skipped, passed) {
                    (true, _) => "skipped (no test command)",
                    (false, true) => "passed",
                    (false, false) => "failed",
                };
                format!("{id} tests {result}")
            }
            Self::PrOpened { id, number, url } => format!("{id} pull request #{number}: {url}"),
            Self::ReviewStarted { id, reviewer } => format!("{id} in review with {reviewer}"),
            Self::ReviewSubmitted {
                id,
                reviewer,
                verdict,
                ..
            } => match verdict {
                ReviewVerdict::Approve => format!("{reviewer} approved {id}"),
                ReviewVerdict::RequestChanges => format!("{reviewer} requested changes on {id}"),
            },
            Self::BranchMerged { id, commit } => {
                format!(
                    "{id} merged into dev ({})",
                    commit.get(..7).unwrap_or(commit)
                )
            }
            Self::MergeFailed { id, reason } => {
                format!("{id} could not merge: {}", one_line(reason))
            }
            Self::BranchRebased { id, outcome, .. } => match outcome {
                RebaseOutcome::Rebased => format!("{id} rebased onto dev"),
                RebaseOutcome::Deferred => format!("{id} rebase deferred"),
                RebaseOutcome::Conflict => format!("{id} rebase conflicted"),
            },
            Self::BranchSynced { id, outcome, .. } => match outcome {
                SyncOutcome::Merged => format!("{id} synced with dev"),
                SyncOutcome::TestsFailed => format!("{id} tests failed after syncing with dev"),
                SyncOutcome::Conflict => format!("{id} conflicts with dev"),
                SyncOutcome::Resolved => format!("{id} conflict resolved"),
                SyncOutcome::Unresolved => format!("{id} conflict still unresolved"),
            },
            Self::ConflictBriefed { id } => format!("{id} conflict handed to its developer"),
            Self::DispatchPaused => "dispatch paused".into(),
            Self::DispatchResumed => "dispatch resumed".into(),
            Self::DispatchKicked => "dispatch refreshed".into(),
            Self::UsageRefreshRequested => "usage refresh requested".into(),
            Self::UsageRefreshed => "usage refreshed".into(),
            Self::BackendLimited {
                backend,
                until,
                message,
            } => match until {
                Some(until) => format!(
                    "{backend} is rate limited until {} ({message})",
                    until.format("%H:%M UTC")
                ),
                None => format!("{backend} is rate limited ({message})"),
            },
            Self::BackendRecovered { backend } => format!("{backend} is available again"),
            Self::QaPromotionRequested => "promotion to qa requested".into(),
            Self::QaRoundStarted { round } => format!(
                "QA round {} started on {} ({} tickets)",
                round.number,
                round.commit.get(..7).unwrap_or(&round.commit),
                round.tickets.len()
            ),
            Self::EnvCommandRun {
                step, ok, skipped, ..
            } => {
                let result = match (skipped, ok) {
                    (true, _) => "skipped (no command)",
                    (false, true) => "ok",
                    (false, false) => "failed",
                };
                format!("qa {step:?} {result}").to_lowercase()
            }
            Self::QaVerdictRecorded {
                id,
                employee,
                verdict,
                ..
            } => match verdict {
                QaVerdict::Pass => format!("{employee} passed {id} in QA"),
                QaVerdict::Fail => format!("{employee} failed {id} in QA"),
                QaVerdict::Env => format!("{employee} hit an environment problem on {id}"),
            },
            Self::BugFiled { story, bug } => format!("bug {bug} filed against {story}"),
            Self::TriageBriefed { fingerprint } => {
                format!("project manager briefed on failure {fingerprint}")
            }
            Self::TriageBugFiled {
                fingerprint, bug, ..
            } => format!("bug {bug} filed for failure {fingerprint}"),
            Self::TriageAbandoned { fingerprint } => {
                format!("no bug was filed for failure {fingerprint}")
            }
            Self::QaRoundClosed { number } => format!("QA round {number} closed"),
            Self::AlertRaised { id, title, .. } => format!("{id} alert: {}", one_line(title)),
            Self::AlertDismissed { id } => format!("{id} dismissed"),
            Self::EnvRetryRequested { env, step } => format!("retry {env} {step:?}").to_lowercase(),
            Self::AppsConfigured { apps } => format!("{} apps configured", apps.len()),
            Self::AppRequested { env, want } => {
                format!("{env} app: {want:?} requested").to_lowercase()
            }
            Self::AppReported {
                env,
                url,
                ticket,
                employee,
                down,
                ..
            } => {
                let ticket = ticket
                    .as_ref()
                    .map(|t| format!(" ({t})"))
                    .unwrap_or_default();
                if *down {
                    format!("{employee} reports {env} stopped{ticket}")
                } else {
                    format!("{employee} reports {env} at {url}{ticket}")
                }
            }
            Self::AppStarted {
                env, url, commit, ..
            } => format!(
                "{env} app running at {url} ({})",
                commit.get(..7).unwrap_or(commit)
            ),
            Self::AppStopped { env, reason } => {
                if reason.is_empty() {
                    format!("{env} app stopped")
                } else {
                    format!("{env} app stopped: {}", one_line(reason))
                }
            }
            Self::AppCrashed {
                env,
                exit,
                will_retry,
                ..
            } => {
                let code = exit.map_or("without an exit code".to_string(), |c| {
                    format!("with exit {c}")
                });
                let next = if *will_retry {
                    "; retrying"
                } else {
                    "; giving up"
                };
                format!("{env} app crashed {code}{next}")
            }
            Self::AppRestarted { env, reason, .. } => {
                format!("{env} app restarting: {}", one_line(reason))
            }
            Self::ReleaseProposed { release, .. } => format!(
                "release {} proposed ({} tickets)",
                release.version,
                release.tickets.len()
            ),
            Self::ReleaseApproved { version, .. } => format!("release {version} approved"),
            Self::ReleaseRejected { id, .. } => format!("release {id} rejected"),
            Self::ReleaseTagged { tag, commit, .. } => {
                format!("released {tag} ({})", commit.get(..7).unwrap_or(commit))
            }
            Self::DeployRun { ok, skipped, .. } => match (skipped, ok) {
                (true, _) => "deploy skipped (no command)".into(),
                (false, true) => "deploy ok".into(),
                (false, false) => "deploy failed".into(),
            },
            Self::AgentPlanUpdated { employee, entries } => {
                format!("{employee} plan: {} steps", entries.len())
            }
            Self::AgentToolCallContent {
                employee,
                tool_call_id,
                ..
            } => format!("{employee} ⚙ {tool_call_id} output"),
        }
    }
}

fn one_line(text: &str) -> String {
    const MAX: usize = 80;
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > MAX {
        let cut: String = line.chars().take(MAX - 1).collect();
        format!("{cut}…")
    } else {
        line
    }
}

/// `claude · opus · high`, leaving out the parts that are not set.
pub fn agent_label(backend: &str, model: &Option<String>, effort: &Option<String>) -> String {
    let mut parts = vec![backend];
    parts.extend(model.as_deref());
    parts.extend(effort.as_deref());
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_json_is_flat_and_tagged() {
        let event = Event {
            seq: 7,
            ts: DateTime::from_timestamp(0, 0).expect("epoch is valid"),
            actor: Actor::Employee("alice".into()),
            kind: EventKind::TicketTransitioned {
                id: "T-1".into(),
                from: "backlog".into(),
                to: "ready".into(),
            },
        };
        let json = serde_json::to_value(&event).expect("serializes");
        assert_eq!(json["type"], "ticket_transitioned");
        assert_eq!(json["actor"]["kind"], "employee");
        assert_eq!(json["actor"]["id"], "alice");
        assert_eq!(json["to"], "ready");
        let back: Event = serde_json::from_value(json).expect("deserializes");
        assert_eq!(back, event);
    }
}
