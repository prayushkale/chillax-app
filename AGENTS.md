# AGENTS.md

Guidance for people and coding agents working on the Chillax codebase. (The prompts for the
agent "employees" Chillax manages live in `roles/`.)

Chillax is a terminal-first manager for an AI product team. You are the stakeholder; Chillax
hires agent employees, drives them over the Agent Client Protocol (ACP), tracks work in a
built-in tracker, and moves code through dev, QA and prod.

## Layout

Cargo workspace (edition 2024, MSRV 1.94). One crate per concern under `crates/`:
`chillax-core` (domain, engine, no IO), `chillax-store` (SQLite event log),
`chillax-agents` (ACP client), `chillax-tracker`, `chillax-catalog`, `chillax-vcs`,
`chillax-dispatch` (dev and QA loops), `chillax-mcp`, `chillax-server` (axum `/api/v1`),
`chillax-tui` (Ratatui) and `chillax-cli` (the `chillax` binary). Built-in roles are in
`roles/`, the default web theme in `web/themes/default/`, and design decisions in
`docs/adr/`.

## Build, test, lint

```sh
cargo build
cargo test --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

All four must pass before a change is done. The web theme has no build step; edit
`web/themes/default/` and rebuild (it is embedded with `rust-embed`).

## Architecture rules

1. `chillax-core` has no UI, HTTP, SQL or process-spawning dependencies.
2. Commands in, events out. Every state change is a persisted, broadcast event; frontends
   send commands and render events.
3. Integrations sit behind adapter traits (`Tracker`, `AgentBackend`, `VcsHost`,
   `Environment`, `Notifier`).
4. Agents reach the tracker and company only through the Chillax MCP tools.
5. `/api/v1` and the event schema are a public contract. Changes are additive.
6. Roles and workflow are data (`roles/*.md`, `workflow.yaml`), not hardcoded in Rust.
7. No secrets in config files or logs.
8. Shell out to `git` for repo operations.

## Conventions

- `thiserror` in library crates, `anyhow` only in the binary. No `unwrap()` outside tests.
- `tokio` for async; never block the runtime. `tracing`, never `println!` in library code.
- Tests never call a real agent, spend API credits or touch remote services. Git logic is
  tested in temporary repositories.
- Keep changes small, one concern per commit. New architectural decisions get a short ADR
  in `docs/adr/`.
