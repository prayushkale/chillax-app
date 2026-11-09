# ADR 0004 — Choosing the agent and model per hire

- Status: Accepted
- Date: 2026-10-19

## Context

Hiring always used the role's `default_backend` (`claude` for every built-in role). The
stakeholder was never asked which coding agent or model an employee should run on, and
`Employee.model` was stored but never applied to the ACP session. Persona revisions ran
on the base role's backend with no model choice either. The stakeholder wants to be
asked and wants the last choice remembered, with a place to change it.

## Decision

1. **`AgentDirectory` adapter** in `chillax-core::agents`, implemented by
   `chillax-agents::AcpAgentDirectory` and served at `/api/v1/agents`. It lists the
   configured `[agents.*]` backends, plus known presets (claude, codex, gemini) whose
   command is on `PATH`. A backend's models come from its ACP session config option of
   category `model`. Getting them means `initialize` + `session/new` in an empty scratch
   directory with no prompt, so it is free. The list is cached per backend.
2. **Enabling a preset writes config.** Picking an installed preset appends its
   `[agents.<name>]` table to `.chillax/chillax.toml`. Appending keeps the
   stakeholder's comments. The table goes into a shared, lock-protected backend map, so
   sessions, the persona editor and the directory can use it without a restart.
3. **Hiring defaults are events.** `SetHiringDefaults { backend, model }` →
   `HiringDefaultsSet`, projected into `State.hiring`. A stakeholder hire whose explicit
   pick differs from the defaults emits `HiringDefaultsSet` too, so the last choice
   sticks. The defaults live in the local event log: they are a per-machine preference,
   not team config.
4. **Precedence** (`engine::hire_choice`, also used for persona drafts): the explicit
   pick, then the defaults, then the role's `default_backend` and `model`, then
   `claude`. A model is only kept with the backend it was chosen for.
5. **Applying the model.** After `session/new`, sessions and the persona editor send
   `session/set_config_option` with the matching option. `match_model` accepts exact
   ids, or aliases whose id or name covers the wanted id (`claude-sonnet-5-5` →
   `sonnet`, "Sonnet 5.5"). With no match, the agent keeps its default and Chillax
   logs a warning.
6. **Readable protocol errors.** `acp::describe_error` turns `auth_required` into
   "the <agent> isn't signed in … run `claude` and /login (or export the API key)".
   The adapter sends that bare error when its CLI login has expired.

## Consequences

- Every frontend asks: the TUI hire wizard (Agent step) and Settings tab, the web hire
  form, persona review and Settings tab, and `chillax hire` / `chillax settings`.
- `/api/v1` grows additively: `/agents`, `/agents/{name}/models`,
  `/agents/{name}/enable`, the `set_hiring_defaults` command, the
  `hiring_defaults_set` event, `State.hiring`, and `model` on drafts and roles.
- The first model listing for a backend starts the agent once (a few seconds for
  `npx` adapters).

## Addendum: effort

A hire can also pick the model's effort (thought level), shaped like the model:
`effort: Option<String>` on `Employee`, `HiringDefaults`, `HireEmployee`,
`SetHiringDefaults` and `hiring_defaults_set`, all additive (`#[serde(default)]`, omitted
when unset, so old logs load unchanged).

- **Discovery.** The agent's session config option of category `thought_level` (fallback
  ids `effort`, `thought_level`, `reasoning_effort`) is read by the same probe that lists
  models and cached with them. `AgentDirectory::efforts` (default: none) and
  `GET /api/v1/agents/{name}/efforts` return a bare array, `[]` when the agent has none.
- **Apply.** After the model switch a session sets the effort the same way (matched on
  id or name), using the config options the model switch returned when present, since
  agents may rebuild their effort choices per model. An unknown effort logs a warning and
  never fails the session. `AgentStarted` reads `claude-agent-acp 1.0, model sonnet, effort high`.
- **Defaults.** `hire_choice` keeps a default effort only for the backend and model it
  was set with, like models stay with their backend. A default effort needs a default
  backend.
- **Known limitation.** Efforts are probed with the agent's default model, so the list
  may not match another model's. A later `?model=` query on the route would be additive.
- **Not covered yet.** Persona drafts and the persona editor carry no effort; picking one
  while hiring a persona is not offered (TUI) or ignored (web, CLI).

## Addendum: changing a hire's agent

An existing hire's agent, model and effort can be changed with `SetEmployeeAgent
{ employee, backend?, model?, effort? }`, which emits `EmployeeAgentChanged`.

- **Semantics.** A blank backend keeps the current one. Model and effort are replaced
  by what is given; absent means the backend's default (as with `SetHiringDefaults`),
  and an effort is kept only alongside a model. Choosing the same agent again records
  nothing. Stakeholder-only.
- **No effect on defaults.** Hiring defaults and role defaults are untouched, so one
  employee's switch does not change who the next hire is.
- **Sessions end, not resume.** Sessions never resume, so the runtime closes every
  session of the employee (chat and tickets) and records one `AgentStopped`. The next
  chat message or dispatcher briefing starts on the new agent; tickets stay in progress.
- **Refused while replying.** The command fails with "X is replying; try again when
  they finish" while a turn is open, like clearing a chat.
- **Validation.** The core does not check that the backend exists; frontends do (the
  CLI via the agent directory). An unknown backend surfaces as `AgentFailed` when the
  next session starts.

A change asked for while the employee is replying can be queued and applied at the end of
the turn (`when_idle`).
