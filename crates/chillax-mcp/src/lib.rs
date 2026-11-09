//! Chillax MCP server, served over streamable HTTP at `/mcp`.
//!
//! Every agent session gets a bearer token that identifies the employee; the role's
//! `mcp_tools` decides which tools that employee sees and may call. Agents never get
//! tracker credentials (AGENTS.md rule 4).

pub mod tools;

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chillax_core::{CompanyHandle, Role};
use chillax_tracker::Tracker;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, ServerHandler};

pub use tools::{AttachmentAccess, TOOLS, ToolContext, ToolError, tools_for};

/// How long an MCP session may sit idle before it is reaped. rmcp's default is 300s, and
/// agents often code for many minutes between MCP calls; a reaped session answers 404, so
/// the agent could no longer call `request_review`. Each request carries its own bearer
/// token, so a long-lived session adds no exposure; it stays finite to cap leaked sessions.
pub const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

/// Bearer tokens → employee ids. Tokens live only in memory; a restart issues new ones.
#[derive(Debug, Default)]
pub struct McpAccess {
    tokens: RwLock<HashMap<String, String>>,
}

impl McpAccess {
    /// Issue (or reuse) the token for an employee.
    pub fn issue(&self, employee: &str) -> String {
        let mut tokens = self.tokens.write().expect("token lock poisoned");
        if let Some((token, _)) = tokens.iter().find(|(_, e)| e.as_str() == employee) {
            return token.clone();
        }
        let token = uuid::Uuid::new_v4().simple().to_string();
        tokens.insert(token.clone(), employee.to_string());
        token
    }

    pub fn employee_for(&self, token: &str) -> Option<String> {
        self.tokens
            .read()
            .expect("token lock poisoned")
            .get(token)
            .cloned()
    }
}

/// The MCP service; cloned per MCP session by the transport.
#[derive(Clone)]
pub struct ChillaxMcp {
    ctx: Arc<ToolContext>,
    access: Arc<McpAccess>,
}

impl ChillaxMcp {
    pub fn new(
        company: Arc<dyn CompanyHandle>,
        tracker: Arc<dyn Tracker>,
        attachments: Option<AttachmentAccess>,
        access: Arc<McpAccess>,
    ) -> Self {
        Self {
            ctx: Arc::new(ToolContext {
                company,
                tracker,
                attachments,
            }),
            access,
        }
    }

    /// A tower service to mount at `/mcp` in the axum router. Sessions idle out after
    /// [`SESSION_IDLE_TIMEOUT`].
    pub fn http_service(self) -> StreamableHttpService<Self, LocalSessionManager> {
        self.http_service_with_keep_alive(Some(SESSION_IDLE_TIMEOUT))
    }

    /// Like [`Self::http_service`] with an explicit idle timeout (`None` never reaps).
    pub fn http_service_with_keep_alive(
        self,
        keep_alive: Option<Duration>,
    ) -> StreamableHttpService<Self, LocalSessionManager> {
        let mut sessions = LocalSessionManager::default();
        sessions.session_config.keep_alive = keep_alive;
        StreamableHttpService::new(
            move || Ok(self.clone()),
            Arc::new(sessions),
            StreamableHttpServerConfig::default(),
        )
    }

    /// Resolve the calling employee and their role from the request's bearer token.
    async fn caller(
        &self,
        context: &RequestContext<RoleServer>,
    ) -> Result<(String, Role), ErrorData> {
        let token = context
            .extensions
            .get::<axum::http::request::Parts>()
            .and_then(|parts| parts.headers.get(axum::http::header::AUTHORIZATION))
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(|| ErrorData::invalid_request("missing bearer token", None))?;
        let employee = self
            .access
            .employee_for(token)
            .ok_or_else(|| ErrorData::invalid_request("unknown token", None))?;
        let state = self.ctx.company.snapshot().await;
        let role_id = state
            .employee(&employee)
            .map(|e| e.employee.role.clone())
            .ok_or_else(|| ErrorData::invalid_request("employee is no longer on the team", None))?;
        let role = self
            .ctx
            .company
            .roles()
            .into_iter()
            .find(|r| r.id == role_id)
            .ok_or_else(|| ErrorData::internal_error("role not found", None))?;
        Ok((employee, role))
    }
}

impl ServerHandler for ChillaxMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("chillax", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Chillax company tools. Use them for all tracker and team work: planning \
                 epics, stories and sprints, updating tickets, and asking the stakeholder.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let (_, role) = self.caller(&context).await?;
        let tools = tools_for(&role)
            .map(|t| {
                let schema = match (t.schema)() {
                    serde_json::Value::Object(map) => map,
                    _ => serde_json::Map::new(),
                };
                Tool::new(t.name, t.description, Arc::new(schema))
            })
            .collect();
        Ok(ListToolsResult {
            tools,
            ..ListToolsResult::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let (employee, role) = self.caller(&context).await?;
        let args = request
            .arguments
            .map(serde_json::Value::Object)
            .unwrap_or(serde_json::Value::Null);
        let result = match self.ctx.call(&employee, &role, &request.name, args).await {
            Ok(value) => CallToolResult::success(vec![ContentBlock::text(
                serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()),
            )]),
            // Tool errors go back to the agent as results so it can adapt.
            Err(err) => CallToolResult::error(vec![ContentBlock::text(err.to_string())]),
        };
        Ok(result.into())
    }
}
