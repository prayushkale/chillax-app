//! Tool logic, plus a real MCP-over-HTTP round trip on a loopback port.

use std::sync::Arc;

use chillax_core::config::TeamConfig;
use chillax_core::{
    Actor, Command, CompanyHandle, Engine, Hub, MemoryLog, Role, RoleCatalog, Workflow,
};
use chillax_mcp::{ChillaxMcp, McpAccess, ToolContext, ToolError};
use chillax_tracker::LocalTracker;
use serde_json::{Value, json};

const PM: &str = "---\nid: pm\ntitle: Project Manager\nmcp_tools: [create_epic, create_story, create_sprint, finish_request, list_tickets, ask_stakeholder, my_assignments, hire_teammate, list_roles, board_digest]\n---\nPM\n";
const DEV: &str = "---\nid: dev\ntitle: Developer\nowns_states: [in_progress]\nmcp_tools: [my_assignments, comment]\n---\nDev\n";

async fn company() -> Arc<Hub> {
    let roles = RoleCatalog::new([Role::parse(PM).expect("pm"), Role::parse(DEV).expect("dev")]);
    let hub = Arc::new(
        Hub::open(
            Engine::new("Acme", roles, Workflow::default()).with_team(TeamConfig {
                hiring_roles: vec!["pm".into()],
                max_team_size: 3,
                ..TeamConfig::default()
            }),
            Arc::new(MemoryLog::default()),
        )
        .await
        .expect("hub"),
    );
    for (role, name) in [("pm", "Pat"), ("dev", "Dana")] {
        hub.send(
            Actor::Stakeholder,
            Command::HireEmployee {
                role: role.into(),
                name: Some(name.into()),
                backend: None,
                model: None,
                effort: None,
                keep_defaults: false,
            },
        )
        .await
        .expect("hired");
    }
    hub
}

fn ctx(hub: &Arc<Hub>) -> ToolContext {
    let company: Arc<dyn CompanyHandle> = hub.clone();
    ToolContext {
        tracker: Arc::new(LocalTracker::new(company.clone())),
        company,
        attachments: None,
    }
}

#[tokio::test]
async fn pm_plans_and_dev_is_limited() {
    let hub = company().await;
    let ctx = ctx(&hub);
    let pm = Role::parse(PM).expect("pm");
    let dev = Role::parse(DEV).expect("dev");

    let epic = ctx
        .call("pat", &pm, "create_epic", json!({"title": "Auth"}))
        .await
        .expect("epic");
    assert_eq!(epic["id"], "EP-1");
    let story = ctx
        .call(
            "pat",
            &pm,
            "create_story",
            json!({"title": "Login", "acceptance_criteria": ["works"], "epic": "EP-1"}),
        )
        .await
        .expect("story");
    assert_eq!(story["kind"], "story");
    let sprint = ctx
        .call(
            "pat",
            &pm,
            "create_sprint",
            json!({"name": "S1", "tickets": ["T-1"]}),
        )
        .await
        .expect("sprint");
    assert_eq!(sprint["sprint"]["status"], "proposed");
    ctx.call(
        "pat",
        &pm,
        "ask_stakeholder",
        json!({"question": "Web or mobile?"}),
    )
    .await
    .expect("asked");
    assert_eq!(hub.snapshot().await.inbox.len(), 2);

    assert_eq!(
        ctx.call("dana", &dev, "create_epic", json!({"title": "x"}))
            .await,
        Err(ToolError::Forbidden("create_epic".into()))
    );
    assert!(matches!(
        ctx.call("pat", &pm, "create_story", json!({"nope": 1}))
            .await,
        Err(ToolError::BadArguments(_))
    ));
    assert!(matches!(
        ctx.call("pat", &pm, "create_story", json!({"title": "x", "acceptance_criteria": [], "epic": "EP-9"})).await,
        Err(ToolError::Rejected(msg)) if msg.contains("EP-9")
    ));
    assert_eq!(
        ctx.call("dana", &dev, "my_assignments", Value::Null).await,
        Ok(json!([]))
    );
}

