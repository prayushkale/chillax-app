//! One employee's ACP session: spawn the agent, initialise, open a session with the
//! Chillax MCP endpoint attached, then turn queued messages into prompts.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    CancelNotification, HttpHeader, InitializeRequest, McpServer, McpServerHttp, NewSessionRequest,
    PromptRequest, RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionNotification, SessionUpdate, StopReason,
};
use agent_client_protocol::{AcpAgent, AcpAgentConfig, Agent, Client, ConnectionTo};
use chillax_core::config::{AgentConfig, PermissionMode};
use chillax_core::dispatch::SessionKind;
use chillax_core::{Actor, Employee, EventKind, Hub, PermissionChoice, PriceTable, Role, TicketId};
use tokio::sync::mpsc;

use crate::acp::{describe_error, enter_bypass_mode, select_effort, select_model};
use crate::convert::{
    auto_choice, option_kind, tool_kind, update_to_events, usage_from_update, within, yolo_choice,
};
use crate::procgroup;
use crate::usage::SessionUsage;
use crate::{Activity, McpEndpoint, PendingPermissions};

/// A session tied to one ticket's worktree (the dev loop), as opposed to a chat.
#[derive(Debug, Clone)]
pub struct TicketSpec {
    pub id: TicketId,
    pub kind: SessionKind,
    /// Where a QA session may write scratch files.
    pub scratch: Option<PathBuf>,
}

pub struct SessionSpec {
    pub employee: Employee,
    pub role: Role,
    pub company: String,
    pub cwd: PathBuf,
    pub backend: AgentConfig,
    pub mcp: Option<McpEndpoint>,
    pub ticket: Option<TicketSpec>,
    /// How permission requests are answered (`[agents] permissions`).
    pub permissions: PermissionMode,
    /// A throwaway session that checks the backend answers: it records no events.
    pub probe: bool,
    /// Prices for estimating cost when the backend reports tokens only (ADR 0023).
    pub pricing: Arc<PriceTable>,
    /// The project's env files for the session's stage.
    pub env: BTreeMap<String, String>,
}

pub fn agent_process(backend: &AgentConfig) -> AcpAgent {
    project_agent_process(backend, &BTreeMap::new())
}

/// An agent process that also gets the project's env file values.
fn project_agent_process(backend: &AgentConfig, project: &BTreeMap<String, String>) -> AcpAgent {
    project_agent_process_at(backend, project, None)
}

/// With a `pid_file`, the agent is started through `sh` which writes its pid there and
/// then `exec`s the real command. The ACP crate makes the process a group leader but does
/// not expose its pid; the same pid is the group id, which Chillax needs to stop the
/// agent's descendants gracefully and on exit (ADR 0015).
fn project_agent_process_at(
    backend: &AgentConfig,
    project: &BTreeMap<String, String>,
    pid_file: Option<&Path>,
) -> AcpAgent {
    let inherited: Vec<String> = std::env::vars().map(|(k, _)| k).collect();
    let config = match pid_file {
        #[cfg(unix)]
        Some(file) => AcpAgentConfig::new("sh").args(
            [
                "-c".to_string(),
                "echo $$ > \"$0\"; exec \"$@\"".to_string(),
            ]
            .into_iter()
            .chain([file.display().to_string(), backend.command.clone()])
            .chain(backend.args.iter().cloned()),
        ),
        _ => AcpAgentConfig::new(&backend.command).args(backend.args.iter().cloned()),
    };
    AcpAgent::new(config.envs(agent_env(backend, project, inherited)))
}

/// Registers the agent's process group while the session lives and forgets it after.
struct GroupGuard {
    pgid: Option<u32>,
    pid_file: PathBuf,
}

impl GroupGuard {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        Self {
            pgid: None,
            pid_file: std::env::temp_dir()
                .join(format!("chillax-agent-{}-{n}.pid", std::process::id())),
        }
    }

    /// Read the pid the wrapper wrote (it does so before the agent can answer) and
    /// register its group.
    async fn register(&mut self, activity: &Activity) {
        for _ in 0..20 {
            if let Ok(text) = tokio::fs::read_to_string(&self.pid_file).await
                && let Ok(pid) = text.trim().parse::<u32>()
            {
                procgroup::register(pid);
                activity.set_pgid(pid);
                self.pgid = Some(pid);
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        tracing::warn!("could not learn the agent's process group");
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(pgid) = self.pgid {
            procgroup::forget(pgid);
        }
        let _ = std::fs::remove_file(&self.pid_file);
    }
}

/// The env vars set on an agent process. The ACP crate can only add variables, not
/// remove them, so every inherited `HERDR_*` one is blanked: agents are not herdr
/// panes, and herdr's own agent integrations would otherwise report them as "working"
/// and never clear it when the process is killed. The project's env files come next,
/// and `[agents.*] env` wins over both.
fn agent_env(
    backend: &AgentConfig,
    project: &BTreeMap<String, String>,
    inherited: impl IntoIterator<Item = String>,
) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = inherited
        .into_iter()
        .filter(|k| k.starts_with("HERDR_"))
        .map(|k| (k, String::new()))
        .collect();
    env.extend(project.clone());
    env.extend(backend.env.clone());
    env
}

