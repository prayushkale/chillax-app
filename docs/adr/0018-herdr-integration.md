# ADR 0018: herdr integration

Status: accepted (2026-10-24)

## Context

[herdr](https://herdr.dev) is a terminal workspace manager for coding agents. Many Chillax
users run Chillax inside a herdr pane. herdr can show an agent's state per pane, tokens
beside it, and desktop notifications, but only if told. Chillax already knows all of that
(inbox, QA, failures) from its event stream.

## Decision

- **Shell out to the `herdr` CLI, not its socket.** For v1 every call is
  `"$HERDR_BIN_PATH" <args>` with an argument vector (never a shell), null stdin, a 3 second
  timeout and `kill_on_drop`. The CLI is documented and versioned; the socket protocol is
  not a stable contract we want to track. Revisit if call volume ever matters.
- **The adapter is a module, not a crate.** `crates/chillax-cli/src/herdr/` holds the
  client, the typed verbs and the reporter. Only the pure parts live in `chillax-core`
  (`notify.rs`: the `Notifier` trait, `notifications_for`, `agent_summary`, and
  `[herdr]` config), so core stays free of processes and vendors (AGENTS.md rule 1, 3).
- **One aggregate pane report.** Chillax is one agent in its pane: `idle`, `working` or
  `blocked` (blocked when the inbox has anything for the stakeholder, working when an
  employee or QA round is active). Failures, inbox and QA counts and the project name go
  out as `report-metadata` tokens, which are visual only. herdr has no workspace-level
  metadata, so `project` is a pane token.
- **Identity.** `--source chillax:<project slug>` (must be stable, unique and not start
  with `herdr:`), `--agent chillax` (a name herdr does not already support). `--seq` is
  milliseconds since the epoch, incremented per call, so it rises across restarts;
  herdr ignores non-increasing reports. The same call carries
  `--agent-session-id <source> -- chillax` so herdr can resume Chillax after a restart
  (needs herdr 0.9.2 or later; the source holds the pane in the same call).
- **Notifications** go through `notification show` (sound `request` for attention and
  errors). A throttle drops the same key within 60 seconds and anything past 5 per 10
  seconds. History replayed at startup never notifies: the reporter subscribes before its
  first snapshot and ignores events already in it.
- **Degrade silently.** herdr is optional and must never block, delay or crash Chillax.
  A failing call is logged once per distinct error, retried with exponential backoff
  (1 s doubling to 30 s), and after 5 consecutive failures reporting turns itself off for
  the run. Shutdown aborts the reporter and releases the agent within 2 seconds. If
  Chillax exits without releasing, herdr clears the entry when the pane returns to a shell.
- **Detection.** Only when `HERDR_ENV=1` and `HERDR_PANE_ID` and `HERDR_BIN_PATH` name an
  existing binary. `[herdr] enabled = "auto"` (default) follows detection, `true` warns
  once when herdr is missing, `false` never talks to it.
- **Pinned to 0.9.x.** `chillax doctor` prints a herdr line only inside herdr (or with
  `enabled = true`) and warns, not fails, on another version.

## Consequences

- No new dependencies; tests use a recording fake client and never run herdr.
- 6b adds verbs to the same typed layer (below).

## 6b: opening employees in herdr, `chillax transcript`, the plugin

Facts taken from the herdr 0.9 docs (plugins, CLI reference, configuration), not assumed:

- Manifest `herdr-plugin.toml` has top-level `id`, `name`, `version`, `min_herdr_version`
  (all required), optional `description` and `platforms`; no `[plugin]` table. Tables:
  `[[actions]]` (`id` without dots, `title`, `contexts`, `command` as an argv array,
  optional `platforms`), `[[startup]]`, `[[events]]` (`on`, `command`), `[[panes]]`,
  `[[build]]`, `[[link_handlers]]`. Commands run with the plugin directory as cwd and no
  shell. Actions get `HERDR_PLUGIN_*`, `HERDR_WORKSPACE_ID`, `HERDR_TAB_ID`,
  `HERDR_PANE_ID`, `HERDR_BIN_PATH`, `HERDR_ENV`, `HERDR_PLUGIN_CONTEXT_JSON` (shape not
  documented; no cwd field is promised). The `HERDR_ACTIVE_*` variables (including
  `HERDR_ACTIVE_PANE_CWD`) exist only for `[[keys.command]]`, not for plugin actions.
  Only `contexts = ["workspace"]` is shown in the docs, so that is all we use.
- `herdr plugin link <path>`; actions are bound with `[[keys.command]]` `type =
  "plugin_action"` and a qualified `<plugin id>.<action id>` command. Other types are
  `popup`, `pane` and `shell`.
- `workspace create` returns `.result.workspace.workspace_id`, `.result.tab.tab_id`,
  `.result.root_pane.pane_id`. `pane split` returns the new pane at `.result.pane.pane_id`
  (not `root_pane`). `pane run <pane> <command>` types the text and Enter atomically.
  `pane get`/`pane list` may carry `foreground_cwd`. The shape of `workspace list` is not
  documented.

Decisions:

- **Verbs return errors.** `workspace create`, `tab create`, `pane split` and `pane run`
  go straight to the client, not through the reporter's failure policy: a user-run
  command must say why it failed, and it runs in its own short-lived process with a fresh
  client, so a reporter that gave up never affects it.
- **`chillax herdr open <employee>`** opens a pane (default: a shell in a split to the
  right of the current pane; `--tab`, `--split right|down`) in the employee's first live
  ticket worktree that exists (`Ticket::worktree_path`, shared with dispatch), else the
  project root. `--shell` types nothing, `--transcript` types `chillax transcript <id>
  --follow`, `--app` types the `[env.dev]` app `run` command (with `{port}` filled in,
  in `run_cwd`). The command is typed into the pane's shell by herdr; Chillax never uses
  a shell itself, and the employee id is shell-quoted.
- **Outside herdr** (`HERDR_ENV` unset) the herdr verbs fail with a message pointing at
  `chillax herdr workspace` from a herdr pane.
- **`chillax transcript`** reads `/api/v1/events` as SSE with the existing reqwest client
  (`Response::chunk`, no new feature) and folds `agent_message_chunk`, `agent_tool_call`
  and `agent_turn_ended` of one employee into text. Without `--follow` it stops when the
  server is quiet for 0.8 s (the backlog is sent at once); with no running instance it
  reads the log directly. It does not reuse `chillax-tui`'s transcript code, which draws
  styled, wrapped lines; the text fold is a few lines. `/api/v1` is unchanged.
- **The plugin** (`integrations/herdr`) has four actions, each a `sh -c` guard that
  explains when `chillax` is not on PATH and then `exec`s `chillax herdr start |
  dashboard | pick project | pick employee`. Because actions do not get the active
  pane's cwd, `dashboard` and `pick` look it up with `pane get` (`foreground_cwd`,
  searched for in the JSON since its nesting is undocumented). `pick` splits the current
  pane and runs `chillax herdr open` / `switch` there, which asks on that terminal.
- A TUI key for opening a pane is deferred.
- Agent processes get every inherited `HERDR_*` var blanked, and `release-agent` on quit bypasses the failure backoff, so herdr no longer shows agents "running" after Chillax exits (a panic still skips the release).
