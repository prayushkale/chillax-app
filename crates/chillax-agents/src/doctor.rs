//! `chillax doctor` support: launch a backend and complete an ACP `initialize`
//! handshake, so drifting launch flags are caught early.

use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::InitializeRequest;
use agent_client_protocol::{Agent, Client, ConnectionTo};
use chillax_core::config::AgentConfig;

use crate::acp::describe_error;
use crate::session::agent_process;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentInfo {
    pub name: String,
    pub version: String,
    pub protocol_version: String,
    pub load_session: bool,
    pub mcp_http: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum HandshakeError {
    #[error("timed out after {0:?}")]
    Timeout(Duration),
    #[error("{0}")]
    Acp(String),
}

pub async fn handshake(
    config: &AgentConfig,
    timeout: Duration,
) -> Result<AgentInfo, HandshakeError> {
    let connect = Client.builder().name("chillax-doctor").connect_with(
        agent_process(config),
        async |cx: ConnectionTo<Agent>| {
            let init = cx
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let (name, version) = init
                .agent_info
                .as_ref()
                .map(|i| (i.name.clone(), i.version.clone()))
                .unwrap_or_else(|| ("unknown".into(), "?".into()));
            Ok(AgentInfo {
                name,
                version,
                protocol_version: format!(
                    "v{}",
                    serde_json::to_string(&init.protocol_version).unwrap_or_default()
                ),
                load_session: init.agent_capabilities.load_session,
                mcp_http: init.agent_capabilities.mcp_capabilities.http,
            })
        },
    );
    match tokio::time::timeout(timeout, connect).await {
        Ok(result) => result.map_err(|e| HandshakeError::Acp(describe_error("", &e))),
        Err(_) => Err(HandshakeError::Timeout(timeout)),
    }
}
