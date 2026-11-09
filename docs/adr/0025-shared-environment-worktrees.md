# ADR 0025 — Shared environment worktrees

- Status: Accepted
- Date: 2026-10-26
- Supersedes: the per-ticket-worktree part of ADR 0006 (and the `_prod` worktree of ADR 0015)

## Context

ADR 0006 gave every ticket its own git worktree under `.chillax/worktrees/<id>`. A
worktree is a full checkout (plus whatever dependencies the agent installs in it), so a
sprint of ten tickets cost ten copies of the project. Releases added a detached `_prod`
checkout next to the `_main` one used for merges.

## Decision

- New setting `[vcs] worktrees = "shared" | "per-ticket"`, default `shared`.
- The environments are `dev`, `qa` and `main`/prod, so the worktrees under
  `.chillax/worktrees/` are `_dev`, `_qa` and `_main`, and nothing else.
- **Shared mode.** A ticket's work happens on its branch (`feat|fix/<id>-<slug>`, cut from
  `dev`) checked out in `_dev` with `git switch` (`VcsHost::switch_branch`). `StartWork`
  records `.chillax/worktrees/_dev` as the ticket's worktree, so the event, command and
  `Ticket.worktree` keep their shape. Sessions, tests and rebases run there; reopening a
  session switches `_dev` back to the ticket branch first.
- **One ticket at a time.** The pure scheduler (`next_actions`, `Context::shared_dev_worktree`)
  lets a single ticket hold `_dev`: from `WorkStarted` (or a start in flight) until it is
  merged or dropped. No other ticket starts work, review, tests or rebase meanwhile. QA
  rounds, promotion and releases do not use `_dev` for ticket work and are unaffected.
- **Merge.** Chillax commits, then checks `dev` out in `_dev` and merges with `--no-ff`
  as before. In GitHub mode it merges the pull request and then fast-forwards `dev` in `_dev`.
  After the merge, and on a drop, `_dev` stays and is left clean on `dev`. A drop first
  commits work in progress to the ticket branch. If `dev` is checked out in the
  stakeholder's own repository git refuses a second checkout, so `_dev` is parked detached
  at `dev` and merges happen in that clean checkout, as before.
- **Prod.** The prod app runs from `_main` detached at the release tag; a release
  switches `_main` back to `main` to merge. There is no `_prod`.
- `per-ticket` is today's behaviour (a worktree per ticket, tickets in parallel), and a
  ticket started in that mode keeps its own worktree if the setting changes.

## Consequences

- Disk use no longer grows with the number of tickets in a sprint.
- Throughput drops to one developer at a time; `max_parallel_tasks` and
  `max_concurrent_agents` still bound reviewers and testers. Teams that want parallel
  development set `worktrees = "per-ticket"`.
- The dev app (ADR 0015) runs from `_dev`, so while a ticket holds it, it shows that
  ticket's branch until the merge moves `_dev` back to `dev`.
- The prod app's files change under it for the length of a release, until it restarts at
  the new tag.
- An old `_prod` directory from an earlier version is no longer used; remove it with
  `git worktree remove`.

## Amendment: a dirty main checkout never blocks a merge

When `dev`, `qa` or `main` is checked out in the user's own repository (git allows one
checkout per branch) and that tree has uncommitted changes, Chillax no longer waits for it
to be clean. It builds the merge in a throwaway detached worktree
(`.chillax/worktrees/_merge-*`), then runs `git merge --ff-only <merge commit>` in the
user's checkout. Git fast-forwards around unrelated local changes and refuses, touching
nothing, when they overlap the files the merge changes; that case is the one `BaseBusy`
alert. The release preflight no longer reports dirty checkouts. The dev loop's per-ticket
cooldown now doubles on each repeated failure (30s up to the backoff cap) and resets on
success or refresh, so a stuck merge is not retried every 35 seconds.
