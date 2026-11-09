# ADR 0020 — Set a folder up on the first command

- Status: Accepted
- Date: 2026-10-25

## Context

Plain `chillax` and `serve --headless` asked on a terminal whether to set the folder up, and
every other command failed with "run `chillax init`". That is friction with no safety gain:
the questions had one sensible answer.

## Decision

Every command except `init`, `projects` (listing only) and `herdr status|start` goes through
`project::ensure_project`. If the folder has no project it creates the git repository (when
missing), `.chillax/`, gitignore entries and the dev/qa branches, prints one line to stderr,
and carries on. This reverses the earlier prompt-on-terminal design. There is no prompt and no
environment opt-out.

Safety rails:

- It refuses the home directory and `/`, and a non-git folder that contains the home
  directory, writing nothing. An explicit `chillax init` is not restricted.
- Inside a git repository the project root is the repository top level, as with `init`.
  Inside a linked worktree (for example an agent running from `.chillax/worktrees`) it is the
  main worktree, so no nested `.chillax/` appears.
- The first commit needs `user.name` and `user.email`; Chillax never sets them. On failure the
  error says what to run, and `init` is idempotent, so a rerun recovers.

## Consequences

- Accepted trade-off: running any command in a wrong folder (say `~/Downloads`) creates a
  repository there. The stderr notice and the rails above are the only brakes.
- The first commit uses `git add -A`, so it sweeps in whatever the folder holds. The
  gitignore entries for `.chillax/state/` and `.chillax/worktrees/` are written first.
- Read-only commands such as `doctor` and `team` now write on their first run.

## Addendum: init commits the files it writes

Init used to leave `.chillax/` and the `.gitignore` lines uncommitted, so the checked-out
prod branch was dirty and the first release was blocked ("main is checked out ... with
uncommitted changes"). Now init commits exactly the paths it created or changed
(`git add -- <paths>`, `git commit -- <paths>`, message `chore: set up Chillax`), before the
`dev`/`qa` branches are cut so they inherit the config. Staged or untracked user files stay
as they were. `.gitignore` is left out when it already had uncommitted changes, and nothing is
committed when HEAD is not on the prod branch; `done` says so. A failed commit (no git
identity, a hook) is a note, never an error, and Chillax never sets an identity. On an empty
repo the files are written before the single "Initial commit". A second init writes nothing
and commits nothing. Because `.chillax/` is now tracked, linked worktrees contain a copy;
`Project::discover` resolves them to the main worktree.

## Addendum: the first start hires a Project Manager

The first `chillax` start of a project whose event log is empty also hires the roles in
`[team] default_hires` (default `["project-manager"]`, `[]` opts out), as the stakeholder
and without prompting. Both auto-init and `chillax init` leave the log empty until the first
start, so both are covered. A PM who was let go leaves events behind and is not re-hired,
a restart is a no-op, and projects with history are untouched. An unknown role id or a failed
hire only logs a warning. Sessions start lazily, so an idle PM costs nothing.