#[tokio::test]
async fn pm_hires_a_teammate_within_the_cap() {
    let hub = company().await;
    let ctx = ctx(&hub);
    let pm = Role::parse(PM).expect("pm");
    let dev = Role::parse(DEV).expect("dev");

    let roles = ctx
        .call("pat", &pm, "list_roles", Value::Null)
        .await
        .expect("roles");
    let ids: Vec<&str> = roles["roles"]
        .as_array()
        .expect("roles array")
        .iter()
        .map(|r| r["id"].as_str().expect("id"))
        .collect();
    assert_eq!(ids, ["dev"]);
    assert_eq!(roles["team_size"], 2);
    assert_eq!(roles["max_team_size"], 3);

    let hired = ctx
        .call(
            "pat",
            &pm,
            "hire_teammate",
            json!({"role": "dev", "reason": "T-1 needs a developer", "name": "Dee"}),
        )
        .await
        .expect("hired");
    assert_eq!(hired["ok"], true);
    assert_eq!(hired["employee"], "dee");
    assert_eq!(hired["role"], "dev");
    let state = hub.snapshot().await;
    assert_eq!(state.employees.len(), 3);
    assert_eq!(state.inbox.len(), 1);

    // At the cap: refused with the number.
    assert!(matches!(
        ctx.call("pat", &pm, "hire_teammate", json!({"role": "dev", "reason": "more"})).await,
        Err(ToolError::Rejected(msg)) if msg.contains('3')
    ));
    assert!(matches!(
        ctx.call("pat", &pm, "hire_teammate", json!({"role": "dev"}))
            .await,
        Err(ToolError::BadArguments(_))
    ));
    // The role file decides who sees the tool.
    assert_eq!(
        ctx.call(
            "dana",
            &dev,
            "hire_teammate",
            json!({"role": "dev", "reason": "x"})
        )
        .await,
        Err(ToolError::Forbidden("hire_teammate".into()))
    );
    // An employee whose role is not a hiring role is refused by the engine too.
    let sneaky =
        Role::parse("---\nid: dev\ntitle: Developer\nmcp_tools: [hire_teammate]\n---\nDev\n")
            .expect("role");
    assert!(matches!(
        ctx.call("dana", &sneaky, "hire_teammate", json!({"role": "dev", "reason": "x"})).await,
        Err(ToolError::Rejected(msg)) if msg.contains("may not hire")
    ));
}

#[tokio::test]
async fn a_developer_reports_an_app_and_the_role_file_decides_who_may() {
    let hub = company().await;
    let ctx = ctx(&hub);
    let pm = Role::parse(PM).expect("pm");
    let dev = Role::parse(DEV).expect("dev");
    let reporter = Role::parse(
        "---\nid: dev\ntitle: Developer\nmcp_tools: [my_assignments, report_app]\n---\nDev\n",
    )
    .expect("role");

    let listed: Vec<&str> = chillax_mcp::tools_for(&reporter).map(|t| t.name).collect();
    assert_eq!(listed, ["my_assignments", "report_app"]);

    let reported = ctx
        .call(
            "dana",
            &reporter,
            "report_app",
            json!({"port": 3001, "env": "preview", "note": "login page"}),
        )
        .await
        .expect("reported");
    assert_eq!(
        reported,
        json!({"ok": true, "url": "http://localhost:3001"})
    );
    let state = hub.snapshot().await;
    assert_eq!(state.reported_apps.len(), 1);
    assert_eq!(state.reported_apps[0].employee, "dana");
    assert_eq!(state.reported_apps[0].note.as_deref(), Some("login page"));

    assert!(matches!(
        ctx.call(
            "dana",
            &reporter,
            "report_app",
            json!({"url": "http://evil.com"})
        )
        .await,
        Err(ToolError::Rejected(_))
    ));
    assert_eq!(
        ctx.call("pat", &pm, "report_app", json!({"port": 3000}))
            .await,
        Err(ToolError::Forbidden("report_app".into()))
    );
    assert_eq!(
        ctx.call("dana", &dev, "report_app", json!({"port": 3000}))
            .await,
        Err(ToolError::Forbidden("report_app".into()))
    );
    ctx.call(
        "dana",
        &reporter,
        "report_app",
        json!({"env": "preview", "down": true}),
    )
    .await
    .expect("down");
    assert!(hub.snapshot().await.reported_apps.is_empty());
}

