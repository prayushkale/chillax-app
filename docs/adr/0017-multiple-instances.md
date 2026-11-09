# ADR 0017: Several projects at once

Status: accepted (2026-10-24)

## Context

Every project defaulted to `127.0.0.1:4747`. A second `chillax` in another repository either
failed ("already running": the health check answered for any project) or, worse, a command such
as `chillax hire` in project B was posted to project A's server, because the CLI only asked
"does something answer on my port?".

## Decision

- **One process per project, one port per process.** Each project keeps its own event log,
  agents and apps; nothing is shared between projects at runtime. A multi-company daemon was
  rejected: it would put every project's agents, worktrees and credentials in one process, make
  one crash or panic hit all of them, and need a project id on every command and event. Projects
  are therefore not a field on events or commands, and the event schema is unchanged.
- **Port selection.** Without `[server] port` (or with the old default `port = 4747`), Chillax
  binds 4747, then the next free port up to 4797. Binding is the check (no probe-then-bind
  race). An explicit other port is used as given: when taken, startup fails naming the holder
  ("port N is in use by project X (root); pick another [server] port"). `chillax init` no longer
  writes the port.
- **Identity in `/api/v1/health`.** The body gains `project: {name, root}` (additive; the root
  is canonical). `identity(url, root)` is `Absent`, `Same` or `Other`. A server without
  `project` (older build) is never `Same`. Startup, `find`, `send_offline_or_live`, `doctor` and
  every command that talks to a running server go through this check, so project B never talks
  to project A.
- **Registry.** `$CHILLAX_HOME/instances` (default `~/.chillax/instances`), one file
  `<first 16 hex of sha256(canonical root)>.json` per project holding `{root, name, url, pid,
  started_at}`. It lives outside every repository, is written atomically (temp file + rename;
  directory mode 0700 on Unix) after the listener is bound, and removed on quit, SIGTERM/SIGHUP
  and panic (only if the file is still ours: same url and pid). A crash or SIGKILL leaves a
  stale file; `list()` probes every entry's health (300 ms) and keeps only those whose server
  answers as that entry's project, deleting entries whose port refuses connections or now
  belongs to another project. The pid is display-only; liveness is the probe. Without a home
  directory the registry is disabled with a warning and commands fall back to the configured
  address.
- **Finding the instance.** `find(root)` is the registry entry for the root if it answers as
  that project, else the configured address with the same check.
- **Switching.** `GET /api/v1/instances` returns `{current, instances}` from the registry
  (`current` is this server's project root) and `chillax projects [--json]` prints the same.
  The web switches by linking to another instance's URL (its own origin, its own SSE stream):
  a project menu in the sidebar, shown only while more than one instance runs, refreshed every
  15 s and on focus. The TUI has an overlay on `O` (`P` is pause) that lists the same instances
  and opens the chosen dashboard in the browser.
- **`chillax_core` stays IO-free.** It has `InstanceInfo` and the `InstanceSource` trait; the
  file IO and probing are in `chillax-cli`.

## Future work

The TUI cannot attach to another instance yet: it is wired in-process to a `Hub`. Attaching
means a remote `CompanyHandle` over REST + SSE, which is a feature of its own. Until then the
TUI lists instances and opens their dashboards.

## Consequences

- Two projects can run side by side with no configuration; each prints its own URL.
- An old Chillax on 4747 is `Other` for every new project, which therefore starts on 4748.
- A project that pins `[server] port` keeps today's behaviour, with a clearer error.
