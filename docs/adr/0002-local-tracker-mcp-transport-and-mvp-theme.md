# ADR 0002 — Local tracker, MCP over HTTP, and the MVP theme

- Status: Accepted
- Date: 2026-10-19

## Context

The MVP covers M0 + M1 plus planning: you hire a PM, chat with it, and it writes epics,
stories and a sprint you approve. An external tracker (M2, since removed) would need a running service, which makes
the first run heavy. Three other questions came up while building it:

1. How agent sessions reach the Chillax MCP tools. ACP's MCP-over-ACP transport exists in
   the Rust SDK but is unstable, and adapters don't support it yet.
2. How the role prompt reaches the agent. ACP v1 has no stable system-prompt field.
3. Whether the default web theme needs a Node build step from day one.

## Decision

1. **A built-in `local` tracker** (`[tracker] kind = "local"`, the default). Epics,
   tickets and sprints are events in Chillax's own log (`EpicCreated`, `TicketCreated`,
   `SprintProposed`, …), projected into state. `chillax-tracker` defines the `Tracker`
   trait and `LocalTracker`, which issues core commands. Chillax MCP tools only talk to
   `dyn Tracker`, so an external tracker could plug in behind the same trait without touching the tools.
2. **MCP over streamable HTTP, served by Chillax** at `http://<bind>:<port>/mcp`
   (`rmcp`). Each session gets an `McpServer::Http` entry with a per-employee bearer
   token, which tells Chillax who is calling. The role's `mcp_tools` filters both
   `tools/list` and `tools/call`. Tokens live only in memory. The server accepts only
   loopback `Host` headers.
3. **The role prompt opens the first prompt** of each session, inside a
   `<chillax-role>` block, followed by the stakeholder's message. When the
   `session/new` system-prompt capability stabilises, it moves there. Per-role `model`
   is recorded but not applied yet.
4. **The MVP default theme is a vanilla static bundle** (`web/themes/default`: HTML,
   CSS and JS), embedded with `rust-embed`. It uses only `/api/v1`. It fetches
   `/state` and re-fetches it on each SSE event, so it holds no business logic. The
   React/Vite theme and the generated TS SDK stay in M5. The OpenAPI document is
   hand-written for now; `utoipa` generation comes with the SDK.
5. **The `sprint_start` gate is enforced in the engine.** A ticket can move from the
   initial state into the planned state only when it belongs to an approved sprint.
   Approving a sprint moves its tickets there. `ready → backlog` was added to the
   default workflow so work can be de-scoped.

## Consequences

- `chillax` works with no external services: git and an ACP agent are enough.
- The event log is the tracker's source of truth. The local log stays the identity source;
  an external tracker, if one is ever added, would mirror it.
- Agents that lack HTTP MCP support still chat but can't plan. The session preamble tells
  the agent so, and `chillax doctor` reports `MCP over HTTP: no`.
- Tokens are reissued after a restart, and sessions restart fresh. Resuming sessions with
  `session/load` arrives with crash recovery (M6).