pub async fn run(
    spec: SessionSpec,
    hub: Arc<Hub>,
    pending: PendingPermissions,
    mut prompts: mpsc::UnboundedReceiver<String>,
    activity: Arc<Activity>,
    usage: Arc<SessionUsage>,
) -> Result<(), String> {
    let mut group = GroupGuard::new();
    let pid_file = group.pid_file.clone();
    let employee_id = spec.employee.id.clone();
    let actor = Actor::Employee(employee_id.clone());

    let notify_hub = Arc::clone(&hub);
    let notify_employee = employee_id.clone();
    let permission_hub = Arc::clone(&hub);
    let permission_employee = employee_id.clone();
    let role = spec.role.clone();
    let backend_name = spec.employee.backend.clone();
    // Ticket sessions stay in their worktree; reviewers may only look and testers may
    // only look and run things.
    let sandbox = spec.ticket.as_ref().map(|_| spec.cwd.clone());
    let session_kind = spec.ticket.as_ref().map(|t| t.kind);
    let scratch = spec.ticket.as_ref().and_then(|t| t.scratch.clone());
    let permission_mode = spec.permissions;
    let quiet = spec.probe;
    let ticket_id = spec.ticket.as_ref().map(|t| t.id.clone());
    let notify_usage = Arc::clone(&usage);

    Client
        .builder()
        .name("chillax")
        .on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                if quiet {
                    return Ok(());
                }
                // Only the tally changes per update; the prompt loop records one sample
                // per turn.
                if let SessionUpdate::UsageUpdate(update) = &notification.update {
                    notify_usage.on_update(usage_from_update(update));
                }
                for kind in update_to_events(&notify_employee, notification.update) {
                    let actor = Actor::Employee(notify_employee.clone());
                    if let Err(err) = notify_hub.record(actor, kind).await {
                        tracing::error!(%err, "could not record agent update");
                    }
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _cx| {
                let fields = &request.tool_call.fields;
                let kind = fields.kind.map(tool_kind).unwrap_or("other");
                let title = fields
                    .title
                    .clone()
                    .unwrap_or_else(|| format!("{kind} ({})", request.tool_call.tool_call_id.0));
                let choices: Vec<PermissionChoice> = request
                    .options
                    .iter()
                    .map(|o| PermissionChoice {
                        id: o.option_id.0.to_string(),
                        name: o.name.clone(),
                        kind: option_kind(o.kind).to_string(),
                    })
                    .collect();
                let actor = Actor::Employee(permission_employee.clone());
                let own_tool = is_chillax_tool(&title);

                // `permissions = "auto"`: say yes to everything, so unattended work never
                // stalls on a prompt (and nobody has to babysit the inbox).
                if permission_mode == PermissionMode::Auto
                    && let Some(choice) = yolo_choice(&choices)
                {
                    if !quiet {
                        let _ = permission_hub
                            .record(
                                actor,
                                EventKind::PermissionResolved {
                                    id: format!("auto:{}", request.tool_call.tool_call_id.0),
                                    choice: Some(choice.id.clone()),
                                    automatic: true,
                                },
                            )
                            .await;
                    }
                    return responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                            choice.id.clone(),
                        )),
                    ));
                }

                let locations = fields.locations.as_deref().unwrap_or_default();
                // A tester's own scratch directory is theirs to write; nothing else
                // outside the tree is.
                let in_scratch = session_kind == Some(SessionKind::Qa)
                    && !locations.is_empty()
                    && scratch.as_ref().is_some_and(|dir| within(dir, locations));
                let contained =
                    in_scratch || sandbox.as_ref().is_none_or(|dir| within(dir, locations));
                let allowed =
                    in_scratch || session_kind.is_none_or(|session| session_allows(session, kind));
                if (own_tool || (role.auto_approves(kind) && contained && allowed))
                    && let Some(choice) = auto_choice(&choices)
                {
                    let _ = permission_hub
                        .record(
                            actor,
                            EventKind::PermissionResolved {
                                id: format!("auto:{}", request.tool_call.tool_call_id.0),
                                choice: Some(choice.id.clone()),
                                automatic: true,
                            },
                        )
                        .await;
                    return responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                            choice.id.clone(),
                        )),
                    ));
                }

                // Escalate. The responder is parked until the stakeholder decides, so
                // the dispatch loop is not held while we wait.
                let employee = permission_employee.clone();
                let stopper = permission_employee.clone();
                let pending = pending.clone();
                let recorded = permission_hub
                    .record_with(actor, move |state| {
                        let id = state.next_inbox_id();
                        pending
                            .lock()
                            .expect("pending lock poisoned")
                            .insert(id.clone(), (stopper, responder));
                        vec![EventKind::PermissionRequested {
                            id,
                            employee,
                            title,
                            choices,
                        }]
                    })
                    .await;
                if let Err(err) = recorded {
                    tracing::error!(%err, "could not record permission request");
                }
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(
            project_agent_process_at(&spec.backend, &spec.env, Some(&pid_file)),
            async move |cx: ConnectionTo<Agent>| {
                group.register(&activity).await;
                let init = cx
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let mut request = NewSessionRequest::new(spec.cwd.clone());
                let mcp_attached = match &spec.mcp {
                    Some(mcp) if init.agent_capabilities.mcp_capabilities.http => {
                        let token = (mcp.issue_token)(&employee_id);
                        request.mcp_servers = vec![McpServer::Http(
                            McpServerHttp::new("chillax", mcp.url.clone()).headers(vec![
                                HttpHeader::new("Authorization", format!("Bearer {token}")),
                            ]),
                        )];
                        true
                    }
                    _ => false,
                };
                let session = cx.send_request(request).block_task().await?;
                let mode = if spec.permissions == PermissionMode::Auto {
                    enter_bypass_mode(&cx, &session.session_id, session.modes.as_ref()).await
                } else {
                    None
                };
                let selection = select_model(
                    &cx,
                    &session.session_id,
                    session.config_options.as_deref(),
                    spec.employee.model.as_deref(),
                )
                .await?;
                // Agents may rebuild their effort choices after a model switch.
                let effort = select_effort(
                    &cx,
                    &session.session_id,
                    selection
                        .options
                        .as_deref()
                        .or(session.config_options.as_deref()),
                    spec.employee.effort.as_deref(),
                )
                .await?;
                usage.set_model(
                    selection
                        .model
                        .clone()
                        .or_else(|| spec.employee.model.clone()),
                );
                let agent = init.agent_info.as_ref().map(|info| {
                    let model = selection
                        .model
                        .map(|m| format!(", model {m}"))
                        .unwrap_or_default();
                    let effort = effort.map(|e| format!(", effort {e}")).unwrap_or_default();
                    let mode = mode.map(|m| format!(", mode {m}")).unwrap_or_default();
                    format!("{} {}{model}{effort}{mode}", info.name, info.version)
                });
                if !quiet {
                    let _ = hub
                        .record(
                            actor.clone(),
                            EventKind::AgentStarted {
                                employee: employee_id.clone(),
                                agent,
                            },
                        )
                        .await;
                }

                let mut first = true;
                let mut seen_stop = activity.stop_gen();
                while let Some(mut text) = prompts.recv().await {
                    activity.dequeued();
                    // Fold in anything else that queued up during the last turn.
                    while let Ok(more) = prompts.try_recv() {
                        activity.dequeued();
                        text.push_str("\n\n");
                        text.push_str(&more);
                    }
                    // The stakeholder stopped before this prompt ran: drop it.
                    if activity.stop_gen() != seen_stop {
                        seen_stop = activity.stop_gen();
                        continue;
                    }
                    if std::mem::take(&mut first) && !quiet {
                        text = preamble(&spec, mcp_attached, &text);
                    }
                    activity.begin_turn();
                    let request = cx
                        .send_request(PromptRequest::new(
                            session.session_id.clone(),
                            vec![text.into()],
                        ))
                        .block_task();
                    tokio::pin!(request);
                    // A close asks the agent to cancel and keeps waiting: the answer
                    // carries the turn's token usage, which nothing else reports.
                    let mut cancelled = false;
                    let response = tokio::select! {
                        response = &mut request => response?,
                        // The stakeholder stopped the turn: ask the agent to cancel and
                        // drop what queued behind it, but keep the session going.
                        () = activity.stopped_since(seen_stop) => {
                            while prompts.try_recv().is_ok() {
                                activity.dequeued();
                            }
                            cx.send_notification(CancelNotification::new(
                                session.session_id.clone(),
                            ))?;
                            request.await?
                        }
                        () = activity.cancelled() => {
                            cancelled = true;
                            cx.send_notification(CancelNotification::new(
                                session.session_id.clone(),
                            ))?;
                            request.await?
                        }
                    };
                    activity.end_turn();
                    seen_stop = activity.stop_gen();
                    if !quiet {
                        if let Some(sample) = usage.take_turn_sample(
                            response.usage.as_ref(),
                            spec.backend.tokens_cumulative,
                            &spec.pricing,
                        ) {
                            let _ = hub.record(actor.clone(), sample).await;
                        }
                        let _ = hub
                            .record(
                                actor.clone(),
                                EventKind::AgentTurnEnded {
                                    employee: employee_id.clone(),
                                    stop_reason: stop_reason(response.stop_reason).to_string(),
                                    ticket: ticket_id.clone(),
                                },
                            )
                            .await;
                    }
                    if cancelled {
                        break;
                    }
                }
                Ok(())
            },
        )
        .await
        .map_err(|err| describe_error(&backend_name, &err))
}

