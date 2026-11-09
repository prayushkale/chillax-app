# Chillax in herdr

When you run `chillax` inside a [herdr](https://herdr.dev) pane (0.9.x), Chillax tells
herdr what it is doing. Design: `docs/adr/0018-herdr-integration.md`.

## Turning it on or off

It is on by default and only does anything inside herdr (herdr sets `HERDR_ENV`,
`HERDR_PANE_ID` and `HERDR_BIN_PATH` in every pane). In `chillax.toml`:

```toml
[herdr]
enabled = "auto"      # "auto": only inside a herdr pane; true / false to force
notifications = true  # "needs you" notifications
metadata = true       # inbox, QA, failed, project and per-hire tokens beside the state
```

`enabled = true` warns when Chillax is not in a herdr pane. `chillax doctor` shows a herdr
line when herdr is present. If herdr stops answering, Chillax logs it once and stops
reporting; it never affects the company.

## What is reported

- The pane's agent state, labelled `chillax`: `blocked` when something waits for you in the
  inbox, `working` while employees or a QA round are active, otherwise `idle`.
- Tokens: `project`, `inbox`, `qa` (tickets under test) and `failed` (employees whose agent
  failed). A zero count is cleared.
- Per-hire activity, in a separate metadata call: tokens `hires` (for example
  `Dana working T-4 · Sam idle`, about 80 characters, ids and names only) and `active`
  (hires working), state labels for `working`, `blocked` and `idle`, and the display agent
  `chillax (2 working)`. It expires after 60 s unless refreshed, is sent at most every 2 s
  and only when it changed. Needs `metadata = true`.
- On quit, Chillax clears the per-hire metadata and releases the pane.

## Notifications

Questions and permission requests (needs you), alerts, agent failures, QA round closed
(needs you when any ticket failed), releases, and failed merges. The same item is not
repeated within a minute, and at most five go out per ten seconds.

## Opening employees in herdr panes

Inside a herdr pane (`chillax herdr status` shows the detection and herdr version):

```sh
chillax herdr open pat                 # a shell in Pat's worktree, split to the right
chillax herdr open pat --transcript    # follow what Pat says and does
chillax herdr open pat --app --tab     # the dev app ([env.dev] run) in a new tab
chillax herdr open pat --split down
chillax herdr workspace                # a herdr workspace for this project, running chillax
```

The directory is the worktree of Pat's current ticket, or the project root when Pat has
none. Outside herdr these fail with "not inside a herdr pane".

`chillax transcript <employee> [--follow] [--since N]` prints an employee's messages, tool
calls and turn ends as plain text; it works in any terminal. Without `--follow` it prints
the history and exits.

## herdr plugin

```sh
herdr plugin link /path/to/chillax/integrations/herdr
```

adds the actions `chillax.herdr.start`, `.dashboard`, `.switch-project` and
`.open-employee` (they need `chillax` on PATH and say so when it is not). Bind them in
herdr's config:

```toml
[[keys.command]]
key = "prefix+c"
type = "plugin_action"
command = "chillax.herdr.open-employee"
description = "open a Chillax employee"
```

Or skip the plugin and bind the CLI directly (herdr exports `HERDR_ACTIVE_PANE_CWD` to
key commands):

```toml
[[keys.command]]
key = "prefix+d"
type = "shell"
command = "chillax herdr dashboard"
```
