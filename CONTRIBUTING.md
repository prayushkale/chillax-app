# Contributing to Chillax

Thanks for looking. Chillax is a Rust workspace; see [AGENTS.md](AGENTS.md) for the layout
and the architecture rules, and [docs/adr](docs/adr/README.md) for the decisions behind them.

## Setup

You need Rust 1.94+ and git. Node is only needed to run the Claude Code ACP adapter.

```sh
git clone https://github.com/prayushkale/chillax-app
cd chillax-app
cargo build
cargo test --workspace
```

## Before you open a pull request

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI runs the same three on Ubuntu and macOS. Tests never call a real agent or spend API
credits; the agent runtime is tested against the scripted `fake-acp-agent`.

## Pull requests

- Keep each pull request to one concern, with a test for the behaviour it changes.
- Changes to `/api/v1` and the event schema must be additive.
- A new architectural decision gets a short ADR in `docs/adr/`.
- Open an issue first for anything large.

Windows is not supported yet.

## Security

Report vulnerabilities privately; see [SECURITY.md](SECURITY.md).