#[tokio::test]
async fn request_intake_files_assigns_links_and_finishes() {
    let hub = company().await;
    let ctx = ctx(&hub);
    let pm = Role::parse(PM).expect("pm");
    let ask = |text: &str| Command::SubmitRequest {
        text: text.into(),
        start_now: true,
    };
    hub.send(Actor::Stakeholder, ask("Add login"))
        .await
        .expect("submitted");

    // An unknown request is refused before any ticket exists.
    assert!(matches!(
        ctx.call("pat", &pm, "create_story", json!({
            "title": "Login", "acceptance_criteria": ["works"], "request_id": "R-9"
        })).await,
        Err(ToolError::Rejected(msg)) if msg.contains("R-9")
    ));
    assert!(hub.snapshot().await.tickets.is_empty());

    let story = ctx
        .call(
            "pat",
            &pm,
            "create_story",
            json!({"title": "Login", "acceptance_criteria": ["works"],
                   "request_id": "R-1", "assignee": "dana"}),
        )
        .await
        .expect("story");
    assert_eq!(story["assignee"], "dana");
    // start_now: the request opened and approved a sprint, so the ticket is ready.
    assert_eq!(story["state"], "ready");
    assert_eq!(story["sprint"], "SP-1");

    ctx.call(
        "pat",
        &pm,
        "finish_request",
        json!({"request_id": "R-1", "summary": "T-1 to Dana"}),
    )
    .await
    .expect("finished");
    let state = hub.snapshot().await;
    assert_eq!(
        state.request("R-1").map(|r| r.tickets.clone()),
        Some(vec!["T-1".to_string()])
    );
    // A finished request takes no more tickets.
    assert!(matches!(
        ctx.call("pat", &pm, "create_story", json!({
            "title": "More", "acceptance_criteria": [], "request_id": "R-1"
        })).await,
        Err(ToolError::Rejected(msg)) if msg.contains("triaged")
    ));
}

/// Minimal MCP streamable-HTTP client: POST JSON-RPC, read JSON or SSE `data:` lines.
struct Client {
    http: reqwest::Client,
    url: String,
    token: String,
    session: Option<String>,
}

impl Client {
    async fn rpc(&mut self, body: Value) -> Option<Value> {
        let mut req = self
            .http
            .post(&self.url)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .header("MCP-Protocol-Version", "2025-06-18")
            .bearer_auth(&self.token)
            .json(&body);
        if let Some(session) = &self.session {
            req = req.header("Mcp-Session-Id", session);
        }
        let resp = req.send().await.expect("request sent");
        assert!(
            resp.status().is_success(),
            "status {} for {body}",
            resp.status()
        );
        if let Some(id) = resp.headers().get("mcp-session-id") {
            self.session = Some(id.to_str().expect("ascii").to_string());
        }
        let text = resp.text().await.expect("body");
        body.get("id")?;
        let json = text
            .lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or(text.trim());
        Some(serde_json::from_str(json).expect("json-rpc response"))
    }
}

#[tokio::test]
async fn tools_are_scoped_by_bearer_token_over_http() {
    let hub = company().await;
    let company: Arc<dyn CompanyHandle> = hub.clone();
    let access = Arc::new(McpAccess::default());
    let mcp = ChillaxMcp::new(
        company.clone(),
        Arc::new(LocalTracker::new(company)),
        None,
        access.clone(),
    );
    let app = axum::Router::new().nest_service("/mcp", mcp.http_service());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, app).await });

    let tool_names = |resp: Value| -> Vec<String> {
        resp["result"]["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .map(|t| t["name"].as_str().expect("name").to_string())
            .collect()
    };

    let mut clients = Vec::new();
    for employee in ["pat", "dana"] {
        let mut c = Client {
            http: reqwest::Client::new(),
            url: format!("http://{addr}/mcp"),
            token: access.issue(employee),
            session: None,
        };
        c.rpc(
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "test", "version": "0"}}}),
        )
        .await;
        c.rpc(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        clients.push(c);
    }

    let pat_tools = tool_names(
        clients[0]
            .rpc(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))
            .await
            .expect("resp"),
    );
    assert!(pat_tools.contains(&"create_story".to_string()));
    assert!(pat_tools.contains(&"hire_teammate".to_string()));
    let dana_tools = tool_names(
        clients[1]
            .rpc(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))
            .await
            .expect("resp"),
    );
    assert_eq!(dana_tools, ["my_assignments", "comment"]);

    let resp = clients[0]
        .rpc(
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
            "name": "create_epic", "arguments": {"title": "Payments"}}}),
        )
        .await
        .expect("resp");
    assert_eq!(resp["result"]["isError"], false, "{resp}");
    assert_eq!(hub.snapshot().await.epics[0].title, "Payments");
    let event = hub
        .events_since(0)
        .await
        .expect("events")
        .pop()
        .expect("event");
    assert_eq!(event.actor, Actor::Employee("pat".into()));

    let denied = clients[1]
        .rpc(
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
            "name": "create_epic", "arguments": {"title": "x"}}}),
        )
        .await
        .expect("resp");
    assert_eq!(denied["result"]["isError"], true, "{denied}");
}

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nshot";

