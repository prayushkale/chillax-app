# Changelog

## Unreleased

- The Board shows only the active sprint, like Jira. Tickets with no sprint no longer crowd it; the Backlog and every past (released or closed) sprint are separate read-only views, chosen with `<` `>` in the TUI or the sprint picker on the web. Between sprints the Board is empty and says where the backlog and past sprints are. Membership follows `ticket.sprint`. No API or event changes.
- After a release is deployed, `qa` (and `dev`, when behind) fast-forward to `main`, `dev`, `qa`, `main` and the tag are pushed to the remote (never forced), and the main checkout is switched to `main` when that is safe (clean, no merge or rebase running, `main` not held by another worktree). Anything skipped raises one alert, "Release X deployed but branches not fully synced". Opt out with `[release] sync_after_deploy = false`.
- An employee's live transcript (Live, Chat and Team views, web and TUI) is emptied once their work on a ticket is done (tests passed, review or QA submitted, merged, round closed, ticket dropped) and they have no other ticket session. New event `employee_context_cleared` and Chillax-only command `clear_employee_context`; history stays in the event log. A dev's chat with the stakeholder shares the buffer and is cleared too (ADR 0008).
- Team grid: the arrows change view until you enter the grid (Enter or ↓, like the Board); then they walk the tiles and Esc leaves.
- The max concurrent agents shows as `Agents: r/limit (max N)` and can be changed live up to what the machine fits (TUI Team `+` / `-`, web Team field, `PUT /api/v1/budget/max-concurrent-agents`); the change is saved to `chillax.toml` and never stops a running agent . State gains `agent_limit`.
- Usage tab lists every ticket (not the top 10) and the token chart toggles between per hour and per day; `GET /usage` adds `hourly` and returns all tickets unless `top` is given.

## v0.1.0 — 2026-10-23

### Features

**Recent activity control**
- T-10 Add a clear-activity action to the TUI and web

**Terminal UI: mouse and work items**
- T-11 Support mouse clicks and scrolling in the TUI
- T-12 Show work item details and change their state from the TUI Board
- T-13 Create and edit work items from the TUI

**Web app design refresh**
- T-14 Refresh the web theme's layout, typography and colour
- T-15 Improve web empty, loading and error states

### Fixes

- T-17 T-14: Environments and Settings views missing; screenshots not attached to the ticket (found in T-14)

### Chores

- T-16 Design pass on the web theme: write DESIGN.md and critique the T-14 refresh

_QA round 2: 2 of 2 tickets passed._
