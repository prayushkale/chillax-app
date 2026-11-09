# ADR 0032: Problems go to the project manager first

- Status: Accepted
- Date: 2026-10-26
- Builds on: ADR 0008 (agent runtime)

## Context

The stakeholder wants the flow to keep running without input. A crashed teammate, an
agent that stops without finishing, a failing dispatcher action or an app that keeps
crashing each raised an Inbox alert and waited for a person, although the project
manager can often fix them (retry, restart, reassign, file a bug).

## Decision

**Two categories.**

| Problem | Category | Handling |
| --- | --- | --- |
| A non-ticket dispatcher action fails (`promote`, closing a QA round, an app start) | 1 | PM first |
| A teammate's agent crashes (`AgentFailed`, not sign-in or limit) | 1 | PM first (it raised no alert before) |
| An agent ends two turns without moving its ticket | 1 | PM first |
| An app keeps crashing (`app_gave_up`) | 1 | PM first |
| Sign-in, billing and usage limits, gates, permission requests, credentials | 2 | alert at once |
| Release, deploy, back-merge and `gh` problems, a checkout behind the remote | 2 | alert at once |
| Failures already triaged (failure triage), requests, planning | - | unchanged |

Category 2 is never deferred. A failing action whose error classifies as sign-in or
limit (`limits::classify_failure`) alerts at once even when its key is deferrable. A
"dirty or behind checkout" is only raised on the release path (`sync_local_branch`),
so it stays category 2.

**Deferral.** `alert_or_defer` records the problem in the runner (`Book.problems`)
instead of raising the alert when `[team] problem_grace_minutes > 0` and a project
manager is available (`core::dispatch::pm_available`: a planner, on a working backend,
not held, not failed; momentary busyness is ignored). Otherwise it alerts exactly as
before; a teammate crash with no PM stays silent as before. `0` turns the feature off.

**Briefing.** `core::problems` (pure, no clock) merges the runner's `ProblemNote`s with
state-derived items (failed, non-held, non-planner employees on an unlimited backend)
and writes `P-<key> | kind | subject | detail | tried N/M` lines, ordered by kind then
key, details cut to 200 characters, the whole text capped at 1,500 characters with
`+N more`. Ids and names only. `dispatch::problem_actions` emits
`Action::PmHandleProblem { pm }` (claim key `@problem`) between request triage and the
board check-in, only when the runner says a briefing is due, no earlier PM briefing is in
the tick, the PM is free (`pick_triager`, so never a held one) and `live_problems` is not
empty. A held employee is never in a briefing; a note naming one is dropped.
The action never alerts on failure: a briefing that cannot be delivered is logged and
retried after the retry wait.

**Bounds and escalation.** A problem is briefed at most `problem_max_attempts` (2) times,
`problem_retry_minutes` (5) apart. The runner raises the stakeholder alert, with the
original title, when the attempts are used up and one more retry wait has passed, or when
`problem_grace_minutes` (15) have passed since the problem was first seen, whichever is
first. The detail is the original one plus
`The project manager tried: <summary or "no summary"> (N attempts).` A problem the PM
reported handled that fails again is escalated at once. A record is dropped when the same
key later succeeds (the action, an app start, the agent starting, the ticket moving) and
by the refresh command, which raises nothing. Failing `@` actions keep their cooldown, so
there is no retry storm.

**PM tools (additive).** `problem_resolved { key, summary }` -> `ResolveProblem` ->
`ProblemResolved` plus an info `AlertRaised` ("PM handled: <summary>"), so the
stakeholder always sees what was done. `retry_action { key }` -> `RetryAction` ->
`ActionRetryRequested`; the runner clears the key's cooldown, failure count and idle
count. `restart_teammate { employee }` -> `RestartEmployee` -> `EmployeeRestarted`; the
agent runtime closes the employee's sessions (`AgentStopped`, never `AgentFailed`) and
the failure clears. The engine authorizes by workflow and role data (the actor's role
owns the first workflow state), never a role name; the commands are refused for any other
actor, and restart is refused for a held employee, the PM himself and unknown employees.
Tool access is role data (`mcp_tools`). `env_retry` is not re-run by `retry_action` (a
retry of a QA environment step stays the stakeholder's `RetryEnv`, and those failures are
already triaged by failure triage).

**Config.** `[team] problem_grace_minutes = 15`, `problem_max_attempts = 2`,
`problem_retry_minutes = 5`. `DispatchConfig::new` leaves the feature off; the CLI sets it.

## Consequences

- The records live in memory only: a restart forgets deferred problems (a still-failed
  teammate is found again from state; a failed action fails again and is re-deferred).
- A project with its own `.chillax/roles/project-manager.md` keeps its copy and gets
  neither the prompt step nor the three tools (as with ADRs 0021 and 0031): the briefing
  still arrives, but the tools are refused until the file lists them, and the problem
  escalates after the grace window with "no summary".
- Amends the failure-triage rules: agent crashes of teammates and stuck agents now go to the PM first
  (they stay out of failure triage).
- Cost: up to `problem_max_attempts` short PM turns per problem.

## Deferred to a later item (9b)

- The PM retuning another employee's agent, model or effort (`SetEmployeeAgent` stays
  stakeholder-only) and the `replace_teammate` / fire tools.
- Routing teammates' `ask_stakeholder` questions through the PM.
