# ADR 0038 — Per-ticket worktrees by default and work stealing

- Status: Accepted
- Date: 2026-10-27
- Related: ADR 0025 (worktrees)

## Context

With `worktrees = "shared"` (the ADR 0025 default) one ticket holds the dev worktree, so no
other ticket starts while any is in progress or awaiting merge. A real run showed a developer
idle for four hours while an unblocked ticket assigned to them waited, and the PM's
pre-assignment left other developers' queues empty while one person's tickets ran serially.

## Decision

- `WorktreeMode::PerTicket` is the default. `worktrees = "shared"` still works, and still
  serialises everything (disk-saving option).
- Work stealing in `core::dispatch::next_actions`: a planned ticket in the active sprint with
  clear blockers whose assignee is at capacity, held or on an unavailable backend is
  reassigned (the existing `Reassign` action) to the least-loaded free employee whose role owns
  `in_progress` (`pick_builder`). This was urgent-only; it now applies to every ready ticket.
  Global `max_concurrent`, per-role caps, holds, halted backends and the shared-worktree
  holder still gate it. Ticket order (urgent first, then creation) is unchanged.
- Blocked tickets are never stolen.

## Consequences

Idle developers pick up ready work; assignments are a preference, not a reservation. Projects
that want the old behaviour set `worktrees = "shared"`.
