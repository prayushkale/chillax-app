//! A scripted ACP agent for tests and demos. It never calls a model.
//!
//! It echoes the last line of every prompt. Prompts containing `use a tool` produce a
//! tool call; prompts containing `ask permission` send a `session/request_permission`
//! and report the outcome. Persona-edit prompts (`<chillax-persona-edit>`) get a
//! `<revised>` reply that quotes the instruction and the new-session request's
//! `_meta.claudeCode.options`.
//!
//! Dev-loop triggers: `write file NAME` writes NAME in the session's working directory
//! (with an edit tool call carrying a diff), `make a plan` sends a plan, `where am i`
//! reports the working directory, and `edit at PATH` asks permission for an edit that
//! touches PATH.
//!
//! Prompts containing `show prompt` also echo the whole prompt, newlines as ` | `.
//!
//! Failure triggers: prompts containing `explode` fail with a plain internal error, and
//! while the file named by the `FAKE_RATE_LIMIT_FILE` environment variable exists every
//! prompt fails with a `rate_limit` error (the file's text, if any, is the message), like
//! an account that hit its usage limit. Deleting the file lifts the limit.
//!
//! Shutdown triggers: when `FAKE_PID_FILE` names a file, the agent writes its process id
//! there on start. A prompt containing `stall forever` sends a chunk and then never finishes the
//! turn, like an agent stuck mid-turn.
//!
//! When `FAKE_CHILD_PID_FILE` names a file, the agent starts `sleep 1000` as a child and
//! writes its pid there, like a dev server launched by an agent's shell tool.
//!
//! A prompt containing `stall until cancel` reports a `usage_update` (cost), sends a chunk and
//! then waits for `session/cancel`, answering `cancelled` with the usual token usage.
//!
//! A prompt containing `finish slowly` sends a chunk and ends the turn on its own after a short
//! pause, so a test can close the session mid-turn without any cancel being needed.
//!
//! Sessions offer the modes `default` and `bypassPermissions` (`which mode` reports the
//! current one) a `model` config option (`default`, `fast`, `smart`) and an `effort` one
//! (`low`, `medium`, `high`); the selected ones are reported in replies (`which effort`). Prompts containing `sign me out` fail with `auth_required`,
//! like a signed-out agent CLI.
//!
//! Usage triggers: a prompt containing `report usage` sends a `usage_update` with a context
//! fill and a CUMULATIVE session cost that grows by 0.25 USD each time. When
//! `FAKE_USAGE_TOKENS=in,out,cached` is set, every `PromptResponse` carries token usage of
//! those counts (per turn, or running totals when `FAKE_USAGE_CUMULATIVE` is set). Without
//! them the agent reports nothing, like a backend that does not support usage.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, ContentBlock, ContentChunk, Cost, Diff, Implementation,
    InitializeRequest, InitializeResponse, McpCapabilities, NewSessionRequest, NewSessionResponse,
    PermissionOption, PermissionOptionKind, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus,
    PromptRequest, PromptResponse, RequestPermissionOutcome, RequestPermissionRequest,
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption, SessionMode,
    SessionModeState, SessionNotification, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, SetSessionModeRequest, SetSessionModeResponse, StopReason,
    ToolCall, ToolCallLocation, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
    Usage, UsageUpdate,
};
use agent_client_protocol::{Agent, Error, Result, Stdio};

/// Woken by `session/cancel`; the stalled turn of `stall until cancel` waits on it.
static CANCELLED: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// Cumulative `report usage` count (the session cost is 0.25 USD per report).
static USAGE_REPORTS: AtomicU64 = AtomicU64::new(0);
/// Running token totals, for `FAKE_USAGE_CUMULATIVE`.
static USAGE_TOTALS: Mutex<[u64; 3]> = Mutex::new([0; 3]);

/// Token usage for the prompt response, from `FAKE_USAGE_TOKENS=in,out,cached`.
fn response_usage() -> Option<Usage> {
    let spec = std::env::var("FAKE_USAGE_TOKENS").ok()?;
    let mut parts = spec
        .split(',')
        .map(|p| p.trim().parse::<u64>().unwrap_or(0));
    let turn = [
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    ];
    let shown = if std::env::var("FAKE_USAGE_CUMULATIVE").is_ok() {
        let mut totals = USAGE_TOTALS.lock().expect("usage totals lock");
        for (total, add) in totals.iter_mut().zip(turn) {
            *total += add;
        }
        *totals
    } else {
        turn
    };
    Some(Usage::new(shown[0] + shown[1], shown[0], shown[1]).cached_read_tokens(shown[2]))
}

/// The word that follows `marker` in the prompt, e.g. the file name in `write file a.txt`.
fn word_after(prompt: &str, marker: &str) -> Option<String> {
    let (_, rest) = prompt.split_once(marker)?;
    rest.split_whitespace().next().map(str::to_string)
}

