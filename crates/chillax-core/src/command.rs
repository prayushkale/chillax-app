//! Commands: requests to change company state. Frontends (stakeholder) and Chillax MCP
//! tools (employees) send these; the [`Engine`](crate::Engine) accepts or rejects them.

use serde::{Deserialize, Serialize};

use crate::model::*;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    HireEmployee {
        role: String,
        /// Absent or blank: the engine picks a famous tech name nobody on the team has.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        backend: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
        /// Set by a duplicate: the agent pick is copied from an employee, so it must not
        /// become the hiring default.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        keep_defaults: bool,
    },
    /// An employee in a hiring role (`[team] hiring_roles`) adds a teammate directly. No
    /// approval; the stakeholder gets an inbox notice. Capped by `max_team_size`.
    HireTeammate {
        role: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        reason: String,
    },
    /// An employee tells the stakeholder where an app they started runs (a dev server in
    /// their worktree), or that it stopped (`down`). Local URLs only.
    ReportApp {
        /// Free-form label; defaults to the ticket id, else `dev`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<String>,
        /// Exactly one of `url` and `port`, unless `down`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ticket: Option<TicketId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        #[serde(default)]
        down: bool,
    },
    LetGo {
        employee: EmployeeId,
    },
    /// Change the agent an existing hire runs on. A blank backend keeps the current one;
    /// model and effort are replaced (blank = the backend's default). Refused while the
    /// employee is replying, unless `when_idle` asks for the change to wait for the end of
    /// the turn; their sessions end and start again on the new agent.
    SetEmployeeAgent {
        employee: EmployeeId,
        /// Queue the change if the employee is replying instead of refusing it.
        #[serde(default)]
        when_idle: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        backend: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
    },
    /// Chillax applies the agent change the stakeholder queued once the employee's turn
    /// has ended. System-only; nothing happens without a queued change.
    ApplyQueuedAgentChange {
        employee: EmployeeId,
    },
    /// Change the default agent backend, model and effort for new hires and persona revisions.
    SetHiringDefaults {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        backend: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
    },
    /// Add a role adopted from a persona catalog. `markdown` is the full
    /// role file; recruiters write it to `.chillax/roles/` after this is accepted.
    AdoptRole {
        markdown: String,
        /// The revision instructions the stakeholder gave, for the audit trail.
        #[serde(default)]
        revisions: Vec<String>,
        /// Replace an earlier adopted role with the same id.
        #[serde(default)]
        overwrite: bool,
    },
    SendChat {
        to: EmployeeId,
        text: String,
    },
    /// The stakeholder posts in the company chat. With `to` it is a normal chat
    /// message; without, it goes to everyone and only `@mentioned` employees are woken.
    PostCompanyChat {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<EmployeeId>,
    },
    ResolvePermission {
        id: InboxId,
        /// `None` cancels the request.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        choice: Option<String>,
    },
    AskStakeholder {
        question: String,
    },
    AnswerQuestion {
        id: InboxId,
        answer: String,
    },
    ReportProgress {
        summary: String,
    },
    /// Hide everything so far from the activity views. The event log is untouched.
    ClearActivity,
    /// Empty the chat with one employee and end their chat session, so the next message
    /// starts fresh. Ticket sessions are untouched. Refused while they are replying.
    ClearChat {
        employee: EmployeeId,
    },
    /// Chillax only: an employee's ticket work is done, so empty their live transcript
    /// (chat log and tool calls). The event log keeps the history; chat sessions are
    /// untouched (ADR 0008).
    ClearEmployeeContext {
        employee: EmployeeId,
        ticket: TicketId,
    },
    /// Stop an employee's current turn but keep their session and context, so the
    /// stakeholder can give more input. Stakeholder-only; a no-op when nothing is running.
    CancelTurn {
        employee: EmployeeId,
    },
    /// Mark a work item urgent (or clear it) so the dispatcher takes it up first.
    /// Stakeholder-only; a no-op when the flag already has that value.
    PrioritizeTicket {
        id: TicketId,
        urgent: bool,
    },
    /// Change `[budget] max_concurrent_agents` live. Stakeholder-only; refused
    /// outside `1..=hardware max`; a no-op when the limit already is `n`.
    SetMaxConcurrentAgents {
        n: usize,
    },
    CreateEpic {
        title: String,
        #[serde(default)]
        description: String,
    },
    CreateTicket {
        kind: TicketKind,
        title: String,
        #[serde(default)]
        description: String,
        #[serde(default)]
        acceptance_criteria: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        estimate: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        epic: Option<EpicId>,
        /// Set when the ticket is imported from an external tracker, so it is born
        /// linked and never mirrored back as a new item.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        external: Option<ExternalRef>,
    },
    UpdateTicket {
        id: TicketId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        acceptance_criteria: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        estimate: Option<u32>,
        /// Move the ticket to this epic.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        epic: Option<EpicId>,
    },
    /// Won't do: take the ticket off the Board and out of its sprint, unassign it and
    /// close its sessions. Refused once its code is in `dev`.
    DropTicket {
        id: TicketId,
        reason: String,
    },
    AssignTicket {
        id: TicketId,
        /// `None` unassigns.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        assignee: Option<EmployeeId>,
    },
    TransitionTicket {
        id: TicketId,
        to: String,
    },
    CommentTicket {
        id: TicketId,
        body: String,
        /// The external tracker's comment id when the comment is imported from it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        external_id: Option<String>,
        /// Images stored through the attachment store.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<crate::attachment::Attachment>,
    },
    ProposeSprint {
        name: String,
        #[serde(default)]
        goal: String,
        tickets: Vec<TicketId>,
    },
    ApproveSprint {
        id: SprintId,
    },
    RejectSprint {
        id: SprintId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Ask the project manager to propose the next sprint from the backlog and the
    /// leftovers. Refused while a sprint is proposed or active.
    RequestSprintPlanning {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    /// The project manager was briefed about the planning request. Sent by the dispatcher.
    RecordPlanningBriefed,
    /// The stakeholder describes a need in their own words; the project manager files
    /// and assigns the work. `start_now` counts as approval of the sprint gate
    /// for the tickets it produces.
    SubmitRequest {
        text: String,
        #[serde(default)]
        start_now: bool,
    },
    /// An employee noticed a bug or an improvement outside their ticket and hands it to
    /// the project manager as a request.
    SuggestWork {
        kind: SuggestionKind,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ticket: Option<TicketId>,
    },
    /// The project manager was briefed about a request. Sent by the dispatcher.
    RecordRequestBriefed {
        id: RequestId,
    },
    /// Give up on a request the project manager never answered. Sent by the dispatcher.
    AbandonRequest {
        id: RequestId,
    },
    /// Say that `ticket` was filed for `request` (sent by `create_story` with a `request_id`).
    LinkRequestTicket {
        request: RequestId,
        ticket: TicketId,
    },
    /// The project manager is done with a request, with a summary of who got what.
    CompleteRequest {
        id: RequestId,
        summary: String,
    },
    /// The project manager handled a problem Chillax briefed them about (ADR 0032). The
    /// stakeholder gets a dismissable note with the summary.
    ResolveProblem {
        key: String,
        summary: String,
    },
    /// The project manager asks Chillax to try a failed action again now, skipping its
    /// wait (ADR 0032). `key` is the problem key from the briefing.
    RetryAction {
        key: String,
    },
    /// The project manager restarts a teammate whose agent session failed: the session
    /// closes and the failure clears (ADR 0032).
    RestartEmployee {
        employee: EmployeeId,
    },
    /// Close an active sprint that will not finish: unfinished tickets leave it and can
    /// be planned again.
    CloseSprint {
        id: SprintId,
    },
    /// Set which tickets must be merged (or done) before this one can start.
    SetBlockers {
        id: TicketId,
        blocked_by: Vec<TicketId>,
    },
    /// Hand a ticket to another role; it goes to that role's least-loaded employee.
    Handoff {
        id: TicketId,
        role: String,
        #[serde(default)]
        note: String,
    },
    /// Start building a ready ticket in a worktree. Sent by the dispatcher (actor
    /// `System`) once it has created `branch` at `worktree`.
    StartWork {
        id: TicketId,
        branch: String,
        worktree: String,
    },
    /// The assignee says the work is done and ready for the tests and a review.
    RequestReview {
        id: TicketId,
        #[serde(default)]
        summary: String,
    },
    /// Chillax ran the project's tests on a ticket's branch. Sent by the dispatcher.
    RecordTests {
        id: TicketId,
        passed: bool,
        /// No test command is configured, so nothing ran.
        #[serde(default)]
        skipped: bool,
        #[serde(default)]
        output_tail: String,
    },
    /// Chillax pushed a ticket's branch and opened a pull request. Sent by the dispatcher.
    RecordPr {
        id: TicketId,
        number: u64,
        url: String,
    },
    /// Hand a ticket whose tests passed to a reviewer. Sent by the dispatcher.
    StartReview {
        id: TicketId,
        reviewer: EmployeeId,
    },
    /// The reviewer approves the branch or asks for changes.
    SubmitReview {
        id: TicketId,
        verdict: ReviewVerdict,
        #[serde(default)]
        body: String,
    },
    /// The branch was merged into `dev`. Sent by the dispatcher.
    RecordMerge {
        id: TicketId,
        commit: String,
    },
    /// Merging into `dev` conflicted or failed and was aborted. Sent by the dispatcher.
    RecordMergeFailed {
        id: TicketId,
        reason: String,
    },
    /// Result of rebasing an open branch onto `dev`. Sent by the dispatcher.
    RecordRebase {
        id: TicketId,
        outcome: RebaseOutcome,
        #[serde(default)]
        note: String,
    },
    /// Result of merging `dev` into a ticket's branch, or of checking the assignee's
    /// conflict resolution. Sent by the dispatcher.
    RecordSync {
        id: TicketId,
        outcome: SyncOutcome,
        #[serde(default)]
        files: Vec<String>,
        #[serde(default)]
        note: String,
    },
    /// The assignee was given the turn to resolve the ticket's conflict. Sent by the
    /// dispatcher.
    RecordConflictBriefed {
        id: TicketId,
    },
    /// Ask for `dev` to be promoted to `qa` now, instead of waiting for the sprint to
    /// finish. Sent by the stakeholder or the dispatcher.
    PromoteToQa,
    /// `qa` was advanced to `commit`; a QA round starts over every ticket waiting in
    /// `in_qa`. Sent by the dispatcher.
    RecordPromotion {
        commit: String,
    },
    /// One of the QA environment's commands ran. Sent by the dispatcher.
    RecordEnvRun {
        step: EnvStep,
        ok: bool,
        /// No command is configured, so nothing ran.
        #[serde(default)]
        skipped: bool,
        #[serde(default)]
        output_tail: String,
    },
    /// Every ticket of the testing round has a verdict (and `down` ran): close it. A
    /// round with no failures proposes a release. Sent by the dispatcher.
    CloseQaRound,
    /// Run a failed environment step again (`qa` up/test, `prod` deploy).
    RetryEnv {
        env: String,
        step: EnvStep,
    },
    /// Start the app of `env` (`dev`, `qa` or `prod`) after it was stopped or gave up.
    StartApp {
        env: String,
    },
    StopApp {
        env: String,
    },
    /// Restart the app of `env` on the code it should be running.
    RestartApp {
        env: String,
    },
    /// The apps in `chillax.toml`. Sent by Chillax at start; replaces what was known.
    ConfigureApps {
        apps: Vec<crate::apps::AppInfo>,
    },
    /// An app was started on `commit`. Sent by the dispatcher.
    RecordAppStarted {
        env: String,
        port: u16,
        url: String,
        commit: String,
    },
    RecordAppStopped {
        env: String,
        #[serde(default)]
        reason: String,
    },
    RecordAppCrashed {
        env: String,
        #[serde(default)]
        exit: Option<i32>,
        #[serde(default)]
        tail: String,
        #[serde(default)]
        will_retry: bool,
        #[serde(default)]
        commit: String,
    },
    RecordAppRestarted {
        env: String,
        #[serde(default)]
        reason: String,
        #[serde(default)]
        attempt: u32,
    },
    /// Open the `release` gate on the work QA has signed off. `force` ships it with
    /// tickets that failed QA, listed as known issues.
    OpenReleaseGate {
        #[serde(default)]
        force: bool,
    },
    /// Approve a release at the gate, with the (possibly edited) version and notes.
    ApproveRelease {
        id: String,
        version: String,
        notes: String,
    },
    RejectRelease {
        id: String,
        #[serde(default)]
        reason: String,
    },
    /// `qa` was merged into the prod branch and tagged. Sent by the dispatcher.
    RecordRelease {
        id: String,
        tag: String,
        commit: String,
    },
    /// The prod `deploy` command ran. Sent by the dispatcher.
    RecordDeploy {
        id: String,
        ok: bool,
        #[serde(default)]
        skipped: bool,
        #[serde(default)]
        output_tail: String,
    },
    /// QA's verdict on a ticket in the open round.
    SubmitQaVerdict {
        id: TicketId,
        verdict: QaVerdict,
        #[serde(default)]
        criteria: Vec<CriterionResult>,
        #[serde(default)]
        notes: String,
        /// Required with a failing verdict.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bug: Option<NewBug>,
    },
    /// Tell the stakeholder about a problem Chillax cannot solve alone. Sent by the
    /// dispatcher; the same title and detail twice make one alert.
    RaiseAlert {
        title: String,
        #[serde(default)]
        detail: String,
    },
    /// Remove an alert from the inbox: the stakeholder has seen it, or Chillax found
    /// the problem gone.
    DismissAlert {
        id: InboxId,
    },
    /// The project manager was briefed about a failure. Sent by the dispatcher.
    RecordTriageBriefed {
        fingerprint: String,
    },
    /// File `bug` (already created) as the bug for the failure with `fingerprint`. Sent by
    /// the project manager's `create_story`; refused when the failure already has an open bug.
    LinkTriageBug {
        fingerprint: String,
        bug: TicketId,
    },
    /// Give up on getting a bug for a failure. Sent by the dispatcher.
    AbandonTriage {
        fingerprint: String,
    },
    /// Stop starting new work and stop running ticket sessions.
    PauseDispatch,
    ResumeDispatch,
    /// Refresh the dispatcher: close idle sessions, clear cooldowns and backoff, and
    /// look again now. Changes no state.
    Kick,
    /// Write the usage live sessions have not recorded yet, so the Usage view is current
    /// without waiting for a turn to end. Changes no state beyond the samples it adds.
    RefreshUsage,
    /// A backend hit its usage limit. Sent by the dispatcher (actor `System`).
    RecordBackendLimit {
        backend: String,
        #[serde(default)]
        until: Option<chrono::DateTime<chrono::Utc>>,
        #[serde(default)]
        message: String,
    },
    /// A probe got through: the backend is usable again. Sent by the dispatcher.
    RecordBackendRecovered {
        backend: String,
    },
    /// Record where an epic, ticket or sprint lives in the external tracker. Sent by
    /// tracker sync (actor `System`), never by agents.
    LinkExternal {
        entity: ExternalEntity,
        id: String,
        external: ExternalRef,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hire_teammate_round_trips_with_and_without_a_name() {
        let named = Command::HireTeammate {
            role: "dev".into(),
            name: Some("Dana".into()),
            reason: "T-1".into(),
        };
        let json = serde_json::to_string(&named).expect("serializes");
        assert_eq!(
            serde_json::from_str::<Command>(&json).expect("reads"),
            named
        );
        let bare: Command =
            serde_json::from_str(r#"{"type":"hire_teammate","role":"dev","reason":"T-1"}"#)
                .expect("reads");
        assert_eq!(
            bare,
            Command::HireTeammate {
                role: "dev".into(),
                name: None,
                reason: "T-1".into()
            }
        );
        assert!(
            !serde_json::to_string(&bare)
                .expect("serializes")
                .contains("name")
        );
    }
}
