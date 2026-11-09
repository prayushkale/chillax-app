# Chillax

> *chill* + *relax*. You are the stakeholder. Chillax runs the company.

Chillax is a terminal-first manager for an AI product team. You hire agent employees
(Project Manager, developers, QA, …), each backed by a coding agent such as Claude Code
driven over the [Agent Client Protocol](https://agentclientprotocol.com). You talk to your
PM, who plans epics, stories and a sprint in a built-in tracker. You approve the sprint and
watch everything live, in the terminal or the browser. Chillax takes approved tickets
through dev, QA and release on its own.

Website: [chillax.prayush.dev](https://chillax.prayush.dev)

Watch it work:

- [Short cut (about a minute)](https://youtu.be/bxLfwfotHdg)
- [Full walkthrough (7:32)](https://youtu.be/UrzEE3ePaiY)

## Status and trade-offs

Chillax is early software, built by one person. The full loop (hire, plan, build, review,
QA, release) works and has been run end to end with real agents, but only the pass path
has had real-world use; failure handling is covered by tests, not by much production time.

- **Real agent sessions spend tokens** on your Claude Code plan or API key. Set
  `max_concurrent_agents` in `.chillax/chillax.toml` (default 3).
- **Agents run with access to your repository.** Chillax does its work in separate git
  worktrees, but the agents run commands. Read the permission
  prompts in the Inbox.
- macOS and Linux only; Windows is not supported.
- The API (`/api/v1`) and event schema are additive-only, but the project has no tagged
  releases yet. Install from `main`.

## Install

You need:

- Rust 1.94 or newer
- git
- Node.js (the Claude Code adapter is fetched with `npx`)
- a Claude Code login, or `ANTHROPIC_API_KEY`

```sh
cargo install --git https://github.com/prayushkale/chillax-app --locked chillax
chillax doctor     # checks git, config, roles, and runs an ACP handshake per agent
chillax            # TUI + web UI at http://127.0.0.1:4747
```

Run these inside the project you want a team on (any folder; there is no setup step).
The first `chillax` command in a folder sets it up (ADR 0020): it creates a git repository
with a first commit if there is none, `.chillax/`, gitignore entries and the `dev`/`qa`
branches, and prints one notice line to stderr. That includes read-only commands such as
`doctor` and `team`. It refuses your home directory and `/`. `chillax init` is optional; use
it to set a folder up on purpose or to recreate missing pieces.

The first `chillax doctor` downloads the Claude adapter, which can take a minute. If a
download was interrupted and the adapter fails with a `SyntaxError`, delete
`~/.npm/_npx` and run the doctor again.

## Try it on a sample project

```sh
mkdir hello-chillax && cd hello-chillax
chillax doctor
chillax
```

1. **Meet your PM.** The first start of a new project hires a Project Manager for you.
   No agent process runs until you chat or there is work. To hire more people, press `h`
   or run `chillax hire <role> --name <name>`. Set `default_hires` under `[team]` in
   `.chillax/chillax.toml` to change who is hired automatically (`default_hires = []`
   opts out).
2. **Talk.** Press `3` to open Chat, then `i` to type. Describe what you want to build,
   for example "a command line tool that prints a greeting".
3. **Plan.** The PM uses Chillax's MCP tools to create epics, stories with acceptance
   criteria, and a proposed sprint.
4. **Approve.** Press `7` to open the Inbox. Press `a` to approve the sprint, pick a
   numbered option to answer a permission request, or press Enter to answer a question.
   Approved tickets move to `ready` on the Board (`4`).
5. **Watch.** Open the same views in the browser. Events (`8`) shows the raw event log.

### Supported agents

Each employee runs on one agent, chosen when you hire them. Chillax knows how to launch:

| Agent | Command | Notes |
|---|---|---|
| Claude Code | `npx -y @agentclientprotocol/claude-agent-acp` | The default, and the one the loop is tested with |
| Codex | `codex-acp` | Needs the adapter on your `PATH` |
| Gemini CLI | `gemini --acp` | |
| Command Code | `command-code acp` | |

Any other ACP agent can be added under `[agents.<name>]` in `.chillax/chillax.toml`.

### Without spending tokens

The repo builds a scripted agent, `fake-acp-agent`, that echoes your messages. It also
demonstrates a tool call ("use a tool") and a permission request ("ask permission"):

```sh
git clone https://github.com/prayushkale/chillax-app && cd chillax-app
cargo build --workspace
```

Add it as a backend in your project's `.chillax/chillax.toml`, then hire with it:

```toml
[agents.fake]
command = "/path/to/chillax-app/target/debug/fake-acp-agent"
```

```sh
chillax hire project-manager --name Echo --backend fake
```

## Commands

| Command | What it does |
|---|---|
| `chillax` | TUI + web server + agent runtime for the current repo |
| `chillax init` | Optional (any command does this on first run). Create `.chillax/` (config, workflow, roles), gitignore entries, `dev`/`qa` branches |
| `chillax doctor [--timeout S]` | Check git, config, roles, port, and run an ACP `initialize` handshake per backend |
| `chillax serve --headless` | Web server + engine, no TUI |
| `chillax hire <role> [--name N] [--backend B] [--model M]` | Hire, through the running server if there is one |
| `chillax team` / `chillax roles` | List employees / roles |
| `chillax pause` / `chillax resume` | Stop the dev loop (no new work, ticket sessions closed) / let it run |

## The dev loop

Approve a sprint and assign its tickets, and Chillax takes them to `dev` on its own:
each ticket gets its own git worktree and branch, a developer builds it, Chillax runs
`[env.dev] test`, a Tech Lead reviews the diff, and approved branches are merged into
`dev` (in a separate worktree, so your checkout is never touched). Watch it on the Live
tab, pause it with Shift+P. Set `test` under `[env.dev]` in `.chillax/chillax.toml`;
without it, tests count as skipped. See [ADR 0006](docs/adr/0006-dev-loop.md).

## How it fits together

```
TUI ─┐                       ┌─► ACP agents (claude-agent-acp, …), one process per employee
     ├─ commands ─► core ────┤        │
Web ─┘  (/api/v1)   │        │        └─ MCP over HTTP (/mcp, per-employee token)
                    ▼        │               │
            SQLite event log └── tracker ◄───┘  (built-in local tracker)
```

- Every state change is an event in `.chillax/state/chillax.db`. State is rebuilt by
  replaying it, so restarts keep your team, chats, tickets and inbox.
- The web API (`/api/v1/state`, `/api/v1/events` as SSE, `POST /api/v1/commands`,
  `/api/v1/openapi.json`) is the public contract for themes.
- Roles are markdown files (`roles/*.md`). Put a file with the same `id` in
  `.chillax/roles/` to override a built-in role.
- The workflow lives in `.chillax/workflow.yaml`.

## Developing

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

See [CONTRIBUTING.md](CONTRIBUTING.md), [AGENTS.md](AGENTS.md) for conventions, and
[docs/adr](docs/adr/README.md) for the design decisions.

## License

[MIT](LICENSE)