fn stop_reason(reason: StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => "end_turn",
        StopReason::MaxTokens => "max_tokens",
        StopReason::MaxTurnRequests => "max_turn_requests",
        StopReason::Refusal => "refusal",
        StopReason::Cancelled => "cancelled",
        _ => "other",
    }
}

/// ACP v1 has no stable system-prompt field, so the role prompt opens the first turn.
fn preamble(spec: &SessionSpec, mcp_attached: bool, message: &str) -> String {
    if spec.ticket.is_some() {
        return ticket_preamble(spec, mcp_attached, message);
    }
    let role = spec.role.render_prompt(&spec.company, &spec.employee.name);
    let tools = if mcp_attached {
        format!(
            "Use the `chillax` MCP tools for all company and tracker work (epics, stories, \
             sprints, tickets, questions for the stakeholder). Your tools: {}. Your \
             employee id is `{}`.",
            spec.role.mcp_tools.join(", "),
            spec.employee.id
        )
    } else {
        "The Chillax MCP tools could not be attached to this session (the agent does not \
         support HTTP MCP servers). Tell the stakeholder if you need them."
            .to_string()
    };
    format!(
        "<chillax-role>\n{role}\n\n{tools}\n\nThe stakeholder is the human who runs this \
         company; they read your replies in the Chillax chat.\n</chillax-role>\n\n\
         Message from the stakeholder:\n\n{message}"
    )
}

