# ADR 0006 — The dev loop: worktrees, Chillax-owned git, and recorded facts

- Status: Accepted
- Date: 2026-10-21

## Context

M2 ends with an approved sprint whose tickets sit in `ready`. M3 has to take them to
`dev` without the stakeholder: developers build, a Tech Lead reviews, and the work is
merged, with several agents working at once. Three things can go wrong. Agents claim
more than is true ("tests pass"). Agents run git commands that disturb each other or
the stakeholder's checkout. Chillax loses track of what it was doing when it restarts.

## Decision

1. **Chillax owns git and the gates; agents write code.** An agent commits inside its
   own worktree and nothing else. Chillax creates worktrees and branches, runs the
   project's tests, merges, and rebases, all by shelling out to `git`
   (`chillax-vcs`, behind the `VcsHost` trait). Agents reach Chillax only through MCP
   tools (`request_review`, `submit_review`, `handoff`).
2. **A ticket's progress is recorded facts.** Chillax records `tests_pass`,
   `review_approved` and `merged_to_dev` on the ticket (`Ticket.checks`) as events
   (`TestsRun`, `ReviewSubmitted`, `BranchMerged`). `workflow.yaml` transitions list the
   facts they `require`, and the engine enforces them for every actor, so an edit in an external tracker
   cannot skip a gate either. An unknown requirement fails when the workflow loads.
   `qa_passed` has no setter until M4, so `in_qa → done` stays closed.
3. **The dev loop owns three transitions.** `ready → in_progress`,
   `in_progress → in_review` and `in_review → in_qa` can only be made by Chillax
   itself (`StartWork`, `StartReview`, `RecordMerge`). Sending a ticket back
   (`→ in_progress`) is open to anyone and clears the stale facts.
4. **One pure function decides.** `chillax_core::dispatch::next_actions(engine, ctx)`
   turns the state plus runtime facts (open sessions, actions in flight) into actions.
   It honours pause, WIP limits (`max_parallel_tasks`), blockers, role ownership of
   states and the concurrent-agent budget. The dispatcher (`chillax-dispatch`) runs the
   actions and records their results as commands.
5. **Decisions come from state, so a restart resumes.** Every decision reads ticket
   state, not memory. A ticket in `in_progress` with a branch and no session gets one;
   one in `in_review` with a reviewer and no session gets one. The briefing a session
   starts with includes the ticket's recent comments, so feedback that arrived while no
   session was running is not lost. Messages sent to running sessions (test output,
   review notes, merge failures) are a convenience on top of that.
6. **Sessions are keyed by (employee, ticket).** A ticket session lives in the ticket's
   worktree. An employee's chat session is separate; chat messages go to their ticket
   session when they have one. Ticket sessions auto-approve only tool calls whose
   locations are inside the worktree, and review sessions auto-approve only reads.
   Shell commands name no location, so for them the role policy decides.
7. **Merges happen in an integration worktree.** Approved branches are merged
   `--no-ff` into `dev` in `.chillax/worktrees/_dev`, so the stakeholder's checkout is
   never touched. If `dev` is already checked out somewhere, Chillax merges there only
   when it is clean. Conflicts abort the merge, record `MergeFailed`, and send the
   ticket back to its developer. Merges and rebases take turns.
8. **Open branches follow `dev`.** A merge marks every `in_progress` branch behind.
   Chillax rebases it when its agent is idle and the tree is clean; a conflicting rebase
   is aborted and the developer is asked to merge `dev` themselves.
9. **Tests are the project's.** `[env.dev] test` runs in the worktree on
   `request_review`, with a timeout. With no command the run counts as skipped, and
   `chillax doctor` warns. This is the smallest piece of the `Environment` adapter that
   M4 will grow.
10. **A failing step cools the ticket down.** A failed action is written on the ticket
    once and the ticket is left alone for `cooldown` before another try.
11. **A kill switch ships with the loop.** `PauseDispatch` / `ResumeDispatch`
    (`chillax pause|resume`, Shift+P in the TUI, a button on the web) closes every
    ticket session; resuming reopens them. It was planned for M6, but an unattended
    loop needs it from the start.
12. **Plans and diffs reach the live view.** The ACP runtime records plans and tool-call
    content (diffs and text, capped at 16 KB per event) as `AgentPlanUpdated` and
    `AgentToolCallContent`. `State.live` keeps each employee's plan and last 30 tool
    calls.

## Not done

- **External tracker relations.** `blocked_by` lives in Chillax only (the external tracker
  integration was removed).
- **Release gate, QA, environments** — M4.

## Consequences

- Event schema and `/api/v1` changes are additive: new commands, events, optional
  ticket fields and state fields.
- Everything the loop does shows up in the event log and, through ticket comments.
- One agent session per ticket means an employee with two tickets (a Tech Lead with
  two reviews) has two sessions whose output shares one chat transcript. Their status
  is per employee.
- The loop starts work as soon as `chillax` opens, which resumes unfinished tickets
  automatically; `chillax pause` stops it.
