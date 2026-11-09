//! The local web server: the versioned `/api/v1` REST + SSE API
//! that every theme talks to, the Chillax MCP endpoint, and the theme's static files.

// The hand-written OpenAPI document is one large `json!` literal.
#![recursion_limit = "512"]

mod agents;
mod attachments;
mod openapi;
mod recruit;
mod theme;

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Path, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use chillax_core::{
    Actor, AgentDirectory, AttachmentStore, Command, CompanyHandle, Event, InstanceInfo,
    InstanceSource, Recruiter,
};
use futures::Stream;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast;

pub use theme::Theme;

#[derive(Clone)]
struct AppState {
    company: Arc<dyn CompanyHandle>,
    /// The persona catalog; `None` when it is disabled.
    recruiter: Option<Arc<dyn Recruiter>>,
    /// Agent backends and their models; `None` serves 404s on `/agents`.
    agents: Option<Arc<dyn AgentDirectory>>,
    /// Image attachments; `None` serves 404s on `/attachments`.
    attachments: Option<Arc<dyn AttachmentStore>>,
    /// Who this server is; `None` leaves `project` out of `/health`.
    info: Option<ServerInfo>,
    /// The registry of running instances behind `/instances`.
    instances: Option<Arc<dyn InstanceSource>>,
}

/// The project a server belongs to, reported by `/health` and `/instances`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    pub name: String,
    /// Canonical project root.
    pub root: String,
}

/// Optional extras for [`router_with`].
#[derive(Clone, Default)]
pub struct ServerOptions {
    info: Option<ServerInfo>,
    instances: Option<Arc<dyn InstanceSource>>,
}

impl ServerOptions {
    pub fn with_info(mut self, info: ServerInfo) -> Self {
        self.info = Some(info);
        self
    }

    pub fn with_instances(mut self, instances: Arc<dyn InstanceSource>) -> Self {
        self.instances = Some(instances);
        self
    }
}

/// Build the router. `mcp` is mounted at `/mcp` when given.
pub fn router(
    company: Arc<dyn CompanyHandle>,
    recruiter: Option<Arc<dyn Recruiter>>,
    agents: Option<Arc<dyn AgentDirectory>>,
    attachments: Option<Arc<dyn AttachmentStore>>,
    mcp: Option<Router>,
    theme: Theme,
) -> Router {
    router_with(
        company,
        recruiter,
        agents,
        attachments,
        mcp,
        theme,
        ServerOptions::default(),
    )
}

/// [`router`] plus the project identity and instance registry (ADR 0017).
pub fn router_with(
    company: Arc<dyn CompanyHandle>,
    recruiter: Option<Arc<dyn Recruiter>>,
    agents: Option<Arc<dyn AgentDirectory>>,
    attachments: Option<Arc<dyn AttachmentStore>>,
    mcp: Option<Router>,
    theme: Theme,
    options: ServerOptions,
) -> Router {
    let api = Router::new()
        .route("/health", get(health))
        .route("/state", get(state))
        .route("/usage", get(usage))
        .route("/usage/refresh", post(refresh_usage))
        .route("/instances", get(instances))
        .route("/roles", get(roles))
        .route("/events", get(events))
        .route("/commands", post(command))
        .route("/company-chat", post(post_company_chat))
        .route("/employees/{id}/chat/clear", post(clear_chat))
        .route("/employees/{id}/cancel", post(cancel_turn))
        .route("/tickets/{id}/prioritize", post(prioritize))
        .route(
            "/budget/max-concurrent-agents",
            put(set_max_concurrent_agents).post(set_max_concurrent_agents),
        )
        .route("/openapi.json", get(openapi::spec))
        .merge(recruit::routes())
        .merge(agents::routes())
        .merge(attachments::routes());
    let mut app = Router::new().nest("/api/v1", api).with_state(AppState {
        company,
        recruiter,
        agents,
        attachments,
        info: options.info,
        instances: options.instances,
    });
    if let Some(mcp) = mcp {
        app = app.merge(mcp);
    }
    app.fallback(theme::serve)
        .layer(axum::Extension(Arc::new(theme)))
        .layer(middleware::from_fn(local_host_only))
}

/// Reject requests whose `Host` is not a loopback name. With the default
/// `127.0.0.1` bind this blocks DNS-rebinding attacks from web pages.
async fn local_host_only(request: Request, next: Next) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let name = host.rsplit_once(':').map_or(host, |(name, port)| {
        if port.chars().all(|c| c.is_ascii_digit()) {
            name
        } else {
            host
        }
    });
    if matches!(name, "" | "localhost" | "127.0.0.1" | "[::1]") {
        next.run(request).await
    } else {
        (StatusCode::FORBIDDEN, "chillax only serves localhost").into_response()
    }
}

async fn health(State(app): State<AppState>) -> Json<serde_json::Value> {
    let mut body = json!({"status": "ok", "version": env!("CARGO_PKG_VERSION"), "api": "v1"});
    if let Some(info) = &app.info {
        body["project"] = json!({"name": info.name, "root": info.root});
    }
    Json(body)
}

/// The running instances and which one answered (`current` is its project root). Without
/// a registry the list is empty: the server does not know its own public URL.
async fn instances(State(app): State<AppState>) -> Json<serde_json::Value> {
    let list: Vec<InstanceInfo> = match &app.instances {
        Some(source) => source.list().await,
        None => Vec::new(),
    };
    let current = app.info.as_ref().map(|i| i.root.clone());
    Json(json!({"current": current, "instances": list}))
}

async fn state(State(app): State<AppState>) -> impl IntoResponse {
    Json(app.company.snapshot().await)
}

#[derive(Deserialize)]
struct UsageQuery {
    days: Option<i64>,
    top: Option<i64>,
}

/// Token and cost usage, derived (windows, daily and hourly series, tickets, health) so themes
/// do not repeat the arithmetic. Out-of-range parameters are clamped, never rejected.
async fn usage(State(app): State<AppState>, Query(q): Query<UsageQuery>) -> impl IntoResponse {
    Json(usage_view(&app, &q).await)
}

async fn usage_view(app: &AppState, q: &UsageQuery) -> chillax_core::usage::UsageView {
    let days = q.days.unwrap_or(14).clamp(1, 90) as usize;
    // No `top`: every ticket (the Usage views list them all).
    let top = q.top.map(|t| t.clamp(1, 1000) as usize);
    let snapshot = app.company.snapshot().await;
    chillax_core::usage::view(&snapshot, chrono::Utc::now(), days, top)
}

/// How long `/usage/refresh` waits for the runtime to flush live sessions.
const REFRESH_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Ask the agent runtime to record what live sessions still owe, wait for it to say so
/// (or give up after [`REFRESH_WAIT`]; nothing running means nobody answers), and serve
/// the fresh view.
async fn refresh_usage(State(app): State<AppState>, Query(q): Query<UsageQuery>) -> Response {
    let mut rx = app.company.subscribe();
    if let Err(err) = app
        .company
        .send(Actor::Stakeholder, Command::RefreshUsage)
        .await
    {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": err.to_string()})),
        )
            .into_response();
    }
    let _ = tokio::time::timeout(REFRESH_WAIT, async {
        while let Ok(event) = rx.recv().await {
            if matches!(event.kind, chillax_core::EventKind::UsageRefreshed) {
                break;
            }
        }
    })
    .await;
    Json(usage_view(&app, &q).await).into_response()
}

