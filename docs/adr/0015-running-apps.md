# ADR 0015: Chillax runs the dev, QA and prod apps

Status: accepted (2026-10-23)

## Context

The stakeholder wants to open the dev, QA and production apps on localhost ports and
see them follow the code, without starting them by hand after every merge, promotion or
release. Until now Chillax ran only one-shot commands per environment (`test`, `up`,
`down`, `deploy`).

## Decision

- **Config.** Each of `[env.dev]`, `[env.qa]`, `[env.prod]` takes optional `run` (a shell
  command; `{port}` is replaced and `PORT` is set), `port`, `url` (default
  `http://localhost:<port>`) and `run_cwd` (a folder inside the checkout). Without `run`
  and `port`, nothing is supervised. Startup refuses duplicate ports, a port equal to the
  Chillax server's, a `run` without a `port` and a `run_cwd` that leaves the repository;
  `chillax doctor` reports the same, plus ports that are already taken.
- **Where each app runs.** dev from the `dev` integration worktree (`_dev`), qa from
  `_qa`, both through `VcsHost::checkout`. prod runs the latest release tag from a
  **detached** worktree `.chillax/worktrees/_prod`, made and moved by the new
  `VcsHost::detached_checkout(name, commit)`. Detached, because the stakeholder's own
  checkout usually holds `main`, and git allows a branch in one worktree only. Untracked
  files (installed dependencies) survive a move. With no release yet prod is stopped with
  the reason "no release yet".
- **Model.** `State.apps` (`RunningApp`: env, port, url, status starting/running/crashed/
  stopped, `wanted`, commit, since, reason, restarts) is projected from additive events
  `AppsConfigured`, `AppRequested`, `AppStarted`, `AppStopped`, `AppCrashed` (with
  `will_retry` and a 4 KB output tail) and `AppRestarted`. The dispatcher sends
  `ConfigureApps` when it starts, so the log knows what is configured. The stakeholder's
  commands are `StartApp`, `StopApp`, `RestartApp {env}`; Chillax records the rest.
- **Decision stays pure.** `core::apps::app_actions` (called from `next_actions`, also
  while dispatch is paused) compares what each app should run (`Context.app_targets`:
  dev head, qa head, tag commit, or an idle reason, read from git by the dispatcher) with
  what is running (`Context.app_live`, the supervisor's commit per app) and returns
  `RunApp {env, commit, why}` or `StopApp`. The log alone is not trusted for "running":
  after a Chillax restart the process is gone, so the app starts again. A stopped app
  stays stopped (`wanted = false`) until started. A crashed app is not retried on the same
  commit; new code or a restart request tries again.
- **Where the processes live.** A new module `chillax-dispatch::apps` (`AppSupervisor`),
  not a new crate: the dispatcher already owns process spawning, the `Environment` shell
  adapter and the VCS handle, and the supervisor needs the hub to record events. The
  one-shot `Environment` trait is unchanged (its `up`/`test`/`down`/`deploy` run as
  before); a long-lived process is a different shape, so it is a concrete type rather than
  a method on that trait.
- **Processes.** `sh -c <run>` in its own process group; stopping sends SIGTERM to the
  group, then SIGKILL after 5 s, so the whole tree ends (`kill -s SIG -- -pgid`; no
  `unsafe`, no new dependency). Output is appended to `.chillax/state/env/<env>.log` (a
  log over 1 MB moves to `<env>.log.1` at the next start). A crash is retried after 2 s,
  4 s, ... up to 30 s, five times in a row (an app that ran 60 s starts counting again);
  then it is `crashed`, an inbox alert shows the output tail, and the log path. A port
  that already answers on 127.0.0.1 fails the start with a plain message instead of a
  confusing crash. On quit, `Company::shutdown` stops every app; process groups are also
  registered globally so a panic hook and SIGTERM/SIGHUP handlers kill them, and dropping
  the supervisor kills its own. A SIGKILL of Chillax itself cannot be caught; its apps
  keep running and the next start reports "port already in use".
- **Views.** (The Dashboard's one-line strip became a "Running apps" panel that also lists reported apps.) TUI Environments: each panel shows its app (status, commit, reason) and the
  address as an OSC 8 hyperlink. Ratatui has no link cell; the sequence goes in one cell's
  symbol with `CellDiffOption::ForcedWidth(width of the text)`, which makes the diff skip
  the covered cells and not count the escape bytes. `CHILLAX_PLAIN_LINKS=1` (or
  `TERM=dumb`) draws plain text. The URL is never put into a sequence if it contains a
  control character. Keys, declared in the key table so hints are conditional: `↑↓` pick
  an app, `s` start, `t` stop, `u` restart, `o` open in the browser. The Dashboard shows
  `dev :3000 ● qa :3001 ● prod —`. The web Environments view shows the same with a link
  (http/https only) and buttons. `chillax env` prints app lines and
  `chillax env --restart <env>`.

## Consequences

- `run` is arbitrary shell from the committed config, with the same trust as `up`, `test`
  and `deploy`. Chillax does not sandbox it; the app listens wherever its command makes
  it listen (a dev server on `0.0.0.0` is reachable from the network).
- Output tails in events are capped at 4 KB and not scrubbed (as for other environment
  commands).
- In GitHub mode `dev` and `qa` run from the local branch heads, which Chillax
  fast-forwards from the remote only when the checkout is clean; otherwise the app runs
  the older code and the existing "local branch is behind" note applies.
- Windows is not supported by the supervisor (process groups and `kill`); it compiles.

## Amendment: every agent-started process dies with Chillax

Apps were the only processes in a process group. Dev servers, `pnpm dev`, `wrangler dev`
or QA `up` servers started by an agent's shell tool or by an environment step outlived
Chillax. Now:

- ACP agent processes run in their own group (the ACP crate does this; Chillax starts them
  through `sh -c 'echo $$ > pidfile; exec agent'` to learn the group id) and environment/test
  shell steps (`run_shell_with`) run under `process_group(0)`. Their group ids go into one
  registry (`chillax_agents::procgroup`) shared with the apps.
- Closing a session, `AgentRuntime::shutdown_all` and `Company::shutdown` send SIGTERM to
  the group, wait up to 5 s, then SIGKILL. SIGTERM/SIGHUP, a second signal and the panic
  hook sweep the registry too (short grace, blocking).
- A shell step that times out has its whole group ended. A step that returns successfully
  leaves its group alone, so a `qa.up` server survives the step; the group stays registered
  while anything in it runs and is stopped when Chillax exits.
- Known limit: a process that calls `setsid` or double-forks into a new session (some
  daemonizing tools) leaves the group and is not reached. A cwd-based sweep of
  `.chillax/worktrees/` (`lsof +D`) was considered and rejected as slow and unreliable on
  macOS.