#[tokio::test]
async fn comment_attaches_images_from_allowed_folders_only() {
    let hub = company().await;
    hub.send(
        Actor::Stakeholder,
        Command::CreateTicket {
            kind: chillax_core::TicketKind::Bug,
            title: "Broken".into(),
            description: String::new(),
            acceptance_criteria: Vec::new(),
            estimate: None,
            epic: None,
            external: None,
        },
    )
    .await
    .expect("ticket");
    let dir = tempfile::tempdir().expect("tempdir");
    let work = dir.path().join("worktrees/T-1");
    std::fs::create_dir_all(&work).expect("mkdir");
    std::fs::write(work.join("fail.png"), PNG).expect("png");
    std::fs::write(work.join("notes.png"), b"not an image").expect("fake");
    let secret = dir.path().join("secret.png");
    std::fs::write(&secret, PNG).expect("secret");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&secret, work.join("link.png")).expect("symlink");

    let mut c = ctx(&hub);
    c.attachments = Some(chillax_mcp::AttachmentAccess {
        store: Arc::new(chillax_store::FileAttachmentStore::new(
            dir.path().join("attachments"),
        )),
        roots: vec![dir.path().join("worktrees")],
    });
    let dev = Role::parse(DEV).expect("dev");
    let call = |paths: Vec<String>| {
        c.call(
            "dana",
            &dev,
            "comment",
            json!({"id": "T-1", "body": "evidence", "attachments": paths}),
        )
    };

    call(vec![work.join("fail.png").display().to_string()])
        .await
        .expect("inside the worktree");
    let state = hub.snapshot().await;
    let a = &state.tickets[0].comments[0].attachments[0];
    assert_eq!(
        (a.name.as_str(), a.mime.as_str()),
        ("fail.png", "image/png")
    );

    for bad in [
        secret.display().to_string(),
        work.join("../../secret.png").display().to_string(),
        work.join("notes.png").display().to_string(),
        work.join("missing.png").display().to_string(),
        #[cfg(unix)]
        work.join("link.png").display().to_string(),
    ] {
        let err = call(vec![bad.clone()]).await.expect_err(&bad);
        assert!(matches!(err, ToolError::Rejected(_)), "{bad}: {err}");
    }
    // Nothing from the failed calls reached the ticket.
    assert_eq!(hub.snapshot().await.tickets[0].comments.len(), 1);

    // Without a store, attachments are refused rather than dropped.
    let plain = ctx(&hub);
    let err = plain
        .call(
            "dana",
            &dev,
            "comment",
            json!({"id": "T-1", "body": "x", "attachments": ["/tmp/a.png"]}),
        )
        .await
        .expect_err("no store");
    assert!(matches!(err, ToolError::Rejected(_)));
}

/// Serve MCP on a loopback port with the given session keep-alive and open one session.
async fn session_client(keep_alive: Option<std::time::Duration>) -> Client {
    let hub = company().await;
    let company: Arc<dyn CompanyHandle> = hub;
    let access = Arc::new(McpAccess::default());
    let mcp = ChillaxMcp::new(
        company.clone(),
        Arc::new(LocalTracker::new(company)),
        None,
        access.clone(),
    );
    let app =
        axum::Router::new().nest_service("/mcp", mcp.http_service_with_keep_alive(keep_alive));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, app).await });
    let mut c = Client {
        http: reqwest::Client::new(),
        url: format!("http://{addr}/mcp"),
        token: access.issue("pat"),
        session: None,
    };
    c.rpc(
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {},
        "clientInfo": {"name": "test", "version": "0"}}}),
    )
    .await;
    c.rpc(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .await;
    c
}