async fn roles(State(app): State<AppState>) -> impl IntoResponse {
    let roles: Vec<_> = app.company.roles().iter().map(recruit::role_json).collect();
    Json(roles)
}

/// Body of `POST /company-chat`.
#[derive(Deserialize)]
struct CompanyChatBody {
    text: String,
    #[serde(default)]
    to: Option<String>,
}

/// Shortcut for the `post_company_chat` command; answers `{seq}` of the committed event.
/// Additive to v1.
async fn post_company_chat(
    State(app): State<AppState>,
    Json(body): Json<CompanyChatBody>,
) -> Response {
    let cmd = Command::PostCompanyChat {
        text: body.text,
        to: body.to,
    };
    match app.company.send(Actor::Stakeholder, cmd).await {
        Ok(events) => Json(json!({"seq": events.first().map(|e| e.seq)})).into_response(),
        Err(err) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": err.to_string()})),
        )
            .into_response(),
    }
}

/// Shortcut for the `clear_chat` command: empty the chat with one employee. Additive to v1.
async fn clear_chat(State(app): State<AppState>, Path(id): Path<String>) -> Response {
    command(State(app), Json(Command::ClearChat { employee: id })).await
}

/// Shortcut for the `cancel_turn` command: stop an employee's current turn, keeping their
/// session. Additive to v1.
async fn cancel_turn(State(app): State<AppState>, Path(id): Path<String>) -> Response {
    command(State(app), Json(Command::CancelTurn { employee: id })).await
}

/// Body of `POST /tickets/{id}/prioritize`; the whole body is optional.
#[derive(Deserialize)]
struct PrioritizeBody {
    #[serde(default = "yes")]
    urgent: bool,
}

fn yes() -> bool {
    true
}

/// Shortcut for the `prioritize_ticket` command: mark a ticket urgent (the default) or,
/// with `{"urgent": false}`, clear the mark. Additive to v1.
async fn prioritize(
    State(app): State<AppState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let urgent = if body.iter().all(u8::is_ascii_whitespace) {
        true
    } else {
        match serde_json::from_slice::<PrioritizeBody>(&body) {
            Ok(b) => b.urgent,
            Err(err) => {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(json!({"error": format!("body: {err}")})),
                )
                    .into_response();
            }
        }
    };
    command(State(app), Json(Command::PrioritizeTicket { id, urgent })).await
}

/// Body of `PUT /budget/max-concurrent-agents`.
#[derive(Deserialize)]
struct AgentLimitBody {
    n: usize,
}

/// Shortcut for the `set_max_concurrent_agents` command. Additive to v1.
async fn set_max_concurrent_agents(
    State(app): State<AppState>,
    body: Result<Json<AgentLimitBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    match body {
        Ok(Json(AgentLimitBody { n })) => {
            command(State(app), Json(Command::SetMaxConcurrentAgents { n })).await
        }
        Err(err) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": format!("body: {err}")})),
        )
            .into_response(),
    }
}

/// Commands posted here come from the stakeholder (the only human with UI access).
async fn command(State(app): State<AppState>, Json(mut cmd): Json<Command>) -> Response {
    if let Some(store) = &app.attachments
        && let Err(err) = attachments::resolve(store.as_ref(), &mut cmd)
    {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": format!("attachment: {err}")})),
        )
            .into_response();
    }
    match app.company.send(Actor::Stakeholder, cmd).await {
        Ok(events) => Json(json!({"events": events})).into_response(),
        Err(err) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": err.to_string()})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct EventsQuery {
    /// Replay persisted events after this sequence number before going live.
    #[serde(default)]
    since: u64,
}

