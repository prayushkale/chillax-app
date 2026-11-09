//! [`AgentDirectory`] over ACP: which backends the stakeholder can hire on and which
//! models each offers. Models are read from a session's config options, so probing
//! starts the agent and opens a session but never sends a prompt (no cost).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{InitializeRequest, NewSessionRequest};
use agent_client_protocol::{Agent, Client, ConnectionTo};
use async_trait::async_trait;
use chillax_core::agents::{backend_title, known_presets};
use chillax_core::config::AgentConfig;
use chillax_core::{AgentDirectory, AgentDirectoryError, BackendInfo, ModelOption};

use crate::SharedBackends;
use crate::acp::{describe_error, effort_options, model_options};
use crate::session::agent_process;

/// Saves a newly enabled backend to the project config (`[agents.<name>]`).
pub type ConfigWriter = Arc<dyn Fn(&str, &AgentConfig) -> Result<(), String> + Send + Sync>;

pub struct AcpAgentDirectory {
    backends: SharedBackends,
    /// Parent of the per-probe empty working directories.
    scratch: PathBuf,
    writer: ConfigWriter,
    probes: Mutex<HashMap<String, Probe>>,
    timeout: Duration,
}

impl AcpAgentDirectory {
    pub fn new(backends: SharedBackends, scratch: PathBuf, writer: ConfigWriter) -> Self {
        Self {
            backends,
            scratch,
            writer,
            probes: Mutex::default(),
            timeout: Duration::from_secs(90),
        }
    }

    /// What a backend offers, from one probe run that is cached per backend.
    async fn probe(&self, backend: &str) -> Result<Probe, AgentDirectoryError> {
        if let Some(probe) = self
            .probes
            .lock()
            .expect("probes lock poisoned")
            .get(backend)
        {
            return Ok(probe.clone());
        }
        let config = self
            .config(backend)
            .ok_or_else(|| AgentDirectoryError::Unknown(backend.to_string()))?;
        let cwd = self.scratch.join(format!("probe-{}", crate::nonce()));
        tokio::fs::create_dir_all(&cwd)
            .await
            .map_err(|e| AgentDirectoryError::Agent(format!("creating {}: {e}", cwd.display())))?;
        let probe = tokio::time::timeout(self.timeout, probe_agent(backend, &config, &cwd)).await;
        let _ = tokio::fs::remove_dir_all(&cwd).await;
        let probe = probe.unwrap_or_else(|_| {
            Err(format!(
                "the agent did not start within {}s",
                self.timeout.as_secs()
            ))
        });
        let probe = probe.map_err(AgentDirectoryError::Agent)?;
        self.probes
            .lock()
            .expect("probes lock poisoned")
            .insert(backend.to_string(), probe.clone());
        Ok(probe)
    }

    fn config(&self, name: &str) -> Option<AgentConfig> {
        self.backends
            .read()
            .expect("backends lock poisoned")
            .get(name)
            .cloned()
    }
}

#[async_trait]
impl AgentDirectory for AcpAgentDirectory {
    async fn backends(&self) -> Vec<BackendInfo> {
        let configured = self
            .backends
            .read()
            .expect("backends lock poisoned")
            .clone();
        let mut out: Vec<BackendInfo> = configured
            .iter()
            .map(|(name, config)| BackendInfo {
                name: name.clone(),
                title: backend_title(name),
                command: config.command.clone(),
                configured: true,
                available: on_path(&config.command),
            })
            .collect();
        out.extend(
            known_presets()
                .into_iter()
                .filter(|(name, _, config)| {
                    !configured.contains_key(*name) && on_path(&config.command)
                })
                .map(|(name, title, config)| BackendInfo {
                    name: name.into(),
                    title: title.into(),
                    command: config.command,
                    configured: false,
                    available: true,
                }),
        );
        out
    }

    async fn models(&self, backend: &str) -> Result<Vec<ModelOption>, AgentDirectoryError> {
        Ok(self.probe(backend).await?.models)
    }

    async fn efforts(&self, backend: &str) -> Result<Vec<ModelOption>, AgentDirectoryError> {
        Ok(self.probe(backend).await?.efforts)
    }

    async fn enable(&self, backend: &str) -> Result<BackendInfo, AgentDirectoryError> {
        let info = |config: &AgentConfig, configured: bool| BackendInfo {
            name: backend.to_string(),
            title: backend_title(backend),
            command: config.command.clone(),
            configured,
            available: on_path(&config.command),
        };
        if let Some(config) = self.config(backend) {
            return Ok(info(&config, true));
        }
        let (_, _, config) = known_presets()
            .into_iter()
            .find(|(name, ..)| *name == backend)
            .ok_or_else(|| AgentDirectoryError::Unknown(backend.to_string()))?;
        (self.writer)(backend, &config).map_err(AgentDirectoryError::Config)?;
        self.backends
            .write()
            .expect("backends lock poisoned")
            .insert(backend.to_string(), config.clone());
        Ok(info(&config, true))
    }
}

/// The models and effort levels an agent offers, read from one session.
#[derive(Clone, Default)]
struct Probe {
    models: Vec<ModelOption>,
    /// For the agent's default model: levels may differ for other models.
    efforts: Vec<ModelOption>,
}

async fn probe_agent(backend: &str, config: &AgentConfig, cwd: &Path) -> Result<Probe, String> {
    let cwd = cwd.to_path_buf();
    Client
        .builder()
        .name("chillax-models")
        .connect_with(
            agent_process(config),
            async move |cx: ConnectionTo<Agent>| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let session = cx
                    .send_request(NewSessionRequest::new(cwd))
                    .block_task()
                    .await?;
                let options = session.config_options.as_deref();
                Ok(Probe {
                    models: options
                        .and_then(model_options)
                        .map(|(_, models)| models)
                        .unwrap_or_default(),
                    efforts: options
                        .and_then(effort_options)
                        .map(|(_, efforts)| efforts)
                        .unwrap_or_default(),
                })
            },
        )
        .await
        .map_err(|err| describe_error(backend, &err))
}

/// Whether `command` resolves to a file, directly or through `PATH`.
fn on_path(command: &str) -> bool {
    let path = Path::new(command);
    if path.components().count() > 1 {
        return path.is_file();
    }
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| {
            let candidate = dir.join(command);
            candidate.is_file() || (cfg!(windows) && candidate.with_extension("exe").is_file())
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_commands_on_path() {
        assert!(on_path("sh"));
        assert!(!on_path("chillax-no-such-agent"));
        assert!(!on_path("/no/such/agent"));
    }
}
