//! A hand-written OpenAPI description of `/api/v1`. Event and command payloads are
//! documented by their `type` tags; generated schemas arrive with the TS SDK (M5).

use axum::Json;
use serde_json::{Value, json};

pub async fn spec() -> Json<Value> {
    Json(json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Chillax API",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "Public contract for Chillax themes. Changes are additive within v1."
        },
        "servers": [{"url": "/api/v1"}],
        "paths": {
            "/health": {"get": {"summary": "Liveness, API version and (additive) the `project` this server runs: `name` and canonical `root`", "responses": {"200": {"description": "OK", "content": {"application/json": {"schema": {"type": "object", "properties": {"status": {"type": "string"}, "version": {"type": "string"}, "api": {"type": "string"}, "project": {"type": "object", "properties": {"name": {"type": "string"}, "root": {"type": "string"}}}}}}}}}}},
            "/instances": {"get": {"summary": "Chillax instances running on this machine, one per project, from the shared registry (`~/.chillax/instances`). `current` is this server's project root (null when unknown); each instance has `root`, `name`, `url`, `pid` (display only) and `started_at` (RFC 3339). Dead entries are pruned. Switch projects by linking to an instance's `url`", "responses": {"200": {"description": "Instances", "content": {"application/json": {"schema": {"type": "object", "properties": {"current": {"type": ["string", "null"]}, "instances": {"type": "array", "items": {"type": "object", "required": ["root", "name", "url", "pid", "started_at"], "properties": {"root": {"type": "string"}, "name": {"type": "string"}, "url": {"type": "string"}, "pid": {"type": "integer"}, "started_at": {"type": "string"}}}}}}}}}}}},
            "/state": {"get": {"summary": "Snapshot of the company projection (team, chats, inbox, epics, tickets, sprints). Tickets carry dev-loop fields (`branch`, `worktree`, `reviewer`, `checks`, `blocked_by`, `needs_rebase`, `review_requested`, `conflict` ({files, attempts, briefed} while the assignee resolves a conflict with dev), and `pr_number` / `pr_url` once a GitHub pull request is opened); `reported_apps` lists the apps employees reported running (see ReportedApp; absent when none); `dispatch_paused` says whether the dev loop is paused; `backend_limits` maps each rate-limited agent backend to its `until` (reset time, if the agent said) and `message`; `qa` is the open QA round (or the last one), `qa_requested` says a promotion to qa was asked for, `releases` lists every release candidate and `env_retry` the environment step a retry was requested for; tickets filed by QA carry `found_in` (the story); a sprint ends as `released` (or `closed` when the stakeholder closes it early); a dropped ticket carries `dropped` (the reason) and `planning` holds the pending request for the next sprint; `requests` lists the stakeholder's requests (`id`, `text`, `start_now`, `status` submitted, briefed, triaged or abandoned, `attempts`, `tickets`, `summary`); `apps` lists the dev, QA and prod apps Chillax runs from `run` and `port` under `[env.*]` (`env`, `port`, `url`, `status` starting, running, crashed or stopped, `wanted` (false once stopped by you), `commit`, `since`, `reason` when not running, `restarts`); ticket comments may carry `attachments` ({id, name, mime, bytes}, served at `/attachments/{id}`); `live` maps each employee to the plan and recent tool calls the Agent live view shows (each call has `tool_call_id`, `title`, `tool_kind` (additive: read, edit, execute, … or absent), `status` and `content`; up to 200 calls are kept, and the oldest calls lose their `content` once a session holds more than 512 KiB of it); `usage` holds the raw token and cost projection (see `/usage` for the derived view); `company_chat` is the company chat feed (see /company-chat; absent when empty)", "responses": {"200": {"description": "State JSON"}}}},
            "/usage": {"get": {
                "summary": "Token and cost usage over the whole event log, with the derivations themes would otherwise repeat: windows (today in UTC, 7 and 30 days), zero-filled daily and hourly series, burn per day, per employee (with `state`: ok, reports_nothing, no_cost or idle), tickets (all of them unless `top` is given), per model, live context windows and the last 20 samples. Costs are millionths of a USD; other currencies stay in `other_currency_micros`. Not every agent reports usage. Out-of-range parameters are clamped",
                "parameters": [
                    {"name": "days", "in": "query", "schema": {"type": "integer", "minimum": 1, "maximum": 90, "default": 14}},
                    {"name": "top", "in": "query", "schema": {"type": "integer", "minimum": 1, "maximum": 1000, "description": "Cap on the tickets listed; omit it for every ticket"}}
                ],
                "responses": {"200": {"description": "UsageView", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/UsageView"}}}}}
            }},
            "/usage/refresh": {"post": {
                "summary": "Record what live agent sessions have reported but not yet recorded (cost so far this turn), then return the fresh derived view, same shape and query parameters as `GET /usage`. Tokens arrive when a turn ends, so a running turn's tokens are not included. Waits up to 5 seconds for the flush",
                "parameters": [
                    {"name": "days", "in": "query", "schema": {"type": "integer", "minimum": 1, "maximum": 90, "default": 14}},
                    {"name": "top", "in": "query", "schema": {"type": "integer", "minimum": 1, "maximum": 1000, "description": "Cap on the tickets listed; omit it for every ticket"}}
                ],
                "responses": {"200": {"description": "UsageView", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/UsageView"}}}}}
            }},
            "/roles": {"get": {"summary": "The role catalog (adopted roles carry `base_role` and `source`; `tags` and `owns_states` say who works which state)", "responses": {"200": {"description": "Role list"}}}},
            "/catalog": {"get": {"summary": "Persona catalog status (commit, persona count, last refresh, error)", "responses": {"200": {"description": "CatalogStatus"}, "404": {"description": "Catalog disabled"}}}},
            "/catalog/refresh": {"post": {"summary": "Fetch the latest persona catalog (keeps the cache on failure)", "responses": {"200": {"description": "CatalogStatus"}, "502": {"description": "Refresh failed"}}}},
            "/personas": {"get": {
                "summary": "Featured personas when `q` is empty, otherwise search results; `all=true` lists every persona",
                "parameters": [
                    {"name": "q", "in": "query", "schema": {"type": "string"}},
                    {"name": "all", "in": "query", "schema": {"type": "boolean", "default": false}},
                    {"name": "limit", "in": "query", "schema": {"type": "integer", "minimum": 1, "maximum": 500, "default": 10}}
                ],
                "responses": {"200": {"description": "PersonaSummary list"}, "503": {"description": "Catalog not fetched yet"}}
            }},
            "/drafts": {"post": {
                "summary": "Start reviewing a persona on top of a trusted base role, or (with `from_role`) a new role copied from an existing one; the copy works with the catalog disabled",
                "requestBody": {"required": true, "content": {"application/json": {"schema": {"type": "object", "description": "Give `persona`, or `from_role` with `role_id` and `title`", "properties": {"persona": {"type": "string"}, "from_role": {"type": "string", "description": "Existing role (built-in, project or adopted) whose prompt becomes the draft's persona text"}, "role_id": {"type": "string", "description": "Id for the new role (with from_role)"}, "title": {"type": "string", "description": "Title for the new role (with from_role)"}, "base_role": {"type": "string", "description": "Trusted role to inherit policy from; defaults to the persona's suggested_base"}, "backend": {"type": "string", "description": "Defaults to the stakeholder's hiring default, then the base role's"}, "model": {"type": "string"}}}}}},
                "responses": {"200": {"description": "RoleDraft (text, findings, diff, composed prompt, and `project_files`: the project's plan files the prompt points at)"}, "404": {"description": "Unknown persona or base role"}}
            }},
            "/drafts/{id}": {
                "get": {"summary": "A draft", "responses": {"200": {"description": "RoleDraft"}}},
                "delete": {"summary": "Discard a draft", "responses": {"204": {"description": "Discarded"}}}
            },
            "/drafts/{id}/revise": {"post": {
                "summary": "Have an agent (no tools, no repo access) apply an instruction to the persona text; can take minutes",
                "requestBody": {"required": true, "content": {"application/json": {"schema": {"type": "object", "required": ["instruction"], "properties": {"instruction": {"type": "string"}}}}}},
                "responses": {"200": {"description": "RoleDraft"}, "502": {"description": "Editor failed"}}
            }},
            "/drafts/{id}/tailor": {"post": {
                "summary": "Have the editor agent tailor the persona to the project, from bounded excerpts of its `project_files` (passed as data); recorded as a revision; can take minutes",
                "responses": {"200": {"description": "RoleDraft"}, "404": {"description": "Unknown draft"}, "502": {"description": "Editor failed, or no project files to tailor to"}}
            }},
            "/drafts/{id}/base": {"post": {
                "summary": "Move the draft onto another trusted base role, keeping its revisions",
                "requestBody": {"required": true, "content": {"application/json": {"schema": {"type": "object", "required": ["base_role"], "properties": {"base_role": {"type": "string"}}}}}},
                "responses": {"200": {"description": "RoleDraft"}, "404": {"description": "Unknown draft or base role"}}
            }},
            "/drafts/{id}/undo": {"post": {"summary": "Drop the latest revision", "responses": {"200": {"description": "RoleDraft"}, "409": {"description": "Nothing to undo"}}}},
            "/drafts/{id}/adopt": {"post": {
                "summary": "Adopt the draft as a project role, optionally hiring into it",
                "requestBody": {"content": {"application/json": {"schema": {"type": "object", "properties": {
                    "role_id": {"type": "string"}, "overwrite": {"type": "boolean"},
                    "hire": {"type": "object", "properties": {"name": {"type": "string"}, "backend": {"type": "string"}, "model": {"type": "string"}, "effort": {"type": "string", "description": "Reasoning effort (thought level), e.g. `high`; kept only if the agent offers it"}}}
                }}}}},
                "responses": {"200": {"description": "{\"role\": {...}, \"events\": [...]}"}, "422": {"description": "Rejected"}}
            }},
            "/agents": {"get": {"summary": "Agent backends to hire on: configured `[agents.*]` first, then installed presets (`configured: false`) that `enable` adds", "responses": {"200": {"description": "BackendInfo list"}, "404": {"description": "No agent directory"}}}},
            "/agents/{name}/models": {"get": {"summary": "Models a backend offers (ACP session config option of category `model`); the first call starts the agent without prompting it", "responses": {"200": {"description": "ModelOption list ({id, name, description})"}, "404": {"description": "Unknown backend"}, "502": {"description": "The agent failed to start or isn't signed in"}}}},
            "/agents/{name}/efforts": {"get": {"summary": "Effort (thought level) choices a backend offers (ACP session config option of category `thought_level`), probed with the agent's default model; a bare array, empty when the agent has none", "responses": {"200": {"description": "ModelOption list ({id, name, description})"}, "404": {"description": "Unknown backend"}, "502": {"description": "The agent failed to start or isn't signed in"}}}},
            "/agents/{name}/enable": {"post": {"summary": "Add a known preset (claude, codex, gemini) to chillax.toml", "responses": {"200": {"description": "BackendInfo"}, "404": {"description": "Not a known preset"}}}},
            "/company-chat": {"post": {
                "summary": "Post in the company chat (same as the `post_company_chat` command; ADR 0040). `to` omitted or null posts to everyone: no agent is woken except those `@mentioned` (`@id` or `@FirstName`; `@all` names nobody); the others see it in their next prompt. With `to` it is a normal chat message to that employee. The feed is `company_chat` in /state: entries {seq, ts, from (absent = stakeholder), from_name, to (absent = everyone), mentions, text}, the newest 500, covering stakeholder and employee messages",
                "requestBody": {"required": true, "content": {"application/json": {"schema": {"type": "object", "required": ["text"], "properties": {"text": {"type": "string"}, "to": {"type": ["string", "null"]}}}}}},
                "responses": {"200": {"description": "{\"seq\": number}, the committed event"}, "422": {"description": "Empty text or unknown `to`"}}
            }},
            "/employees/{id}/chat/clear": {"post": {
                "summary": "Empty the chat with one employee and end their chat session so the next message starts fresh (same as the `clear_chat` command; ticket sessions are untouched)",
                "responses": {"200": {"description": "Committed events: {\"events\": [...]}"}, "422": {"description": "Unknown employee, or they are replying right now"}}
            }},
            "/employees/{id}/cancel": {"post": {
                "summary": "Stop the employee's current turn but keep their session and context (same as the `cancel_turn` command). The dispatcher gives them no new work until the stakeholder messages them again. Does nothing (200, `events: []`) when they are not working",
                "responses": {"200": {"description": "Committed events: {\"events\": [...]}"}, "422": {"description": "Unknown employee"}}
            }},
            "/tickets/{id}/prioritize": {"post": {
                "summary": "Mark a work item urgent so the dispatcher takes it up first (same as the `prioritize_ticket` command). The optional JSON body `{\"urgent\": false}` clears the mark; the default is true. Sprint approval and blockers still apply, and running work is not pre-empted. Does nothing (200, `events: []`) when the flag already has that value",
                "requestBody": {"required": false, "content": {"application/json": {"schema": {"type": "object", "properties": {"urgent": {"type": "boolean", "default": true}}}}}},
                "responses": {"200": {"description": "Committed events: {\"events\": [...]}"}, "422": {"description": "Unknown ticket, or it is dropped or finished"}}
            }},
            "/budget/max-concurrent-agents": {"put": {
                "summary": "Change how many agents work at once (same as the `set_max_concurrent_agents` command; POST works too). Body `{\"n\": 4}`. `n` must be 1 up to this machine's maximum (`agent_limit.hardware_max` in /state). Takes effect at once and is saved to `[budget] max_concurrent_agents`; lowering it never stops a running agent. Does nothing (200, `events: []`) when the limit already is `n`",
                "requestBody": {"required": true, "content": {"application/json": {"schema": {"type": "object", "required": ["n"], "properties": {"n": {"type": "integer", "minimum": 1}}}}}},
                "responses": {"200": {"description": "Committed events: {\"events\": [...]}"}, "422": {"description": "`limit must be 1..=N`, or a body that is not {\"n\": integer}"}}
            }},
            "/attachments": {"post": {
                "summary": "Upload an image (png, jpeg, gif or webp; at most 10 MB) for a comment. The body is the raw image; `X-Filename` names it (percent-encoded). The type is read from the bytes. Returns the Attachment ({id (sha256), name, mime, bytes}) to send in a `comment_ticket` command's `attachments` (at most 6)",
                "requestBody": {"required": true, "content": {"image/*": {"schema": {"type": "string", "format": "binary"}}}},
                "responses": {"200": {"description": "Attachment"}, "413": {"description": "Empty or over 10 MB"}, "415": {"description": "Not an allowed image"}, "404": {"description": "No attachment store"}}
            }},
            "/attachments/{id}": {"get": {
                "summary": "The image, with its content type, `nosniff` and a locked-down CSP; `id` is the 64-character lowercase hex SHA-256",
                "responses": {"200": {"description": "Image bytes"}, "404": {"description": "Unknown id"}}
            }},
            "/events": {"get": {
                "summary": "Server-sent events: replay after `since`, then live",
                "parameters": [{"name": "since", "in": "query", "schema": {"type": "integer", "minimum": 0}}],
                "responses": {"200": {"description": "text/event-stream; each message is `event: chillax`, `id: <seq>`, data = event JSON with a `type` tag"}}
            }},
            "/commands": {"post": {
                "summary": "Send a command as the stakeholder",
                "requestBody": {"required": true, "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Command"}}}},
                "responses": {
                    "200": {"description": "Committed events: {\"events\": [...]}"},
                    "422": {"description": "Rejected: {\"error\": \"...\"}"}
                }
            }}
        },
        "components": {"schemas": {
            "Command": {
                "type": "object",
                "required": ["type"],
                "properties": {"type": {"type": "string", "enum": [
                    "hire_employee", "hire_teammate", "report_app", "let_go", "set_hiring_defaults", "set_employee_agent", "adopt_role", "send_chat", "post_company_chat", "resolve_permission",
                    "answer_question", "create_epic", "create_ticket", "update_ticket",
                    "assign_ticket", "transition_ticket", "comment_ticket", "propose_sprint",
                    "approve_sprint", "reject_sprint", "clear_activity", "clear_chat", "clear_employee_context", "cancel_turn", "prioritize_ticket",
                    "set_max_concurrent_agents", "set_blockers", "handoff", "request_review", "submit_review",
                    "pause_dispatch", "resume_dispatch", "kick", "refresh_usage",
                    "promote_to_qa", "retry_env", "open_release_gate", "approve_release",
                    "reject_release", "dismiss_alert",
                    "drop_ticket", "request_sprint_planning", "close_sprint", "submit_request", "suggest_work",
                    "start_app", "stop_app", "restart_app",
                    "resolve_problem", "retry_action", "restart_employee"
                ]}},
                "description": "Commands are sent as the stakeholder. post_company_chat ({text, to?}; stakeholder-only) posts in the company chat and emits company_chat_posted ({text, mentions}) when `to` is absent, or chat_message_sent when it is set. hire_employee ({role, name?, backend?, model?, effort?}; stakeholder-only) hires directly; an absent or blank name defaults to the first unused famous tech name. hire_teammate ({role, name?, reason}) is sent by an employee whose role is in `[team] hiring_roles` (default: the project manager): it hires directly with no approval, refuses roles that may hire themselves and a team at `max_team_size` (default 8; the stakeholder's hire_employee is not capped), and emits employee_hired and alert_raised (the inbox notice, detail = reason). set_employee_agent ({employee, when_idle?, backend?, model?, effort?}; stakeholder-only; model and effort absent = backend default; refused while the employee is replying unless `when_idle` is true, which queues the change and emits employee_agent_change_queued (employee, backend, model?, effort?; the employee's `pending_agent` {backend, model?, effort?} in /state until it is applied or replaced; asking for the current agent withdraws it); ends their sessions) changes the agent of an existing hire and emits employee_agent_changed. A queued change is applied by Chillax itself (the system-only apply_queued_agent_change command) when the employee's turn ends. clear_chat ({employee}) empties one employee's chat and ends their chat session (stakeholder-only; refused while they are replying). clear_employee_context ({employee, ticket}; Chillax only) empties an employee's live transcript once their ticket work is done and emits employee_context_cleared (employee, ticket); the event log keeps the history and no session is closed (ADR 0008). cancel_turn ({employee}) stops an employee's current turn and keeps their session and context (stakeholder-only; a no-op when they are not working); it emits turn_cancelled, and the dispatcher gives them no new work until the stakeholder next messages them. set_max_concurrent_agents ({n}; stakeholder-only; refused unless 1 <= n <= the machine maximum, \"limit must be 1..=N\"; a no-op when it already matches) changes the agent limit live and emits max_concurrent_agents_changed ({n, hardware_max}; Chillax also records it at start-up from `chillax.toml`); /state carries `agent_limit` {limit, hardware_max}. prioritize_ticket ({id, urgent}; stakeholder-only; refused for a dropped or finished ticket; a no-op when the flag already matches) marks a work item urgent or clears the mark and emits ticket_prioritized, plus an inbox alert when turning it on; the dispatcher takes urgent tickets first, hands one to a free developer when its assignee is full or held, and still honours sprint approval and blockers. report_app ({env?, url?, port?, ticket?, note?, down?}; employee-only, localhost URLs only: it adds or replaces the reporter's app for that label, `down` removes it, and it emits app_reported) tells the stakeholder where an app an employee started runs. start_app, stop_app and restart_app ({env}) control the dev, QA and prod apps (stakeholder-only); configure_apps and record_app_started, record_app_stopped, record_app_crashed and record_app_restarted are Chillax only. The dev loop also uses start_work, record_tests, record_pr, start_review, record_merge, record_merge_failed and record_rebase, which only Chillax itself may send. QA and release add record_promotion, record_env_run, close_qa_round, record_release, record_deploy and raise_alert (Chillax only), and submit_qa_verdict (an employee that owns in_qa). promote_to_qa and dismiss_alert may come from the stakeholder or Chillax; retry_env, open_release_gate, approve_release and reject_release are stakeholder-only. kick (refresh the dispatcher: close idle agent sessions, forget cooldowns and backoff, probe rate-limited backends now) is stakeholder-only; record_backend_limit and record_backend_recovered are Chillax only. Work-item housekeeping adds drop_ticket (won't do: the ticket leaves the board and its sprint, is unassigned and its sessions end; refused once its code is in dev), close_sprint (an active sprint that will not finish: unfinished tickets leave it) and request_sprint_planning (asks the project manager to propose the next sprint; refused while a sprint is proposed or active), all stakeholder-only, plus record_planning_briefed (Chillax only); update_ticket also takes an epic. Failure triage adds record_triage_briefed and abandon_triage (Chillax only) and link_triage_bug (the project manager or Chillax; refused when the failure already has an open bug). Request intake adds submit_request ({text, start_now}; stakeholder-only: the project manager files it as tickets and assigns each; with start_now the tickets join the active sprint, or open and approve one, as ready), record_request_briefed and abandon_request (Chillax only), and link_request_ticket and complete_request (the project manager or Chillax; refused once the request is triaged). suggest_work ({kind: bug|improvement, text, ticket?}; employee-only) files a request from the employee, emitting request_submitted with `from`, `kind` and `ticket` set; an employee may have `[team] max_open_suggestions` (default 3) waiting, and a bug ticket filed for it starts in the sprint while an improvement waits in the backlog. Requests in /state carry the same optional `from`, `kind` and `ticket`. Problem handling (ADR 0032) adds resolve_problem ({key, summary}), retry_action ({key}: try a failed action again now, emits action_retry_requested) and restart_employee ({employee}: ends the sessions and clears a failure, emits employee_restarted, then agent_stopped when a session ended), all sent by an employee whose role owns the first workflow state (the project manager) and refused for anyone else; restart_employee is also refused for a stopped employee, for the sender and for an unknown employee. resolve_problem emits problem_resolved and an alert_raised note (\"PM handled: <summary>\") for the stakeholder.",
                "additionalProperties": true
            },
            "EnvStep": {"type": "string", "enum": ["up", "test", "down", "deploy"]},
            "QaVerdict": {"type": "string", "enum": ["pass", "fail", "env"]},
            "QaRound": {
                "type": "object",
                "description": "One QA pass over the tickets waiting in in_qa",
                "properties": {
                    "number": {"type": "integer"},
                    "sprint": {"type": "string"},
                    "commit": {"type": "string", "description": "The qa commit under test"},
                    "status": {"enum": ["starting", "testing", "env_failed", "closed"]},
                    "tickets": {"type": "array", "items": {"type": "string"}},
                    "verdicts": {"type": "object", "description": "Latest verdict per ticket"},
                    "env_retries": {"type": "object"},
                    "up_done": {"type": "boolean"}
                },
                "additionalProperties": true
            },
            "ReportedApp": {
                "type": "object",
                "description": "An app an employee says they started, shown beside the supervised apps (`State.reported_apps`). Local URLs only; it ends with the reporter's session.",
                "properties": {
                    "label": {"type": "string", "description": "dev, qa, prod, a ticket id or free text"},
                    "url": {"type": "string"},
                    "port": {"type": "integer"},
                    "ticket": {"type": "string"},
                    "employee": {"type": "string"},
                    "worktree": {"type": "string", "description": "Relative to the repository root"},
                    "note": {"type": "string"},
                    "since": {"type": "string", "format": "date-time"}
                },
                "required": ["label", "url", "employee", "since"]
            },
            "RunningApp": {
                "type": "object",
                "description": "An app Chillax runs on a localhost port for one environment (`State.apps`)",
                "properties": {
                    "env": {"type": "string", "enum": ["dev", "qa", "prod"]},
                    "port": {"type": "integer"},
                    "url": {"type": "string", "description": "Where to open it"},
                    "status": {"type": "string", "enum": ["starting", "running", "crashed", "stopped"]},
                    "wanted": {"type": "boolean", "description": "false once the stakeholder stopped it"},
                    "commit": {"type": "string"},
                    "since": {"type": "string", "format": "date-time"},
                    "reason": {"type": "string", "description": "Why it is not running"},
                    "restarts": {"type": "integer"}
                }
            },
            "UsageTotals": {
                "type": "object",
                "properties": {
                    "input_tokens": {"type": "integer"}, "output_tokens": {"type": "integer", "description": "Includes thought tokens"},
                    "cached_tokens": {"type": "integer"}, "cache_write_tokens": {"type": "integer"},
                    "turns": {"type": "integer", "description": "Samples that carried tokens"},
                    "cost_micros": {"type": "integer", "description": "Reported USD cost, millionths"},
                    "estimated_cost_micros": {"type": "integer", "description": "USD cost estimated from [pricing], millionths"},
                    "unpriced_samples": {"type": "integer"},
                    "other_currency_micros": {"type": "object", "additionalProperties": {"type": "integer"}}
                }
            },
            "UsageView": {
                "type": "object",
                "description": "Derived token and cost usage (`GET /usage`)",
                "properties": {
                    "generated_at": {"type": "string", "format": "date-time"},
                    "days": {"type": "integer"},
                    "last_seq": {"type": "integer"},
                    "total": {"$ref": "#/components/schemas/UsageTotals"},
                    "cost": {"type": "object", "properties": {"total_micros": {"type": "integer"}, "reported_micros": {"type": "integer"}, "estimated_micros": {"type": "integer"}, "estimated": {"type": "boolean"}, "unpriced_samples": {"type": "integer"}}},
                    "windows": {"type": "object", "properties": {"today": {"$ref": "#/components/schemas/UsageTotals"}, "last_7_days": {"$ref": "#/components/schemas/UsageTotals"}, "last_30_days": {"$ref": "#/components/schemas/UsageTotals"}}},
                    "burn": {"type": "object", "properties": {"tokens_per_day_7d": {"type": "integer"}, "cost_micros_per_day_7d": {"type": "integer"}}},
                    "daily": {"type": "array", "description": "Exactly `days` UTC days, oldest first, zero-filled", "items": {"type": "object", "properties": {"date": {"type": "string"}, "input_tokens": {"type": "integer"}, "output_tokens": {"type": "integer"}, "cached_tokens": {"type": "integer"}, "cost_micros": {"type": "integer"}, "estimated_cost_micros": {"type": "integer"}, "turns": {"type": "integer"}}}},
                    "hourly": {"type": "array", "description": "The last 48 UTC hours, oldest first, zero-filled (the HourPoint series)", "items": {"type": "object", "properties": {"hour": {"type": "string", "description": "YYYY-MM-DDTHH"}, "input_tokens": {"type": "integer"}, "output_tokens": {"type": "integer"}, "cached_tokens": {"type": "integer"}, "cost_micros": {"type": "integer"}, "estimated_cost_micros": {"type": "integer"}, "turns": {"type": "integer"}}}},
                    "employees": {"type": "array", "items": {"type": "object", "properties": {"id": {"type": "string"}, "name": {"type": "string"}, "role_title": {"type": "string"}, "backend": {"type": "string"}, "model": {"type": ["string", "null"]}, "totals": {"$ref": "#/components/schemas/UsageTotals"}, "turns_total": {"type": "integer"}, "sampled_turns": {"type": "integer"}, "reports_tokens": {"type": "boolean"}, "reports_cost": {"type": "boolean"}, "reports_context": {"type": "boolean"}, "last_sample_at": {"type": ["string", "null"]}, "state": {"enum": ["ok", "reports_nothing", "no_cost", "idle"]}}}},
                    "tickets": {"type": "array", "items": {"type": "object", "properties": {"id": {"type": "string"}, "title": {"type": ["string", "null"]}, "totals": {"$ref": "#/components/schemas/UsageTotals"}}}},
                    "tickets_total": {"type": "integer"},
                    "models": {"type": "array", "items": {"type": "object", "properties": {"model": {"type": "string"}, "totals": {"$ref": "#/components/schemas/UsageTotals"}}}},
                    "context": {"type": "array", "items": {"type": "object", "properties": {"employee": {"type": "string"}, "name": {"type": "string"}, "used": {"type": "integer"}, "size": {"type": "integer"}, "percent": {"type": "integer"}, "ticket": {"type": ["string", "null"]}, "at": {"type": "string"}}}},
                    "recent": {"type": "array", "description": "Last 20 samples, newest first", "items": {"type": "object", "additionalProperties": true}}
                }
            },
            "Release": {
                "type": "object",
                "description": "A release candidate, from the gate to the tag and deploy",
                "properties": {
                    "id": {"type": "string"},
                    "version": {"type": "string", "description": "Like v1.2.3"},
                    "sprint": {"type": "string"},
                    "round": {"type": "integer"},
                    "tickets": {"type": "array", "items": {"type": "string"}},
                    "known_issues": {"type": "array", "items": {"type": "string"}},
                    "notes": {"type": "string", "description": "Markdown; editable at the gate"},
                    "status": {"enum": ["gated", "approved", "tagged", "deployed", "deploy_failed", "rejected"]},
                    "tag": {"type": ["string", "null"]},
                    "commit": {"type": ["string", "null"]}
                },
                "additionalProperties": true
            },
            "InboxItem": {
                "description": "An item in the stakeholder's inbox, tagged by `type`",
                "oneOf": [
                    {"type": "object", "properties": {"type": {"const": "permission"}}, "required": ["type"], "additionalProperties": true},
                    {"type": "object", "properties": {"type": {"const": "question"}}, "required": ["type"], "additionalProperties": true},
                    {"type": "object", "properties": {"type": {"const": "sprint_gate"}}, "required": ["type"], "additionalProperties": true},
                    {"type": "object", "description": "A qa or prod command failed; answer with retry_env or dismiss_alert", "properties": {"type": {"const": "env_alert"}, "id": {"type": "string"}, "env": {"type": "string"}, "step": {"$ref": "#/components/schemas/EnvStep"}, "output_tail": {"type": "string"}}, "required": ["type", "id"], "additionalProperties": true},
                    {"type": "object", "description": "A release waits for approval; answer with approve_release or reject_release", "properties": {"type": {"const": "release_gate"}, "id": {"type": "string"}, "release": {"type": "string"}}, "required": ["type", "id"], "additionalProperties": true},
                    {"type": "object", "description": "Something Chillax cannot fix alone; answer with dismiss_alert", "properties": {"type": {"const": "alert"}, "id": {"type": "string"}, "title": {"type": "string"}, "detail": {"type": "string"}}, "required": ["type", "id"], "additionalProperties": true}
                ]
            },
            "Event": {
                "type": "object",
                "required": ["seq", "ts", "actor", "type"],
                "properties": {
                    "seq": {"type": "integer"},
                    "ts": {"type": "string", "format": "date-time"},
                    "actor": {"type": "object", "properties": {"kind": {"enum": ["stakeholder", "employee", "system"]}, "id": {"type": "string"}}},
                    "type": {"type": "string", "description": "Includes employee_agent_change_queued and the app events apps_configured, app_reported (an employee reported or stopped an app; env, url, port?, ticket?, employee, worktree?, note?, down), app_requested, app_started, app_stopped, app_crashed and app_restarted, and the dev-loop events work_started, review_requested, tests_run, pr_opened, review_started, review_submitted, branch_merged, merge_failed, branch_rebased, branch_synced (id, outcome: merged | tests_failed | conflict | resolved | unresolved, files, note), conflict_briefed (id), ticket_blockers_set, ticket_handed_off, ticket_dropped, sprint_planning_requested, sprint_planning_briefed, sprint_closed, the request-intake events request_submitted, request_briefed, request_ticket_linked, request_triaged and request_abandoned, dispatch_paused, dispatch_resumed, dispatch_kicked, usage_refresh_requested, usage_refreshed, backend_limited, backend_recovered, the failure-triage events triage_briefed, triage_bug_filed and triage_abandoned, the live-view events agent_plan_updated and agent_tool_call_content, usage_recorded (employee, ticket?, session, model?, input_tokens, output_tokens, cached_tokens, cache_write_tokens, cost_micros? in millionths of currency, currency when not USD, estimated, context_used?, context_size?), and the QA and release events qa_promotion_requested, qa_round_started, env_command_run, qa_verdict_recorded, bug_filed, qa_round_closed, alert_raised, alert_dismissed, env_retry_requested, release_proposed, release_approved, release_rejected, release_tagged and deploy_run"}
                },
                "additionalProperties": true
            }
        }}
    }))
}