/// Dev-loop sessions are started by Chillax, not by a chat message, and live in one
/// worktree.
/// Whether a ticket session of `session` kind may auto-approve a tool call of ACP kind
/// `tool`, before the role's own policy is consulted. Developers are limited by their
/// worktree alone; reviewers only look; testers look and run commands but never edit
/// the shared `qa` tree.
pub(crate) fn session_allows(session: SessionKind, tool: &str) -> bool {
    match session {
        SessionKind::Work => true,
        SessionKind::Review => matches!(tool, "read" | "search" | "think"),
        SessionKind::Qa => matches!(tool, "read" | "search" | "think" | "execute"),
    }
}

/// Whether a permission request is for one of Chillax's own MCP tools. Agents name MCP
/// tools `mcp__<server>__<tool>` and we attach the server as `chillax`. Those calls are
/// already gated where it counts: the endpoint knows the caller by its token and only
/// serves the tools its role lists. Asking the stakeholder about each one would stall
/// every unattended session.
pub(crate) fn is_chillax_tool(title: &str) -> bool {
    title.starts_with("mcp__chillax__")
}

fn ticket_preamble(spec: &SessionSpec, mcp_attached: bool, message: &str) -> String {
    let ticket = spec
        .ticket
        .as_ref()
        .map(|t| t.id.as_str())
        .unwrap_or_default();
    let role = spec.role.render_prompt(&spec.company, &spec.employee.name);
    let tools = if mcp_attached {
        format!(
            "Use the `chillax` MCP tools for ticket work. Your tools: {}. Your employee id \
             is `{}`.",
            spec.role.mcp_tools.join(", "),
            spec.employee.id
        )
    } else {
        "The Chillax MCP tools could not be attached to this session, so you cannot report \
         your work. Say so in your reply."
            .to_string()
    };
    format!(
        "<chillax-role>\n{role}\n\n{tools}\n\nChillax, the system that runs this company, \
         started this session for ticket {ticket}. Your working directory is {}. Stay \
         inside it: everything you read or change must be under that directory.\n</chillax-role>\n\n{message}",
        spec.cwd.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_env_blanks_herdr_vars_but_keeps_configured_env() {
        let mut backend = AgentConfig::default();
        backend.env.insert("KEEP".into(), "1".into());
        backend.env.insert("HERDR_CUSTOM".into(), "mine".into());
        let inherited = ["HERDR_ENV", "HERDR_PANE_ID", "HERDR_CUSTOM", "PATH"].map(String::from);
        let env = agent_env(&backend, &BTreeMap::new(), inherited);
        assert_eq!(env.get("HERDR_ENV").map(String::as_str), Some(""));
        assert_eq!(env.get("HERDR_PANE_ID").map(String::as_str), Some(""));
        assert_eq!(env.get("HERDR_CUSTOM").map(String::as_str), Some("mine"));
        assert_eq!(env.get("KEEP").map(String::as_str), Some("1"));
        assert!(!env.contains_key("PATH"));
    }

    #[test]
    fn agent_env_adds_project_env_files_under_configured_env() {
        let mut backend = AgentConfig::default();
        backend.env.insert("SHARED".into(), "backend".into());
        let project = BTreeMap::from([
            ("SHARED".to_string(), "file".to_string()),
            ("API_URL".to_string(), "http://qa".to_string()),
        ]);
        let env = agent_env(&backend, &project, []);
        assert_eq!(env.get("API_URL").map(String::as_str), Some("http://qa"));
        assert_eq!(env.get("SHARED").map(String::as_str), Some("backend"));
    }

    #[test]
    fn sessions_auto_approve_only_what_their_kind_may_do() {
        for tool in ["read", "edit", "execute", "delete", "fetch"] {
            assert!(session_allows(SessionKind::Work, tool), "{tool}");
        }
        assert!(session_allows(SessionKind::Review, "read"));
        assert!(!session_allows(SessionKind::Review, "execute"));
        assert!(!session_allows(SessionKind::Review, "edit"));
        for tool in ["read", "search", "think", "execute"] {
            assert!(session_allows(SessionKind::Qa, tool), "{tool}");
        }
        for tool in ["edit", "delete", "move", "fetch"] {
            assert!(!session_allows(SessionKind::Qa, tool), "{tool}");
        }
    }

    #[test]
    fn only_chillax_mcp_tools_skip_the_stakeholder() {
        assert!(is_chillax_tool("mcp__chillax__list_team"));
        assert!(is_chillax_tool("mcp__chillax__submit_qa"));
        // Another server, a lookalike, and ordinary tools still go through the policy.
        assert!(!is_chillax_tool("mcp__github__create_issue"));
        assert!(!is_chillax_tool("mcp__chillaxx__list_team"));
        assert!(!is_chillax_tool("Bash: rm -rf /"));
        assert!(!is_chillax_tool("chillax list_team"));
    }

    fn spec(ticket: Option<TicketSpec>) -> SessionSpec {
        SessionSpec {
            employee: Employee {
                id: "dev".into(),
                name: "Dev".into(),
                role: "dev".into(),
                role_title: "Developer".into(),
                backend: "fake".into(),
                model: None,
                effort: None,
            },
            role: Role::parse(
                "---\nid: dev\ntitle: Developer\nmcp_tools: [request_review]\n---\nYou build.\n",
            )
            .expect("valid role"),
            company: "Acme".into(),
            cwd: PathBuf::from("/repo/.chillax/worktrees/T-1"),
            backend: AgentConfig::default(),
            mcp: None,
            ticket,
            permissions: PermissionMode::Policy,
            probe: false,
            pricing: Arc::default(),
            env: BTreeMap::new(),
        }
    }

    #[test]
    fn ticket_sessions_are_briefed_by_chillax_not_the_stakeholder() {
        let ticket = TicketSpec {
            id: "T-1".into(),
            kind: SessionKind::Work,
            scratch: None,
        };
        let text = preamble(&spec(Some(ticket)), true, "Build the login page.");
        assert!(text.contains("ticket T-1"), "{text}");
        assert!(text.contains("/repo/.chillax/worktrees/T-1"), "{text}");
        assert!(text.contains("request_review"), "{text}");
        assert!(text.ends_with("Build the login page."), "{text}");
        assert!(!text.contains("Message from the stakeholder"), "{text}");
    }

    #[test]
    fn chat_sessions_keep_the_stakeholder_framing() {
        let text = preamble(&spec(None), true, "hi");
        assert!(
            text.contains("Message from the stakeholder:\n\nhi"),
            "{text}"
        );
    }
}