fn model_option(current: &str) -> SessionConfigOption {
    let choice = |id: &str, name: &str| SessionConfigSelectOption::new(id.to_string(), name);
    SessionConfigOption::select(
        "model",
        "Model",
        current.to_string(),
        vec![
            choice("default", "Default"),
            choice("fast", "Fast"),
            choice("smart", "Smart"),
        ],
    )
    .category(SessionConfigOptionCategory::Model)
}

fn effort_option(current: &str) -> SessionConfigOption {
    let choice = |id: &str, name: &str| SessionConfigSelectOption::new(id.to_string(), name);
    SessionConfigOption::select(
        "effort",
        "Effort",
        current.to_string(),
        vec![
            choice("low", "Low"),
            choice("medium", "Medium"),
            choice("high", "High"),
        ],
    )
    .category(SessionConfigOptionCategory::ThoughtLevel)
}

#[tokio::main]
async fn main() -> Result<()> {
    if let Ok(path) = std::env::var("FAKE_PID_FILE") {
        let _ = std::fs::write(path, std::process::id().to_string());
    }
    // Stand-in for a dev server an agent's shell tool started: a child in our group.
    if let Ok(path) = std::env::var("FAKE_CHILD_PID_FILE")
        && let Ok(child) = std::process::Command::new("sleep").arg("1000").spawn()
    {
        let _ = std::fs::write(path, child.id().to_string());
    }
    let mcp_attached = Arc::new(AtomicBool::new(false));
    let session_flag = Arc::clone(&mcp_attached);
    let model = Arc::new(Mutex::new("default".to_string()));
    let set_model = Arc::clone(&model);
    let effort = Arc::new(Mutex::new("medium".to_string()));
    let set_effort = Arc::clone(&effort);
    let mode = Arc::new(Mutex::new("default".to_string()));
    let set_mode = Arc::clone(&mode);
    let cwd = Arc::new(Mutex::new(std::path::PathBuf::new()));
    let session_cwd = Arc::clone(&cwd);
    // `_meta.claudeCode.options` of the new-session request, echoed by persona edits.
    let claude_options = Arc::new(Mutex::new(String::new()));
    let session_options = Arc::clone(&claude_options);
    Agent
        .builder()
        .name("fake-acp-agent")
        .on_receive_request(
            async move |init: InitializeRequest, responder, _cx| {
                responder.respond(
                    InitializeResponse::new(init.protocol_version)
                        .agent_info(Implementation::new("fake-acp-agent", "0.1.0"))
                        .agent_capabilities(
                            AgentCapabilities::new()
                                .mcp_capabilities(McpCapabilities::new().http(true)),
                        ),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: NewSessionRequest, responder, _cx| {
                session_flag.store(!request.mcp_servers.is_empty(), Ordering::SeqCst);
                *session_cwd.lock().expect("cwd lock") = request.cwd.clone();
                *session_options.lock().expect("options lock") = request
                    .meta
                    .as_ref()
                    .and_then(|meta| meta.get("claudeCode")?.get("options"))
                    .map(ToString::to_string)
                    .unwrap_or_default();
                responder.respond(
                    NewSessionResponse::new("fake-session")
                        .modes(SessionModeState::new(
                            "default",
                            vec![
                                SessionMode::new("default", "Default"),
                                SessionMode::new("bypassPermissions", "Bypass permissions"),
                            ],
                        ))
                        .config_options(vec![model_option("default"), effort_option("medium")]),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: SetSessionModeRequest, responder, _cx| {
                *set_mode.lock().expect("mode lock") = request.mode_id.0.to_string();
                responder.respond(SetSessionModeResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: SetSessionConfigOptionRequest, responder, _cx| {
                let value = request
                    .value
                    .as_value_id()
                    .map(|v| v.0.to_string())
                    .unwrap_or_default();
                if request.config_id.0.as_ref() == "effort" {
                    if !["low", "medium", "high"].contains(&value.as_str()) {
                        return responder.respond_with_error(Error::invalid_params());
                    }
                    *set_effort.lock().expect("effort lock") = value.clone();
                    let model = set_model.lock().expect("model lock").clone();
                    return responder.respond(SetSessionConfigOptionResponse::new(vec![
                        model_option(&model),
                        effort_option(&value),
                    ]));
                }
                if !["default", "fast", "smart"].contains(&value.as_str()) {
                    return responder.respond_with_error(Error::invalid_params());
                }
                *set_model.lock().expect("model lock") = value.clone();
                let effort = set_effort.lock().expect("effort lock").clone();
                responder.respond(SetSessionConfigOptionResponse::new(vec![
                    model_option(&value),
                    effort_option(&effort),
                ]))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |_cancel: CancelNotification, _cx| {
                CANCELLED.notify_one();
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: PromptRequest, responder, cx| {
                let mcp = mcp_attached.load(Ordering::SeqCst);
                let model = model.lock().expect("model lock").clone();
                let effort = effort.lock().expect("effort lock").clone();
                let mode = mode.lock().expect("mode lock").clone();
                let cwd = cwd.lock().expect("cwd lock").clone();
                let options = claude_options.lock().expect("options lock").clone();
                let task_cx = cx.clone();
                cx.spawn(async move {
                    let cx = task_cx;
                    let session = request.session_id.clone();
                    let prompt: String = request
                        .prompt
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text(t) => Some(t.text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    let say = |text: String| {
                        cx.send_notification(SessionNotification::new(
                            session.clone(),
                            SessionUpdate::AgentMessageChunk(ContentChunk::new(text.into())),
                        ))
                    };

                    if let Ok(path) = std::env::var("FAKE_RATE_LIMIT_FILE")
                        && std::path::Path::new(&path).exists()
                    {
                        let text = std::fs::read_to_string(&path).unwrap_or_default();
                        let message = match text.trim() {
                            "" => "You've hit your session limit",
                            text => text,
                        };
                        return responder.respond_with_error(
                            Error::new(-32603, message)
                                .data(serde_json::json!({"errorKind": "rate_limit"})),
                        );
                    }

                    if prompt.contains("stall until cancel") {
                        let _ = say("working on it".to_string());
                        cx.send_notification(SessionNotification::new(
                            session.clone(),
                            SessionUpdate::UsageUpdate(
                                UsageUpdate::new(1_000, 200_000).cost(Cost::new(0.25, "USD")),
                            ),
                        ))?;
                        CANCELLED.notified().await;
                        return responder
                            .respond(PromptResponse::new(StopReason::Cancelled).usage(response_usage()));
                    }

                    if prompt.contains("finish slowly") {
                        // Like an agent still waiting on a tool result: the turn ends by
                        // itself shortly, with no cancel needed.
                        let _ = say("working on it".to_string());
                        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
                        return responder.respond(
                            PromptResponse::new(StopReason::EndTurn).usage(response_usage()),
                        );
                    }

                    if prompt.contains("stall forever") {
                        let _ = say("working on it".to_string());
                        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                    }

                    if prompt.contains("explode") {
                        return responder.respond_with_error(Error::new(-32603, "boom"));
                    }

                    if prompt.contains("sign me out") {
                        return responder.respond_with_error(Error::auth_required());
                    }

                    if prompt.contains("<chillax-persona-edit>") {
                        let instruction = prompt
                            .split_once("<instruction>")
                            .and_then(|(_, rest)| rest.split_once("</instruction>"))
                            .map_or("", |(inside, _)| inside.trim())
                            .to_string();
                        let mut changes = format!("- applied: {instruction}");
                        // Echo the project excerpts block, so tests can check what was sent.
                        if let Some((_, rest)) = prompt.split_once("PROJECT-")
                            && let Some((_, rest)) = rest.split_once(" BEGIN\n")
                            && let Some((excerpts, _)) = rest.split_once("\nPROJECT-")
                        {
                            let first = excerpts.lines().next().unwrap_or("");
                            changes.push_str(&format!(
                                "\n- project: {} bytes, first: {first}",
                                excerpts.len()
                            ));
                        }
                        if instruction.contains("ask permission") {
                            let response = cx
                                .send_request(RequestPermissionRequest::new(
                                    session.clone(),
                                    ToolCallUpdate::new(
                                        "edit-1",
                                        ToolCallUpdateFields::new()
                                            .title("Fetch a URL")
                                            .kind(ToolKind::Fetch),
                                    ),
                                    vec![PermissionOption::new(
                                        "allow",
                                        "Allow",
                                        PermissionOptionKind::AllowOnce,
                                    )],
                                ))
                                .block_task()
                                .await?;
                            let outcome = match response.outcome {
                                RequestPermissionOutcome::Selected(s) => s.option_id.0.to_string(),
                                _ => "cancelled".to_string(),
                            };
                            changes.push_str(&format!("\n- permission: {outcome}"));
                        }
                        say(format!(
                            "<revised>\n# Edited persona\n\n{instruction}\n</revised>\n\
                             <changes>\n{changes}\n- mcp: {}\n- model: {model}\n- effort: {effort}\n- options: {options}\n</changes>",
                            if mcp { "yes" } else { "no" }
                        ))?;
                        return responder.respond(PromptResponse::new(StopReason::EndTurn));
                    }

                    if prompt.contains("use a tool") {
                        cx.send_notification(SessionNotification::new(
                            session.clone(),
                            SessionUpdate::ToolCall(
                                ToolCall::new("call-1", "Read README.md").kind(ToolKind::Read),
                            ),
                        ))?;
                        cx.send_notification(SessionNotification::new(
                            session.clone(),
                            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                                "call-1",
                                ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
                            )),
                        ))?;
                    }

                    if let Some(name) = word_after(&prompt, "write file ") {
                        let path = cwd.join(&name);
                        let body = format!("{name}\n");
                        std::fs::write(&path, &body).map_err(Error::into_internal_error)?;
                        cx.send_notification(SessionNotification::new(
                            session.clone(),
                            SessionUpdate::ToolCall(
                                ToolCall::new("call-w", format!("Write {name}"))
                                    .kind(ToolKind::Edit)
                                    .content(vec![Diff::new(path.clone(), body).into()])
                                    .locations(vec![ToolCallLocation::new(path)]),
                            ),
                        ))?;
                        cx.send_notification(SessionNotification::new(
                            session.clone(),
                            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                                "call-w",
                                ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
                            )),
                        ))?;
                    }

                    if prompt.contains("make a plan") {
                        cx.send_notification(SessionNotification::new(
                            session.clone(),
                            SessionUpdate::Plan(Plan::new(vec![
                                PlanEntry::new(
                                    "write the code",
                                    PlanEntryPriority::High,
                                    PlanEntryStatus::Completed,
                                ),
                                PlanEntry::new(
                                    "run the tests",
                                    PlanEntryPriority::Medium,
                                    PlanEntryStatus::Pending,
                                ),
                            ])),
                        ))?;
                    }

                    if let Some(path) = word_after(&prompt, "edit at ") {
                        let response = cx
                            .send_request(RequestPermissionRequest::new(
                                session.clone(),
                                ToolCallUpdate::new(
                                    "call-e",
                                    ToolCallUpdateFields::new()
                                        .title(format!("Edit {path}"))
                                        .kind(ToolKind::Edit)
                                        .locations(vec![ToolCallLocation::new(path)]),
                                ),
                                vec![PermissionOption::new(
                                    "allow",
                                    "Allow",
                                    PermissionOptionKind::AllowOnce,
                                )],
                            ))
                            .block_task()
                            .await?;
                        let outcome = match response.outcome {
                            RequestPermissionOutcome::Selected(s) => s.option_id.0.to_string(),
                            _ => "cancelled".to_string(),
                        };
                        say(format!("edit permission: {outcome}. "))?;
                    }

                    if prompt.contains("where am i") {
                        say(format!("cwd: {}. ", cwd.display()))?;
                    }

                    if prompt.contains("ask permission") {
                        let response = cx
                            .send_request(RequestPermissionRequest::new(
                                session.clone(),
                                ToolCallUpdate::new(
                                    "call-2",
                                    ToolCallUpdateFields::new()
                                        .title("Run rm -rf build")
                                        .kind(ToolKind::Execute),
                                ),
                                vec![
                                    PermissionOption::new(
                                        "allow",
                                        "Allow",
                                        PermissionOptionKind::AllowOnce,
                                    ),
                                    PermissionOption::new(
                                        "reject",
                                        "Reject",
                                        PermissionOptionKind::RejectOnce,
                                    ),
                                ],
                            ))
                            .block_task()
                            .await?;
                        let outcome = match response.outcome {
                            RequestPermissionOutcome::Selected(s) => s.option_id.0.to_string(),
                            _ => "cancelled".to_string(),
                        };
                        say(format!("permission: {outcome}. "))?;
                    }

                    let last = prompt
                        .lines()
                        .rev()
                        .find(|l| !l.trim().is_empty())
                        .unwrap_or("");
                    say("echo: ".to_string())?;
                    say(format!("{last} "))?;
                    say(format!("(mcp: {})", if mcp { "yes" } else { "no" }))?;
                    if prompt.contains("report usage") {
                        let n = USAGE_REPORTS.fetch_add(1, Ordering::SeqCst) + 1;
                        cx.send_notification(SessionNotification::new(
                            session.clone(),
                            SessionUpdate::UsageUpdate(
                                UsageUpdate::new(1_000 * n, 200_000)
                                    .cost(Cost::new(0.25 * n as f64, "USD")),
                            ),
                        ))?;
                    }
                    if prompt.contains("show prompt") {
                        say(format!(" (prompt: {})", prompt.replace('\n', " | ")))?;
                    }
                    if prompt.contains("which mode") {
                        say(format!(" (mode: {mode})"))?;
                    }
                    if prompt.contains("which model") {
                        say(format!(" (model: {model})"))?;
                    }
                    if prompt.contains("which effort") {
                        say(format!(" (effort: {effort})"))?;
                    }
                    responder.respond(PromptResponse::new(StopReason::EndTurn).usage(response_usage()))
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_to(Stdio::new())
        .await
}
