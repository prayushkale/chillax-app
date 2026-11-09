# ADR 0007 — QA rounds, the release gate and the branch flow

- Status: Accepted
- Date: 2026-10-22

## Context

M3 takes an approved sprint to `in_qa` (merged into `dev`) and stops. Nothing recorded
`qa_passed`, nothing started a QA agent, the `release` gate was parsed but unused, and
`qa` and `main` had no owner. M4 closes the lifecycle: backlog → done → tagged release.
The risks are the same as in ADR 0006: an agent claiming more than is true ("looks
fine"), flaky environments producing false bugs, and git operations disturbing the
stakeholder's checkout.

## Decision

1. **QA works in rounds over a snapshot.** A `QaRound` covers every ticket waiting in
   `in_qa` when `dev` is promoted to `qa`. Promotion is automatic once every story of
   the active sprint is in `in_qa`, or on demand (`PromoteToQa`: `p` in the TUI, a
   button on the web, `chillax promote`). `qa` is only ever fast-forwarded to `dev`.
   Only one round is open at a time, and promotion also waits while a release is in
   flight, otherwise `qa` would move under an untested release.
2. **Chillax runs the environment, agents test.** `[env.qa] up` and `test` run before
   any agent starts, through the `Environment` trait (`ShellEnvironment`: timeouts,
   output capped at 4 KB). A failure there is an *environment* failure, never a bug:
   the round is `EnvFailed`, an inbox `EnvAlert` offers a retry, and nothing is retried
   automatically. `QaRound.up_done` lets a retry resume at `test`. `down` runs when the
   round closes. An agent's `env` verdict is retried once, then raised.
3. **A verdict has to be earned.** `submit_qa` (MCP, for the role that owns `in_qa`)
   records `pass`, `fail` or `env`. A pass must give a passing result with evidence for
   every acceptance criterion, or the engine refuses it. A fail must name the failed
   criteria with evidence and file a bug. Only a pass records `qa_passed`, which is the
   only way through `in_qa → done`; the loop owns that transition too.
4. **A failed story stays in `in_qa`; its bug runs the normal dev loop.** The bug
   (`kind: bug`, `found_in: story`) joins the story's sprint as `ready`, assigned to the
   story's developer. It skips the `sprint_start` gate, a narrow system-only exception:
   its sprint was already approved. The next round retests the story and the bug.
   Sending a ticket back out of `in_qa` clears `qa_*`.
5. **QA reads and runs, but writes only its scratch directory.** QA sessions work in
   the shared `_qa` worktree. They can run commands under the role policy but may edit
   only `.chillax/state/qa/<ticket>/`.
6. **The gate opens on facts, or on force.** When a round closes with every ticket
   passed, `ReleaseProposed` opens the `release` gate and puts a `ReleaseGate` in the
   inbox. The stakeholder can open it earlier with `force`, and the failures are then
   listed under "Known issues". There are no reverts.
7. **Versions and notes.** `core::release` suggests the version from Chillax's own
   releases (not git tags): `v0.1.0` first, then a minor bump when a story ships and a
   patch bump otherwise. An existing tag is adopted only if it equals the prod head.
   The notes are generated deterministically (stories by epic, fixes, known issues, a
   QA summary) and are editable at the gate. The approved version must be valid semver
   and newer than the last release.
8. **Release flow.** After approval Chillax commits the notes to `CHANGELOG.md` on `qa`,
   merges `qa` into the prod branch with `--no-ff`, tags the merge commit (annotated,
   the notes as its message) and **back-merges the prod branch into `dev`**, so `qa`
   stays an ancestor of `dev` and the next promotion is a fast-forward. A back-merge
   conflict, a dirty checked-out prod branch or a diverged `dev` raises an inbox
   `Alert` (generic `RaiseAlert` / `DismissAlert`, for problems with no ticket). If
   `[env.prod]` has a `deploy` command that fails, the release still stands: the
   failure is recorded, an alert offers a retry (`RetryEnv`), and the release is
   `DeployFailed` until it passes.
9. **Integration worktrees per branch.** `LocalGit` keeps one worktree per integration
   branch (`_dev`, `_qa`, `_main`) and the M3 rule: if a branch is checked out
   elsewhere, Chillax merges there only when that checkout is clean. All merges share
   the git lock. Local only in M4; GitHub mode pushes and merges through pull requests.
10. **The stakeholder drives it from every surface.** The Environments view (TUI tab,
    web tab) and `chillax promote | release | env` send the same stakeholder commands:
    `PromoteToQa`, `OpenReleaseGate{force}`, `ApproveRelease{id,version,notes}`,
    `RejectRelease`, `RetryEnv`, `DismissAlert`. An `EnvAlert` clears when its step is
    retried, so only plain alerts are dismissed. The commands, events and state are
    additive in `/api/v1` and listed in the OpenAPI spec.
11. **No external tracker mirror.** An earlier version mirrored relations and cycles to an external
    tracker (since removed); Chillax's own event log is the only source of truth.


## First real run

One story ("add `farewell`") went from the PM's plan to a tagged `v0.1.0` with real
agents: Command Code on `minimax/minimax-m3.1-flash:free`, the local tracker, a scratch
repo. The tag is annotated and on the merge commit, `CHANGELOG.md` is on `main`, `qa` and
`main` are ancestors of `dev`, and the sprint ended `released`. Only the pass path ran;
no bug was filed. What it taught:

- **Chillax's own MCP tools asked for permission** (`mcp__chillax__list_team`), so every
  call landed in the inbox. Fixed: calls to the `chillax` server are auto-approved; the
  endpoint already limits each caller to its role's tools.
- **Reviewers needed approvals for shell commands** (six in one review), because review
  sessions auto-approve only reads (ADR 0006). Reviewing a diff needs `git`, so an
  unattended review still stalls. Open: let review sessions run read-only git commands.
- **The PM tried to run shell commands in the stakeholder's checkout**, and the agent
  offered to enter its own plan mode. Both landed in the inbox and were refused.
- **A dirty `main` blocks the release**, untracked files included (the release raised
  the alert, dismissing it and cleaning up let the retry through). Open: decide whether
  untracked files should count. Resolved: they still count (they catch real user and
  agent changes), but `chillax init` now commits its own files (ADR 0020) and the alert
  names the first dirty paths.
- A permission request left from before a restart has no live agent behind it; it has
  to be cancelled by hand.

## Consequences

- Chillax decides promotion, the environment and git; agents only test and report.
  Restarting resumes from state, as in M3: a round, a gate and a release are all
  projections of events.
- A flaky QA environment cannot file bugs, and a QA agent cannot rubber-stamp.
- `up`, `test` and `deploy` run commands from committed config. Their output is capped
  in events but not scrubbed, so deploy scripts must not print secrets.
- The back-merge means every release adds a merge commit to `dev`. Teams that dislike
  that can rebase `qa` themselves; Chillax then raises the diverged-`dev` alert.
- Budgets, stuck-agent detection and web auth are still M6.

## Amended 2026-10-23 (continuous QA)

QA no longer waits for the whole sprint. Promotion is automatic as soon as any ticket is
waiting in `in_qa` (not yet `qa_passed`) and no round or release is open; `qa` is still only
fast-forwarded to `dev`. Tickets that arrive while a round runs join the next round, which
starts when the open one closes. A story that failed QA is held back until the bugs filed
against it have been merged, so it is retested together with its fixes. A clean round
proposes the release only when every ticket of its sprint has reached the workflow's last
state (no longer the literal `done`); otherwise the sprint stays open and the gate opens at
sprint completion, or when the stakeholder prepares a release by hand. A manual promotion
request still promotes whatever waits.