/// Server-sent events: replay from `since`, then stream live. Each SSE message has
/// `event: chillax`, `id: <seq>` and the event JSON as data.
async fn events(
    State(app): State<AppState>,
    Query(query): Query<EventsQuery>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    // Subscribe before reading the backlog so nothing falls in between; duplicates
    // are dropped by sequence number.
    let rx = app.company.subscribe();
    let backlog = app
        .company
        .events_since(query.since)
        .await
        .unwrap_or_default();
    let initial = (VecDeque::from(backlog), rx, query.since, app.company);
    let stream = futures::stream::unfold(
        initial,
        |(mut queue, mut rx, mut last, company)| async move {
            loop {
                if let Some(event) = queue.pop_front() {
                    if event.seq <= last {
                        continue;
                    }
                    last = event.seq;
                    return Some((Ok(to_sse(&event)), (queue, rx, last, company)));
                }
                match rx.recv().await {
                    Ok(event) => queue.push_back(event),
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        queue.extend(company.events_since(last).await.unwrap_or_default());
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        },
    );
    Sse::new(stream).keep_alive(KeepAlive::default())
}

fn to_sse(event: &Event) -> SseEvent {
    SseEvent::default()
        .event("chillax")
        .id(event.seq.to_string())
        .json_data(event)
        .unwrap_or_else(|_| SseEvent::default().comment("unserialisable event"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use chillax_core::{Actor, Engine, EventKind, Hub, MemoryLog, Role, RoleCatalog, Workflow};
    use futures::StreamExt;
    use tower::ServiceExt;

    async fn app() -> (Router, Arc<Hub>) {
        let role = Role::parse("---\nid: pm\ntitle: Project Manager\n---\nPM\n").expect("role");
        let engine =
            Engine::new("Acme", RoleCatalog::new([role]), Workflow::default()).with_hardware_max(8);
        let hub = Arc::new(
            Hub::open(engine, Arc::new(MemoryLog::default()))
                .await
                .expect("hub"),
        );
        (
            router(hub.clone(), None, None, None, None, Theme::embedded()),
            hub,
        )
    }

    fn get(uri: &str) -> Request<Body> {
        Request::get(uri)
            .header("host", "127.0.0.1:4747")
            .body(Body::empty())
            .expect("request")
    }

    async fn body_json(resp: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body");
        serde_json::from_slice(&bytes).expect("json")
    }

    struct Fixed(Vec<InstanceInfo>);

    #[async_trait::async_trait]
    impl InstanceSource for Fixed {
        async fn list(&self) -> Vec<InstanceInfo> {
            self.0.clone()
        }
    }

    #[tokio::test]
    async fn health_names_the_project_and_instances_lists_the_registry() {
        let (plain, hub) = app().await;
        let none = body_json(
            plain
                .oneshot(get("/api/v1/instances"))
                .await
                .expect("response"),
        )
        .await;
        assert_eq!(none, json!({"current": null, "instances": []}));

        let other = InstanceInfo {
            root: "/work/b".into(),
            name: "B".into(),
            url: "http://127.0.0.1:4748".into(),
            pid: 7,
            started_at: "2026-10-24T10:00:00Z".into(),
        };
        let app = router_with(
            hub,
            None,
            None,
            None,
            None,
            Theme::embedded(),
            ServerOptions::default()
                .with_info(ServerInfo {
                    name: "Acme".into(),
                    root: "/work/a".into(),
                })
                .with_instances(Arc::new(Fixed(vec![other]))),
        );
        let health = body_json(
            app.clone()
                .oneshot(get("/api/v1/health"))
                .await
                .expect("response"),
        )
        .await;
        assert_eq!(
            health["project"],
            json!({"name": "Acme", "root": "/work/a"})
        );
        let list = body_json(
            app.oneshot(get("/api/v1/instances"))
                .await
                .expect("response"),
        )
        .await;
        assert_eq!(list["current"], "/work/a");
        assert_eq!(list["instances"][0]["name"], "B");
        assert_eq!(list["instances"][0]["pid"], 7);
    }

    fn hire() -> Request<Body> {
        Request::post("/api/v1/commands")
            .header("host", "localhost")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"type":"hire_employee","role":"pm","name":"Pat"}"#,
            ))
            .expect("request")
    }

    #[tokio::test]
    async fn health_state_and_roles() {
        let (app, _) = app().await;
        let resp = app
            .clone()
            .oneshot(get("/api/v1/health"))
            .await
            .expect("response");
        assert_eq!(resp.status(), StatusCode::OK);
        let health = body_json(resp).await;
        assert_eq!(health["status"], "ok");
        assert_eq!(health["api"], "v1");
        assert!(health.get("project").is_none(), "no identity without info");

        let roles = body_json(
            app.clone()
                .oneshot(get("/api/v1/roles"))
                .await
                .expect("response"),
        )
        .await;
        assert_eq!(roles[0]["id"], "pm");

        let state = body_json(app.oneshot(get("/api/v1/state")).await.expect("response")).await;
        assert_eq!(state["company"], "Acme");
        assert_eq!(state["workflow_states"][0], "backlog");
    }

    #[tokio::test]
    async fn company_chat_route_posts_to_everyone_and_leaves_chats_alone() {
        let (app, _) = app().await;
        app.clone().oneshot(hire()).await.expect("hired");
        let post = |body: &'static str| {
            Request::post("/api/v1/company-chat")
                .header("host", "localhost")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .expect("request")
        };
        let resp = app
            .clone()
            .oneshot(post(r#"{"text":"all hands at 10","to":null}"#))
            .await
            .expect("r");
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(body_json(resp).await["seq"].as_u64().is_some());
        let state = body_json(app.clone().oneshot(get("/api/v1/state")).await.expect("r")).await;
        assert_eq!(state["company_chat"][0]["text"], "all hands at 10");
        assert_eq!(state["company_chat"][0]["from_name"], "Stakeholder");
        assert!(
            state["chats"]["pat"]
                .as_array()
                .is_none_or(|lines| lines.iter().all(|l| l["text"] != "all hands at 10"))
        );
        for bad in [r#"{"text":"  "}"#, r#"{"text":"x","to":"ghost"}"#] {
            let resp = app.clone().oneshot(post(bad)).await.expect("r");
            assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
        }
        let cmd = Request::post("/api/v1/commands")
            .header("host", "localhost")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"type":"post_company_chat","text":"@pat hi"}"#,
            ))
            .expect("request");
        let resp = app.oneshot(cmd).await.expect("r");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn clear_chat_route_empties_the_chat_and_refuses_unknown_people() {
        let (app, hub) = app().await;
        app.clone().oneshot(hire()).await.expect("hired");
        hub.send(
            Actor::Stakeholder,
            Command::SendChat {
                to: "pat".into(),
                text: "hi".into(),
            },
        )
        .await
        .expect("chat");
        let post = |id: &str| {
            Request::post(format!("/api/v1/employees/{id}/chat/clear"))
                .header("host", "localhost")
                .body(Body::empty())
                .expect("request")
        };
        let resp = app.clone().oneshot(post("pat")).await.expect("r");
        assert_eq!(resp.status(), StatusCode::OK);
        let state = body_json(app.clone().oneshot(get("/api/v1/state")).await.expect("r")).await;
        assert!(
            state["chats"]["pat"]
                .as_array()
                .is_none_or(|lines| lines.is_empty())
        );
        let resp = app.oneshot(post("nobody")).await.expect("r");
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn cancel_route_is_a_noop_when_idle_and_refuses_unknown_people() {
        let (app, _) = app().await;
        app.clone().oneshot(hire()).await.expect("hired");
        let post = |id: &str| {
            Request::post(format!("/api/v1/employees/{id}/cancel"))
                .header("host", "localhost")
                .body(Body::empty())
                .expect("request")
        };
        let resp = app.clone().oneshot(post("pat")).await.expect("r");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["events"], serde_json::json!([]));
        let resp = app.oneshot(post("nobody")).await.expect("r");
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn prioritize_route_marks_a_ticket_urgent_and_clears_it() {
        let (app, _) = app().await;
        let created = app
            .clone()
            .oneshot(post_json(
                "/api/v1/commands",
                serde_json::json!({"type":"create_ticket","kind":"story","title":"Login"}),
            ))
            .await
            .expect("r");
        assert_eq!(created.status(), StatusCode::OK);
        let post = |body: Option<&str>| {
            Request::post("/api/v1/tickets/T-1/prioritize")
                .header("host", "localhost")
                .header("content-type", "application/json")
                .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
                .expect("request")
        };
        let resp = app.clone().oneshot(post(None)).await.expect("r");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["events"][0]["type"], "ticket_prioritized");
        assert_eq!(body["events"][0]["urgent"], true);
        let state = body_json(app.clone().oneshot(get("/api/v1/state")).await.expect("r")).await;
        assert_eq!(state["tickets"][0]["urgent"], true);
        let resp = app
            .clone()
            .oneshot(post(Some(r#"{"urgent":false}"#)))
            .await
            .expect("r");
        let body = body_json(resp).await;
        assert_eq!(body["events"][0]["urgent"], false);
        let resp = app.clone().oneshot(post(Some("nope"))).await.expect("r");
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let resp = app
            .oneshot(
                Request::post("/api/v1/tickets/T-9/prioritize")
                    .header("host", "localhost")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("r");
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn agent_limit_route_sets_the_limit_and_refuses_bad_values() {
        let (app, _) = app().await;
        let put = |body: &str| {
            Request::put("/api/v1/budget/max-concurrent-agents")
                .header("host", "localhost")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("request")
        };
        let resp = app.clone().oneshot(put(r#"{"n":4}"#)).await.expect("r");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["events"][0]["type"], "max_concurrent_agents_changed");
        assert_eq!(body["events"][0]["n"], 4);
        assert_eq!(body["events"][0]["hardware_max"], 8);
        let state = body_json(app.clone().oneshot(get("/api/v1/state")).await.expect("r")).await;
        assert_eq!(state["agent_limit"]["limit"], 4);
        assert_eq!(state["agent_limit"]["hardware_max"], 8);
        for bad in [r#"{"n":0}"#, r#"{"n":9}"#, r#"{"n":"x"}"#, "nope"] {
            let resp = app.clone().oneshot(put(bad)).await.expect("r");
            assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
        }
    }

    #[tokio::test]
    async fn set_employee_agent_command_changes_the_hire_and_refuses_unknown_people() {
        let (app, _) = app().await;
        app.clone().oneshot(hire()).await.expect("hired");
        let resp = app
            .clone()
            .oneshot(post_json(
                "/api/v1/commands",
                serde_json::json!({"type":"set_employee_agent","employee":"pat","backend":"codex","model":"gpt-5"}),
            ))
            .await
            .expect("r");
        assert_eq!(resp.status(), StatusCode::OK);
        let state = body_json(app.clone().oneshot(get("/api/v1/state")).await.expect("r")).await;
        let pat = &state["employees"][0];
        assert_eq!(
            (pat["backend"].as_str(), pat["model"].as_str()),
            (Some("codex"), Some("gpt-5"))
        );
        let resp = app
            .oneshot(post_json(
                "/api/v1/commands",
                serde_json::json!({"type":"set_employee_agent","employee":"nobody"}),
            ))
            .await
            .expect("r");
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn set_employee_agent_when_idle_queues_while_the_employee_replies() {
        let (app, hub) = app().await;
        app.clone().oneshot(hire()).await.expect("hired");
        hub.record(
            Actor::System,
            EventKind::AgentMessageChunk {
                employee: "pat".into(),
                text: "thinking".into(),
            },
        )
        .await
        .expect("chunk");
        let send = |when_idle: bool| {
            post_json(
                "/api/v1/commands",
                serde_json::json!({"type":"set_employee_agent","employee":"pat","backend":"codex","when_idle":when_idle}),
            )
        };
        let resp = app.clone().oneshot(send(false)).await.expect("r");
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let resp = app.clone().oneshot(send(true)).await.expect("r");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["events"][0]["type"], "employee_agent_change_queued");
        let state = body_json(app.oneshot(get("/api/v1/state")).await.expect("r")).await;
        let pat = &state["employees"][0];
        assert_eq!(pat["pending_agent"]["backend"], "codex");
        assert_ne!(pat["backend"], "codex", "not applied yet");
    }

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nimage-bytes";

    async fn attach_app() -> (Router, Arc<Hub>, tempfile::TempDir) {
        let role = Role::parse("---\nid: pm\ntitle: Project Manager\n---\nPM\n").expect("role");
        let engine = Engine::new("Acme", RoleCatalog::new([role]), Workflow::default());
        let hub = Arc::new(
            Hub::open(engine, Arc::new(MemoryLog::default()))
                .await
                .expect("hub"),
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let store = chillax_store::FileAttachmentStore::new(dir.path().join("a"));
        let app = router(
            hub.clone(),
            None,
            None,
            Some(Arc::new(store)),
            None,
            Theme::embedded(),
        );
        (app, hub, dir)
    }

    fn upload(name: &str, body: Vec<u8>) -> Request<Body> {
        Request::post("/api/v1/attachments")
            .header("host", "localhost")
            .header("x-filename", name)
            .body(Body::from(body))
            .expect("request")
    }

    #[tokio::test]
    async fn attachments_upload_comment_and_download() {
        let (app, hub, _dir) = attach_app().await;
        let resp = app
            .clone()
            .oneshot(upload("a%20shot.png", PNG.to_vec()))
            .await
            .expect("r");
        assert_eq!(resp.status(), StatusCode::OK);
        let att = body_json(resp).await;
        assert_eq!(att["name"], "a shot.png");
        assert_eq!(att["mime"], "image/png");
        let id = att["id"].as_str().expect("id").to_string();

        let create = post_json(
            "/api/v1/commands",
            serde_json::json!({"type":"create_ticket","kind":"bug","title":"Typo"}),
        );
        assert_eq!(
            app.clone().oneshot(create).await.expect("r").status(),
            StatusCode::OK
        );
        // The client lies about the size and type; the store's answer wins.
        let comment = serde_json::json!({
            "type": "comment_ticket", "id": "T-1", "body": "",
            "attachments": [{"id": id, "name": "../x/y.png", "mime": "image/gif", "bytes": 1}]
        });
        let resp = app
            .clone()
            .oneshot(post_json("/api/v1/commands", comment))
            .await
            .expect("r");
        assert_eq!(resp.status(), StatusCode::OK);
        let state = hub.snapshot().await;
        let a = &state.tickets[0].comments[0].attachments[0];
        assert_eq!(
            (a.name.as_str(), a.mime.as_str(), a.bytes),
            ("y.png", "image/png", PNG.len() as u64)
        );

        let resp = app
            .clone()
            .oneshot(get(&format!("/api/v1/attachments/{id}")))
            .await
            .expect("r");
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()["content-type"], "image/png");
        assert_eq!(resp.headers()["x-content-type-options"], "nosniff");
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("b");
        assert_eq!(&bytes[..], PNG);

        // The host check applies like everywhere else.
        let foreign = Request::get(format!("/api/v1/attachments/{id}"))
            .header("host", "evil.example")
            .body(Body::empty())
            .expect("request");
        assert_eq!(
            app.oneshot(foreign).await.expect("r").status(),
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn bad_uploads_and_ids_are_refused() {
        let (app, _, _dir) = attach_app().await;
        let svg = app
            .clone()
            .oneshot(upload("a.png", b"<svg onload=x/>".to_vec()))
            .await
            .expect("r");
        assert_eq!(svg.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let mut big = PNG.to_vec();
        big.resize(
            chillax_core::attachment::MAX_ATTACHMENT_BYTES as usize + 1,
            0,
        );
        let big = app.clone().oneshot(upload("a.png", big)).await.expect("r");
        assert_eq!(big.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let empty = app
            .clone()
            .oneshot(upload("a.png", vec![]))
            .await
            .expect("r");
        assert_eq!(empty.status(), StatusCode::PAYLOAD_TOO_LARGE);
        for bad in ["..%2F..%2Fetc%2Fpasswd", "nothex", &"a".repeat(64)] {
            let resp = app
                .clone()
                .oneshot(get(&format!("/api/v1/attachments/{bad}")))
                .await
                .expect("r");
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{bad}");
        }
        // A comment naming an image the store does not have is rejected.
        let create = post_json(
            "/api/v1/commands",
            serde_json::json!({"type":"create_ticket","kind":"bug","title":"Typo"}),
        );
        app.clone().oneshot(create).await.expect("r");
        let comment = serde_json::json!({
            "type": "comment_ticket", "id": "T-1", "body": "x",
            "attachments": [{"id": "a".repeat(64), "name": "n.png", "mime": "image/png", "bytes": 3}]
        });
        let resp = app
            .oneshot(post_json("/api/v1/commands", comment))
            .await
            .expect("r");
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn commands_are_applied_or_rejected() {
        let (app, hub) = app().await;
        let resp = app.clone().oneshot(hire()).await.expect("response");
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await["events"][0]["type"], "employee_hired");
        assert_eq!(hub.snapshot().await.employees.len(), 1);

        let bad = Request::post("/api/v1/commands")
            .header("host", "localhost")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"type":"approve_sprint","id":"SP-1"}"#))
            .expect("request");
        let resp = app.oneshot(bad).await.expect("response");
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            body_json(resp).await["error"]
                .as_str()
                .expect("message")
                .contains("SP-1")
        );
    }

    #[tokio::test]
    async fn foreign_hosts_are_rejected() {
        let (app, _) = app().await;
        let req = Request::get("/api/v1/health")
            .header("host", "evil.example:4747")
            .body(Body::empty())
            .expect("request");
        assert_eq!(
            app.oneshot(req).await.expect("response").status(),
            StatusCode::FORBIDDEN
        );
    }

    async fn record_usage(hub: &Hub, ticket: Option<&str>, tokens: u64, cost: Option<u64>) {
        hub.record(
            Actor::System,
            chillax_core::EventKind::UsageRecorded {
                employee: "pat".into(),
                ticket: ticket.map(Into::into),
                session: "s".into(),
                model: Some("m".into()),
                input_tokens: tokens,
                output_tokens: 1,
                cached_tokens: 0,
                cache_write_tokens: 0,
                cost_micros: cost,
                currency: "USD".into(),
                estimated: false,
                context_used: Some(50),
                context_size: Some(200),
            },
        )
        .await
        .expect("record");
    }

    #[tokio::test]
    async fn usage_on_an_empty_state_is_zero_filled() {
        let (app, _) = app().await;
        let resp = app.oneshot(get("/api/v1/usage")).await.expect("response");
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["days"], 14);
        assert_eq!(v["daily"].as_array().expect("daily").len(), 14);
        assert_eq!(v["total"]["turns"], 0);
        assert_eq!(v["cost"]["estimated"], false);
        assert_eq!(v["burn"]["tokens_per_day_7d"], 0);
        assert!(v["employees"].as_array().expect("employees").is_empty());
        assert!(v["recent"].as_array().expect("recent").is_empty());
    }

    #[tokio::test]
    async fn usage_refresh_waits_for_the_runtime_flush() {
        let (app, hub) = app().await;
        app.clone().oneshot(hire()).await.expect("hire");
        // Stand-in for the agent runtime: on the request, record a flushed sample, then
        // say it is done.
        let mut rx = hub.subscribe();
        let runtime = Arc::clone(&hub);
        tokio::spawn(async move {
            while let Ok(event) = rx.recv().await {
                if matches!(event.kind, chillax_core::EventKind::UsageRefreshRequested) {
                    record_usage(&runtime, Some("T-1"), 7, Some(500)).await;
                    runtime
                        .record(Actor::System, chillax_core::EventKind::UsageRefreshed)
                        .await
                        .expect("record");
                }
            }
        });
        let req = Request::post("/api/v1/usage/refresh?days=1")
            .header("host", "localhost")
            .body(Body::empty())
            .expect("request");
        let resp = app.oneshot(req).await.expect("response");
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["total"]["input_tokens"], 7);
        assert_eq!(v["cost"]["total_micros"], 500);
    }

    #[tokio::test]
    async fn usage_with_samples_and_clamped_parameters() {
        let (app, hub) = app().await;
        app.clone().oneshot(hire()).await.expect("hire");
        record_usage(&hub, Some("T-1"), 100, Some(2_000)).await;
        record_usage(&hub, None, 10, None).await;
        let resp = app
            .oneshot(get("/api/v1/usage?days=0&top=9999"))
            .await
            .expect("response");
        let v = body_json(resp).await;
        assert_eq!(v["days"], 1);
        assert_eq!(v["daily"].as_array().expect("daily").len(), 1);
        assert_eq!(v["daily"][0]["turns"], 2);
        assert_eq!(v["total"]["input_tokens"], 110);
        assert_eq!(v["cost"]["total_micros"], 2_000);
        assert_eq!(v["cost"]["unpriced_samples"], 1);
        assert_eq!(v["windows"]["today"]["turns"], 2);
        assert_eq!(v["employees"][0]["id"], "pat");
        assert_eq!(v["employees"][0]["name"], "Pat");
        assert_eq!(v["employees"][0]["state"], "ok");
        assert_eq!(v["tickets"][0]["id"], "T-1");
        assert!(v["tickets"][0]["title"].is_null());
        assert_eq!(v["tickets_total"], 1);
        assert_eq!(v["models"][0]["model"], "m");
        assert_eq!(v["context"][0]["percent"], 25);
        assert_eq!(v["recent"][0]["employee_name"], "Pat");
    }

    #[tokio::test]
    async fn usage_without_top_lists_every_ticket_and_has_hourly() {
        let (app, hub) = app().await;
        app.clone().oneshot(hire()).await.expect("hire");
        for n in 0..12 {
            record_usage(&hub, Some(&format!("T-{n}")), 10, Some(100)).await;
        }
        let resp = app
            .clone()
            .oneshot(get("/api/v1/usage"))
            .await
            .expect("response");
        let v = body_json(resp).await;
        assert_eq!(v["tickets"].as_array().expect("tickets").len(), 12);
        assert_eq!(v["tickets_total"], 12);
        let hourly = v["hourly"].as_array().expect("hourly");
        assert_eq!(hourly.len(), 48);
        assert_eq!(hourly[47]["turns"], 12);
        let resp = app
            .oneshot(get("/api/v1/usage?top=5"))
            .await
            .expect("response");
        let v = body_json(resp).await;
        assert_eq!(v["tickets"].as_array().expect("tickets").len(), 5);
    }

    #[tokio::test]
    async fn usage_rejects_foreign_hosts() {
        let (app, _) = app().await;
        let req = Request::get("/api/v1/usage")
            .header("host", "evil.example:4747")
            .body(Body::empty())
            .expect("request");
        assert_eq!(
            app.oneshot(req).await.expect("response").status(),
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn sse_replays_then_streams_live() {
        let (app, _) = app().await;
        app.clone().oneshot(hire()).await.expect("first hire");
        let resp = app
            .clone()
            .oneshot(get("/api/v1/events?since=0"))
            .await
            .expect("response");
        assert_eq!(resp.headers()[header::CONTENT_TYPE], "text/event-stream");
        let mut body = resp.into_body().into_data_stream();

        let first = String::from_utf8(body.next().await.expect("chunk").expect("bytes").to_vec())
            .expect("utf8");
        assert!(
            first.contains("event: chillax") && first.contains("id: 1"),
            "{first}"
        );

        app.oneshot(hire()).await.expect("second hire");
        let second = String::from_utf8(body.next().await.expect("chunk").expect("bytes").to_vec())
            .expect("utf8");
        assert!(
            second.contains("id: 2") && second.contains("pat-2"),
            "{second}"
        );
    }

    #[tokio::test]
    async fn serves_the_embedded_theme() {
        let (app, _) = app().await;
        let resp = app.clone().oneshot(get("/")).await.expect("response");
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            resp.headers()[header::CONTENT_TYPE]
                .to_str()
                .expect("ascii")
                .starts_with("text/html")
        );
        let js = app
            .clone()
            .oneshot(get("/js/main.js"))
            .await
            .expect("response");
        assert_eq!(js.headers()[header::CONTENT_TYPE], "text/javascript");
        let spec = body_json(
            app.oneshot(get("/api/v1/openapi.json"))
                .await
                .expect("response"),
        )
        .await;
        assert_eq!(spec["openapi"], "3.1.0");
        assert!(spec["paths"]["/agents/{name}/efforts"]["get"].is_object());
        assert!(spec["paths"]["/usage"]["get"].is_object());
        assert!(spec["paths"]["/usage/refresh"]["post"].is_object());
        assert!(spec["paths"]["/employees/{id}/cancel"]["post"].is_object());
        assert!(spec["paths"]["/tickets/{id}/prioritize"]["post"].is_object());
        assert!(spec["paths"]["/budget/max-concurrent-agents"]["put"].is_object());
        let commands = spec["components"]["schemas"]["Command"]["properties"]["type"]["enum"]
            .as_array()
            .expect("command enum");
        assert!(commands.iter().any(|c| c == "cancel_turn"));
        assert!(commands.iter().any(|c| c == "prioritize_ticket"));
        assert!(commands.iter().any(|c| c == "set_max_concurrent_agents"));
        let text = spec.to_string();
        for name in ["when_idle", "employee_agent_change_queued", "pending_agent"] {
            assert!(text.contains(name), "{name} is documented");
        }
        assert!(spec["components"]["schemas"]["UsageView"].is_object());
    }

    /// The theme is a tree of ES modules: every embedded file must be served with a type the
    /// browser accepts, and every relative import must point at a file that is embedded.
    #[tokio::test]
    async fn every_embedded_theme_file_is_served_and_its_imports_resolve() {
        let (app, _) = app().await;
        let paths = theme::embedded_paths();
        assert!(paths.iter().any(|p| p == "index.html"), "{paths:?}");
        assert!(paths.iter().any(|p| p == "theme.toml"), "{paths:?}");
        for path in &paths {
            let resp = app
                .clone()
                .oneshot(get(&format!("/{path}")))
                .await
                .expect("response");
            assert_eq!(resp.status(), StatusCode::OK, "{path}");
            let want = match path.rsplit('.').next() {
                Some("html") => "text/html; charset=utf-8",
                Some("js") => "text/javascript",
                Some("css") => "text/css",
                _ => "application/octet-stream",
            };
            assert_eq!(resp.headers()[header::CONTENT_TYPE], want, "{path}");
            if path.ends_with(".js") {
                let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .expect("body");
                let source = String::from_utf8(bytes.to_vec()).expect("utf8");
                let dir = path.rsplit_once('/').map_or("", |(d, _)| d);
                for line in source.lines() {
                    let Some((_, spec)) = line.split_once(" from \"") else {
                        continue;
                    };
                    let Some((spec, _)) = spec.split_once('"') else {
                        continue;
                    };
                    if !spec.starts_with('.') {
                        continue;
                    }
                    let mut parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
                    for seg in spec.split('/') {
                        match seg {
                            "." => {}
                            ".." => {
                                parts.pop();
                            }
                            s => parts.push(s),
                        }
                    }
                    let target = parts.join("/");
                    assert!(paths.contains(&target), "{path} imports missing {target}");
                }
            }
        }
        let index = app.oneshot(get("/")).await.expect("response");
        let html = axum::body::to_bytes(index.into_body(), usize::MAX)
            .await
            .expect("body");
        let html = String::from_utf8(html.to_vec()).expect("utf8");
        assert!(html.contains("type=\"module\" src=\"js/main.js\""));
    }

    #[tokio::test]
    async fn openapi_lists_the_qa_and_release_surface() {
        let (app, _) = app().await;
        let spec = body_json(
            app.oneshot(get("/api/v1/openapi.json"))
                .await
                .expect("response"),
        )
        .await;
        let commands: Vec<&str> =
            spec["components"]["schemas"]["Command"]["properties"]["type"]["enum"]
                .as_array()
                .expect("command enum")
                .iter()
                .filter_map(|v| v.as_str())
                .collect();
        for tag in [
            "hire_teammate",
            "report_app",
            "promote_to_qa",
            "retry_env",
            "open_release_gate",
            "approve_release",
            "reject_release",
            "dismiss_alert",
            "drop_ticket",
            "request_sprint_planning",
            "close_sprint",
            "submit_request",
            "start_app",
            "stop_app",
            "restart_app",
            "resolve_problem",
            "retry_action",
            "restart_employee",
        ] {
            assert!(commands.contains(&tag), "missing command {tag}");
        }
        for schema in [
            "QaRound",
            "Release",
            "InboxItem",
            "EnvStep",
            "QaVerdict",
            "RunningApp",
        ] {
            assert!(
                spec["components"]["schemas"][schema].is_object(),
                "missing schema {schema}"
            );
        }
        let inbox = spec["components"]["schemas"]["InboxItem"].to_string();
        for tag in ["env_alert", "release_gate", "alert"] {
            assert!(inbox.contains(tag), "InboxItem lacks {tag}");
        }
        let text = spec.to_string();
        for needle in [
            "qa_round_started",
            "release_tagged",
            "deploy_run",
            "found_in",
            "released",
            "env_retry",
            "pr_opened",
            "pr_url",
            "record_pr",
            "ticket_dropped",
            "sprint_closed",
            "app_crashed",
            "restart_app",
        ] {
            assert!(text.contains(needle), "spec never mentions {needle}");
        }
    }

    #[tokio::test]
    async fn promote_to_qa_reaches_the_engine() {
        let (app, _) = app().await;
        let req = Request::post("/api/v1/commands")
            .header("host", "localhost")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"type":"promote_to_qa"}"#))
            .expect("request");
        let resp = app.oneshot(req).await.expect("response");
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            body_json(resp).await["error"]
                .as_str()
                .expect("message")
                .contains("nothing is waiting")
        );
    }

    mod catalog {
        use std::sync::Mutex;

        use async_trait::async_trait;
        use chillax_core::{
            CatalogStatus, PersonaSummary, RecruitError, Recruiter, Revision, Role, RoleDraft,
        };

        use super::*;

        /// One persona; adopting sends the real `AdoptRole` command to the hub.
        pub struct FakeRecruiter {
            pub company: Arc<Hub>,
            pub draft: Mutex<Option<RoleDraft>>,
        }

        fn persona() -> PersonaSummary {
            PersonaSummary {
                catalog: "agency".into(),
                slug: "backend".into(),
                path: "eng/backend.md".into(),
                name: "Backend Architect".into(),
                description: "APIs".into(),
                division: "Engineering".into(),
                emoji: String::new(),
                vibe: String::new(),
                hires: 0,
                adopted_role: None,
                suggested_base: Some("pm".into()),
            }
        }

        #[async_trait]
        impl Recruiter for FakeRecruiter {
            async fn status(&self) -> CatalogStatus {
                CatalogStatus {
                    name: "agency".into(),
                    personas: 1,
                    ..CatalogStatus::default()
                }
            }
            async fn refresh(&self) -> Result<CatalogStatus, RecruitError> {
                Err(RecruitError::Refresh("offline".into()))
            }
            async fn personas(&self) -> Result<Vec<PersonaSummary>, RecruitError> {
                Ok(vec![persona()])
            }
            async fn featured(&self, _: usize) -> Result<Vec<PersonaSummary>, RecruitError> {
                Ok(vec![persona()])
            }
            async fn draft(
                &self,
                slug: &str,
                base_role: Option<&str>,
                _: Option<String>,
                model: Option<String>,
            ) -> Result<RoleDraft, RecruitError> {
                if slug != "backend" {
                    return Err(RecruitError::UnknownPersona(slug.into()));
                }
                let draft = RoleDraft {
                    id: "d1".into(),
                    persona: persona(),
                    from_role: None,
                    base_role: base_role.unwrap_or("pm").into(),
                    backend: "claude".into(),
                    model,
                    role_id: "backend".into(),
                    base_prompt: "PM".into(),
                    original: "You design APIs.".into(),
                    persona_text: "You design APIs.".into(),
                    findings: vec![],
                    revisions: vec![],
                    diff: vec![],
                    prompt: "PM\n\nYou design APIs.".into(),
                    project_files: vec!["plan.md".into()],
                };
                *self.draft.lock().expect("lock") = Some(draft.clone());
                Ok(draft)
            }
            async fn draft_from_role(
                &self,
                from_role: &str,
                new_id: &str,
                title: &str,
                _: Option<String>,
                _: Option<String>,
            ) -> Result<RoleDraft, RecruitError> {
                Ok(RoleDraft {
                    id: "d2".into(),
                    persona: persona(),
                    from_role: Some(from_role.into()),
                    base_role: "pm".into(),
                    backend: "claude".into(),
                    model: None,
                    role_id: new_id.into(),
                    base_prompt: "PM".into(),
                    original: title.into(),
                    persona_text: title.into(),
                    findings: vec![],
                    revisions: vec![],
                    diff: vec![],
                    prompt: "PM".into(),
                    project_files: vec![],
                })
            }
            async fn get_draft(&self, id: &str) -> Result<RoleDraft, RecruitError> {
                self.draft
                    .lock()
                    .expect("lock")
                    .clone()
                    .filter(|d| d.id == id)
                    .ok_or_else(|| RecruitError::UnknownDraft(id.into()))
            }
            async fn set_base(&self, id: &str, base: &str) -> Result<RoleDraft, RecruitError> {
                let mut draft = self.get_draft(id).await?;
                if base != "pm" && base != "qa" {
                    return Err(RecruitError::UnknownRole(base.into()));
                }
                draft.base_role = base.into();
                *self.draft.lock().expect("lock") = Some(draft.clone());
                Ok(draft)
            }
            async fn revise(&self, id: &str, instruction: &str) -> Result<RoleDraft, RecruitError> {
                let mut draft = self.get_draft(id).await?;
                draft.revisions.push(Revision {
                    instruction: instruction.into(),
                    summary: "ok".into(),
                    text: instruction.into(),
                });
                draft.persona_text = instruction.into();
                *self.draft.lock().expect("lock") = Some(draft.clone());
                Ok(draft)
            }
            async fn tailor(&self, id: &str) -> Result<RoleDraft, RecruitError> {
                self.revise(id, "tailored to the project").await
            }
            async fn undo(&self, _: &str) -> Result<RoleDraft, RecruitError> {
                Err(RecruitError::NothingToUndo)
            }
            async fn adopt(
                &self,
                id: &str,
                role_id: Option<String>,
                overwrite: bool,
            ) -> Result<Role, RecruitError> {
                let draft = self.get_draft(id).await?;
                let mut role = Role::parse(&format!(
                    "---\nid: {}\ntitle: Backend Architect\nbase_role: pm\n---\n{}\n",
                    role_id.unwrap_or(draft.role_id),
                    draft.persona_text
                ))
                .expect("role");
                role.source = Some(chillax_core::RoleSource {
                    catalog: "agency".into(),
                    repo: "r".into(),
                    commit: "c".into(),
                    path: "eng/backend.md".into(),
                    blob: "b".into(),
                    license: None,
                    edited: true,
                    copy_of: None,
                });
                self.company
                    .send(
                        Actor::Stakeholder,
                        Command::AdoptRole {
                            markdown: role.to_markdown().expect("markdown"),
                            revisions: vec![],
                            overwrite,
                        },
                    )
                    .await?;
                Ok(role)
            }
            async fn discard(&self, _: &str) {
                *self.draft.lock().expect("lock") = None;
            }
        }
    }

    async fn catalog_app() -> (Router, Arc<Hub>) {
        let (_, hub) = app().await;
        let recruiter = Arc::new(catalog::FakeRecruiter {
            company: hub.clone(),
            draft: Default::default(),
        });
        (
            router(
                hub.clone(),
                Some(recruiter),
                None,
                None,
                None,
                Theme::embedded(),
            ),
            hub,
        )
    }

    fn post_json(uri: &str, body: serde_json::Value) -> Request<Body> {
        Request::post(uri)
            .header("host", "localhost")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("request")
    }

    #[tokio::test]
    async fn catalog_routes_are_404_when_disabled() {
        let (app, _) = app().await;
        let resp = app
            .oneshot(get("/api/v1/personas"))
            .await
            .expect("response");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(
            body_json(resp).await["error"]
                .as_str()
                .expect("message")
                .contains("disabled")
        );
    }

    #[tokio::test]
    async fn search_review_revise_and_adopt_with_hire() {
        let (app, hub) = catalog_app().await;
        let status = body_json(
            app.clone()
                .oneshot(get("/api/v1/catalog"))
                .await
                .expect("r"),
        )
        .await;
        assert_eq!(status["personas"], 1);
        let refresh = app
            .clone()
            .oneshot(post_json("/api/v1/catalog/refresh", json!({})))
            .await
            .expect("r");
        assert_eq!(refresh.status(), StatusCode::BAD_GATEWAY);

        let top = body_json(
            app.clone()
                .oneshot(get("/api/v1/personas"))
                .await
                .expect("r"),
        )
        .await;
        assert_eq!(top[0]["slug"], "backend");
        let found = body_json(
            app.clone()
                .oneshot(get("/api/v1/personas?q=api&limit=5"))
                .await
                .expect("r"),
        )
        .await;
        assert_eq!(found[0]["name"], "Backend Architect");
        let all = body_json(
            app.clone()
                .oneshot(get("/api/v1/personas?all=true"))
                .await
                .expect("r"),
        )
        .await;
        assert_eq!(all.as_array().map(Vec::len), Some(1));

        let missing = app
            .clone()
            .oneshot(post_json(
                "/api/v1/drafts",
                json!({"persona": "nope", "base_role": "pm"}),
            ))
            .await
            .expect("r");
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        let draft = body_json(
            app.clone()
                .oneshot(post_json("/api/v1/drafts", json!({"persona": "backend"})))
                .await
                .expect("r"),
        )
        .await;
        assert_eq!(draft["id"], "d1");
        assert_eq!(draft["base_role"], "pm", "defaults to the suggested base");
        assert_eq!(draft["persona"]["suggested_base"], "pm");

        // A draft from an existing role: `{from_role, role_id, title}` instead of `persona`.
        let copy = body_json(
            app.clone()
                .oneshot(post_json(
                    "/api/v1/drafts",
                    json!({"from_role": "pm", "role_id": "pm-lite", "title": "PM Lite"}),
                ))
                .await
                .expect("r"),
        )
        .await;
        assert_eq!(copy["from_role"], "pm");
        assert_eq!(copy["role_id"], "pm-lite");
        let both = app
            .clone()
            .oneshot(post_json(
                "/api/v1/drafts",
                json!({"from_role": "pm", "role_id": "x", "title": "X", "base_role": "pm"}),
            ))
            .await
            .expect("r");
        assert_eq!(both.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let neither = app
            .clone()
            .oneshot(post_json("/api/v1/drafts", json!({})))
            .await
            .expect("r");
        assert_eq!(neither.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let revised = body_json(
            app.clone()
                .oneshot(post_json(
                    "/api/v1/drafts/d1/revise",
                    json!({"instruction": "Rust only"}),
                ))
                .await
                .expect("r"),
        )
        .await;
        assert_eq!(revised["persona_text"], "Rust only");
        assert_eq!(revised["project_files"], json!(["plan.md"]));
        let tailored = body_json(
            app.clone()
                .oneshot(post_json("/api/v1/drafts/d1/tailor", json!({})))
                .await
                .expect("r"),
        )
        .await;
        assert_eq!(tailored["persona_text"], "tailored to the project");
        assert_eq!(tailored["revisions"].as_array().map(Vec::len), Some(2));
        let moved = body_json(
            app.clone()
                .oneshot(post_json(
                    "/api/v1/drafts/d1/base",
                    json!({"base_role": "qa"}),
                ))
                .await
                .expect("r"),
        )
        .await;
        assert_eq!(moved["base_role"], "qa");
        assert_eq!(moved["revisions"].as_array().map(Vec::len), Some(2));
        let unknown = app
            .clone()
            .oneshot(post_json(
                "/api/v1/drafts/d1/base",
                json!({"base_role": "x"}),
            ))
            .await
            .expect("r");
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
        let undo = app
            .clone()
            .oneshot(post_json("/api/v1/drafts/d1/undo", json!({})))
            .await
            .expect("r");
        assert_eq!(undo.status(), StatusCode::CONFLICT);

        let adopted = app
            .clone()
            .oneshot(post_json(
                "/api/v1/drafts/d1/adopt",
                json!({"hire": {"name": "Bea"}}),
            ))
            .await
            .expect("r");
        assert_eq!(adopted.status(), StatusCode::OK);
        let adopted = body_json(adopted).await;
        assert_eq!(adopted["role"]["base_role"], "pm");
        assert_eq!(adopted["events"][0]["type"], "employee_hired");
        assert_eq!(hub.snapshot().await.employees[0].employee.role, "backend");

        let roles = body_json(app.oneshot(get("/api/v1/roles")).await.expect("r")).await;
        assert!(
            roles
                .as_array()
                .expect("list")
                .iter()
                .any(|r| r["id"] == "backend" && r["source"]["catalog"] == "agency")
        );
    }

    struct FakeAgents;

    #[async_trait::async_trait]
    impl AgentDirectory for FakeAgents {
        async fn backends(&self) -> Vec<chillax_core::BackendInfo> {
            vec![chillax_core::BackendInfo {
                name: "claude".into(),
                title: "Claude Code".into(),
                command: "npx".into(),
                configured: true,
                available: true,
            }]
        }
        async fn models(
            &self,
            backend: &str,
        ) -> Result<Vec<chillax_core::ModelOption>, chillax_core::AgentDirectoryError> {
            match backend {
                "claude" => Ok(vec![chillax_core::ModelOption {
                    id: "sonnet".into(),
                    name: "Sonnet".into(),
                    description: None,
                }]),
                other => Err(chillax_core::AgentDirectoryError::Unknown(other.into())),
            }
        }
        async fn efforts(
            &self,
            backend: &str,
        ) -> Result<Vec<chillax_core::ModelOption>, chillax_core::AgentDirectoryError> {
            match backend {
                "claude" => Ok(vec![chillax_core::ModelOption {
                    id: "high".into(),
                    name: "High".into(),
                    description: None,
                }]),
                "plain" => Ok(Vec::new()),
                other => Err(chillax_core::AgentDirectoryError::Unknown(other.into())),
            }
        }
        async fn enable(
            &self,
            backend: &str,
        ) -> Result<chillax_core::BackendInfo, chillax_core::AgentDirectoryError> {
            Err(chillax_core::AgentDirectoryError::Unknown(backend.into()))
        }
    }

    #[tokio::test]
    async fn agents_routes_list_backends_and_models() {
        let (_, hub) = app().await;
        let app = router(
            hub,
            None,
            Some(Arc::new(FakeAgents)),
            None,
            None,
            Theme::embedded(),
        );
        let backends =
            body_json(app.clone().oneshot(get("/api/v1/agents")).await.expect("r")).await;
        assert_eq!(backends[0]["name"], "claude");
        assert_eq!(backends[0]["configured"], true);
        let models = body_json(
            app.clone()
                .oneshot(get("/api/v1/agents/claude/models"))
                .await
                .expect("r"),
        )
        .await;
        assert_eq!(models[0]["id"], "sonnet");
        let efforts = body_json(
            app.clone()
                .oneshot(get("/api/v1/agents/claude/efforts"))
                .await
                .expect("r"),
        )
        .await;
        assert_eq!(efforts[0]["id"], "high");
        let none = body_json(
            app.clone()
                .oneshot(get("/api/v1/agents/plain/efforts"))
                .await
                .expect("r"),
        )
        .await;
        assert_eq!(none, json!([]));
        let no_efforts = app
            .clone()
            .oneshot(get("/api/v1/agents/nope/efforts"))
            .await
            .expect("r");
        assert_eq!(no_efforts.status(), StatusCode::NOT_FOUND);
        let missing = app
            .clone()
            .oneshot(get("/api/v1/agents/nope/models"))
            .await
            .expect("r");
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        let enable = app
            .oneshot(post_json("/api/v1/agents/nope/enable", json!({})))
            .await
            .expect("r");
        assert_eq!(enable.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn agents_routes_are_404_without_a_directory() {
        let (app, _) = app().await;
        let resp = app.oneshot(get("/api/v1/agents")).await.expect("r");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