/// Status of a `tools/list` on the client's current session.
async fn list_status(c: &Client) -> reqwest::StatusCode {
    c.http
        .post(&c.url)
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .header("MCP-Protocol-Version", "2025-06-18")
        .header("Mcp-Session-Id", c.session.as_deref().expect("session"))
        .bearer_auth(&c.token)
        .json(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))
        .send()
        .await
        .expect("request sent")
        .status()
}

#[tokio::test]
async fn short_keep_alive_reaps_idle_sessions() {
    let c = session_client(Some(std::time::Duration::from_millis(200))).await;
    assert!(list_status(&c).await.is_success());
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    assert_eq!(list_status(&c).await, reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn long_keep_alive_keeps_idle_sessions_usable() {
    let c = session_client(Some(std::time::Duration::from_secs(3600))).await;
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    assert!(list_status(&c).await.is_success());
    // A brand-new session still initialises.
    let fresh = session_client(Some(std::time::Duration::from_secs(3600))).await;
    assert!(fresh.session.is_some());
}

#[test]
fn default_session_idle_timeout_outlasts_a_coding_run() {
    assert!(chillax_mcp::SESSION_IDLE_TIMEOUT >= std::time::Duration::from_secs(3600));
}

#[tokio::test]
async fn board_digest_reads_the_board_and_is_gated_by_the_role() {
    let hub = company().await;
    let ctx = ctx(&hub);
    let pm = Role::parse(PM).expect("pm");
    let dev = Role::parse(DEV).expect("dev");
    let empty = ctx
        .call("pat", &pm, "board_digest", Value::Null)
        .await
        .expect("digest");
    assert_eq!(empty["digest"], "No open work.");
    ctx.call(
        "pat",
        &pm,
        "create_story",
        json!({"title": "Login", "acceptance_criteria": ["works"]}),
    )
    .await
    .expect("story");
    let digest = ctx
        .call("pat", &pm, "board_digest", Value::Null)
        .await
        .expect("digest");
    let text = digest["digest"].as_str().expect("text");
    assert!(text.contains("Open: 1"), "{text}");
    assert!(!text.contains("Parallel limit"), "{text}");
    assert_eq!(
        ctx.call("dana", &dev, "board_digest", Value::Null).await,
        Err(ToolError::Forbidden("board_digest".into()))
    );
}

#[tokio::test]
async fn a_developer_suggests_work_and_the_role_file_decides_who_may() {
    let hub = company().await;
    let ctx = ctx(&hub);
    let pm = Role::parse(PM).expect("pm");
    let dev = Role::parse(DEV).expect("dev");
    let suggester =
        Role::parse("---\nid: dev\ntitle: Developer\nmcp_tools: [suggest_work]\n---\nDev\n")
            .expect("suggester");

    let args = json!({"kind": "bug", "summary": "Login 500s on an empty email"});
    // The role file decides who sees the tool.
    assert_eq!(
        ctx.call("dana", &dev, "suggest_work", args.clone()).await,
        Err(ToolError::Forbidden("suggest_work".into()))
    );
    assert_eq!(
        ctx.call("pat", &pm, "suggest_work", args.clone()).await,
        Err(ToolError::Forbidden("suggest_work".into()))
    );

    let sent = ctx
        .call("dana", &suggester, "suggest_work", args)
        .await
        .expect("suggested");
    assert_eq!(sent["request"], "R-1");
    let state = hub.snapshot().await;
    let request = &state.requests[0];
    assert_eq!(request.from.as_deref(), Some("dana"));
    assert!(!request.start_now);

    assert!(matches!(
        ctx.call(
            "dana",
            &suggester,
            "suggest_work",
            json!({"kind": "idea", "summary": "x"})
        )
        .await,
        Err(ToolError::BadArguments(_))
    ));
}
