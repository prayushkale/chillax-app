# Chillax herdr plugin

Four herdr actions that call the `chillax` CLI. Needs herdr 0.9.x and `chillax` on PATH.

```sh
herdr plugin link /path/to/chillax/integrations/herdr
herdr plugin action list --plugin chillax.herdr
```

| Action | What it does |
| --- | --- |
| `chillax.herdr.start` | runs `chillax` in the focused pane |
| `chillax.herdr.dashboard` | opens the web UI of the project in the focused pane's directory |
| `chillax.herdr.switch-project` | asks which running project, then creates or focuses its herdr workspace |
| `chillax.herdr.open-employee` | asks which employee, then opens a shell in their worktree beside the pane |

Bind one in herdr's config:

```toml
[[keys.command]]
key = "prefix+c"
type = "plugin_action"
command = "chillax.herdr.open-employee"
description = "open a Chillax employee"
```

More in `docs/herdr.md` and `docs/adr/0018-herdr-integration.md`.
