# ADR 0008: Agent runtime robustness

Status: accepted (2026-10-23).

## Context

A long unattended run showed four problems: finished sessions never closed, so chat and
idle ticket sessions used up `max_concurrent_agents`; a failed session was reopened at
once (365 restarts in 12 minutes after a usage limit); a ticket agent that ended its turn
without calling a tool stalled the ticket forever; and permission prompts blocked
unattended work. A stakeholder chat message also landed inside a ticket session.

## Decision

- Slots: only ticket sessions count towards `max_concurrent_agents`. Chats have
  `max_chat` (default 3, LRU idle chat evicted) and `chat_idle` (10 min).
- Every ticket, rework, review and QA round is a new session with a recap briefing.
  Sessions close at hand-over (tests pass, review submitted, QA verdict).
- Stuck turns: nudge once, close and alert after the second idle end
  (`core::dispatch::session_is_waiting` decides, after `idle_grace`).
- Backoff: per ticket 30 s doubling to 15 min after `AgentFailed` (which now carries the
  ticket). `auth_required` halts the backend and raises one alert.
- Usage limits are state: `BackendLimited` / `BackendRecovered` events and
  `State.backend_limits`. `core::limits` classifies errors and parses reset times
  (relative, epoch, RFC 3339, clock time in local time). A throwaway probe session through
  the PM (no slot, no events) tests the backend on a jittered schedule; the jitter source
  is injectable.
- `[agents] permissions = "auto" | "policy"` (default `auto`). `auto` approves every
  request and sets the agent's bypass mode. `config.agents` became `AgentsConfig`, which
  derefs to the backend map, so existing configs still parse.
- `Kick` command and `DispatchKicked` event refresh the dispatcher.

All events and commands are additive.

## Consequences

- Under `auto`, the worktree, reviewer read-only and QA read-only rules are prompt-level
  only. Choose `policy` to keep them enforced.
- Reset times without a zone are read in the machine's local zone; a wrong guess only
  moves the first probe.
- A rate-limited backend with no employee to probe is treated as recovered.

## Amendment (2026-10-24): fresh PM per briefing, configurable idle close

- The PM's chat session closes once a Chillax briefing is done, on `SprintProposed`,
  `TriageBugFiled` and `RequestTriaged` recorded by an employee. The runtime marks the
  session and the chat sweeper closes it when no turn is running, so the PM's own turn is
  never cut off. A new turn clears the mark.
- `[agents] chat_idle_minutes` (default 10) replaces the fixed idle timeout.
- Cheaper models for QA, reviewer and PM are a data choice (role `model:` or the hire pick).

## Amendment (2026-10-25): close stale ticket sessions before a new ticket

- When the dispatcher starts an employee on a ticket (work, review or QA), it first closes
  that employee's other idle ticket sessions (`AgentRuntime::close_other_ticket_sessions`).
  A session mid-turn, one whose ticket still waits on the employee (`session_is_waiting`,
  so parallel QA keeps working) and the chat session are kept. This frees
  `max_concurrent_agents` slots and stops old context leaking into the new ticket.
- No new events: the close records the usual `AgentStopped`.

## Amendment (2026-10-27): clear the transcript when the work is done

- The Live, Chat and web views read one per-employee buffer (`chats[employee]` joined
  with `live[employee].tools`). When an employee's ticket work ends (tests passed or
  skipped, review or QA verdict submitted, merged, round closed, ticket dropped), the
  dispatcher closes the session and then sends the system-only `ClearEmployeeContext`,
  which emits `EmployeeContextCleared { employee, ticket }`. Apply empties the chat log
  and the live tool calls and clears `reply_pending`; the plan stays and no session is
  closed (unlike `ChatCleared`, which ends the chat session). History stays in the
  event log and the Activity views.
- Guards: only when `close` really ended a ticket session, and not while the employee
  still has another ticket session. The PM and chat sessions are never cleared this way.
- Late chunks: `close` cancels the turn and waits for the session task to end, and the
  clear is sent after it returns, so no chunk or tool call of that session follows it.
  This is simpler than filtering chunks in `apply` or deferring to `AgentTurnEnded`,
  and replay is deterministic because the clear is an ordinary logged event.
- A developer's chat with the stakeholder shares the buffer, so it is cleared too (the
  stakeholder's decision). An open chat turn is not checked: after `close` the ticket
  turn's open flag is stale, so such a guard would always skip.
- Views: an empty transcript for an idle employee who has reported progress reads
  "Context cleared — idle".

## Amendment (2026-10-27): wrap up chat sessions too

- Clearing only after a ticket session left chat sessions (teammate messages, check-ins,
  briefings) open for `chat_idle` with their context, so the stakeholder cleared chats by hand.
- `finish_session` now also marks the employee's chat session (`start_fresh_next`). The
  sweeper closes a marked session once it is idle with no queued prompt, and, when the
  employee then has no session left, sends `ClearEmployeeContext` (empty ticket). This
  covers PM briefings too. A chat with a live ticket session keeps its shared transcript.
