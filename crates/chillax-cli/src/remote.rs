//! [`Recruiter`] and [`AgentDirectory`] over a running Chillax's `/api/v1`, so
//! `chillax hire` and `chillax settings` work against the live server (drafts, adoption
//! and newly enabled agents happen there, keeping one writer per event log and config).

use async_trait::async_trait;
use chillax_core::{
    AgentDirectory, AgentDirectoryError, BackendInfo, CatalogStatus, ModelOption, PersonaSummary,
    RecruitError, Recruiter, Role, RoleDraft,
};
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

pub struct RemoteRecruiter {
    base: String,
    http: reqwest::Client,
}

impl RemoteRecruiter {
    pub fn new(url: &str) -> Self {
        Self {
            base: format!("{url}/api/v1"),
            http: reqwest::Client::new(),
        }
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<T, RecruitError> {
        let mut request = self.http.request(method, format!("{}{path}", self.base));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let resp = request
            .send()
            .await
            .map_err(|e| RecruitError::Io(e.to_string()))?;
        let status = resp.status();
        if status.is_success() {
            return resp
                .json()
                .await
                .map_err(|e| RecruitError::Io(e.to_string()));
        }
        let message = resp
            .json::<Value>()
            .await
            .ok()
            .and_then(|v| v["error"].as_str().map(str::to_string))
            .unwrap_or_else(|| status.to_string());
        Err(match status {
            StatusCode::SERVICE_UNAVAILABLE => RecruitError::NotReady,
            StatusCode::CONFLICT => RecruitError::NothingToUndo,
            StatusCode::BAD_GATEWAY => RecruitError::Editor(message),
            _ => RecruitError::Io(message),
        })
    }
}

#[async_trait]
impl Recruiter for RemoteRecruiter {
    async fn status(&self) -> CatalogStatus {
        self.call(Method::GET, "/catalog", None)
            .await
            .unwrap_or_default()
    }

    async fn refresh(&self) -> Result<CatalogStatus, RecruitError> {
        self.call(Method::POST, "/catalog/refresh", Some(json!({})))
            .await
    }

    async fn personas(&self) -> Result<Vec<PersonaSummary>, RecruitError> {
        self.call(Method::GET, "/personas?all=true", None).await
    }

    async fn featured(&self, limit: usize) -> Result<Vec<PersonaSummary>, RecruitError> {
        self.call(Method::GET, &format!("/personas?limit={limit}"), None)
            .await
    }

    async fn search(&self, query: &str, limit: usize) -> Result<Vec<PersonaSummary>, RecruitError> {
        let q: String = query
            .bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect();
        self.call(Method::GET, &format!("/personas?q={q}&limit={limit}"), None)
            .await
    }

    async fn draft(
        &self,
        persona: &str,
        base_role: Option<&str>,
        backend: Option<String>,
        model: Option<String>,
    ) -> Result<RoleDraft, RecruitError> {
        let body = json!({
            "persona": persona,
            "base_role": base_role,
            "backend": backend,
            "model": model,
        });
        self.call(Method::POST, "/drafts", Some(body)).await
    }

    async fn draft_from_role(
        &self,
        from_role: &str,
        new_id: &str,
        title: &str,
        backend: Option<String>,
        model: Option<String>,
    ) -> Result<RoleDraft, RecruitError> {
        let body = json!({
            "from_role": from_role,
            "role_id": new_id,
            "title": title,
            "backend": backend,
            "model": model,
        });
        self.call(Method::POST, "/drafts", Some(body)).await
    }

    async fn get_draft(&self, id: &str) -> Result<RoleDraft, RecruitError> {
        self.call(Method::GET, &format!("/drafts/{id}"), None).await
    }

    async fn set_base(&self, id: &str, base_role: &str) -> Result<RoleDraft, RecruitError> {
        let body = json!({"base_role": base_role});
        self.call(Method::POST, &format!("/drafts/{id}/base"), Some(body))
            .await
    }

    async fn revise(&self, id: &str, instruction: &str) -> Result<RoleDraft, RecruitError> {
        let body = json!({"instruction": instruction});
        self.call(Method::POST, &format!("/drafts/{id}/revise"), Some(body))
            .await
    }

    async fn tailor(&self, id: &str) -> Result<RoleDraft, RecruitError> {
        self.call(
            Method::POST,
            &format!("/drafts/{id}/tailor"),
            Some(json!({})),
        )
        .await
    }

    async fn undo(&self, id: &str) -> Result<RoleDraft, RecruitError> {
        self.call(Method::POST, &format!("/drafts/{id}/undo"), Some(json!({})))
            .await
    }

    async fn adopt(
        &self,
        id: &str,
        role_id: Option<String>,
        overwrite: bool,
    ) -> Result<Role, RecruitError> {
        let body = json!({"role_id": role_id, "overwrite": overwrite});
        let reply: Value = self
            .call(Method::POST, &format!("/drafts/{id}/adopt"), Some(body))
            .await?;
        // The API leaves the prompt out of role JSON; callers only need the id and title.
        serde_json::from_value(reply["role"].clone()).map_err(|e| RecruitError::Io(e.to_string()))
    }

    async fn discard(&self, id: &str) {
        let _ = self
            .http
            .delete(format!("{}/drafts/{id}", self.base))
            .send()
            .await;
    }
}

pub struct RemoteAgents {
    recruiter: RemoteRecruiter,
}

impl RemoteAgents {
    pub fn new(url: &str) -> Self {
        Self {
            recruiter: RemoteRecruiter::new(url),
        }
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
    ) -> Result<T, AgentDirectoryError> {
        let body = (method == Method::POST).then(|| json!({}));
        self.recruiter
            .call(method, path, body)
            .await
            .map_err(|err| match err {
                RecruitError::Editor(message) => AgentDirectoryError::Agent(message),
                other => AgentDirectoryError::Config(other.to_string()),
            })
    }
}

#[async_trait]
impl AgentDirectory for RemoteAgents {
    async fn backends(&self) -> Vec<BackendInfo> {
        self.call(Method::GET, "/agents").await.unwrap_or_default()
    }

    async fn models(&self, backend: &str) -> Result<Vec<ModelOption>, AgentDirectoryError> {
        self.call(Method::GET, &format!("/agents/{backend}/models"))
            .await
    }

    async fn efforts(&self, backend: &str) -> Result<Vec<ModelOption>, AgentDirectoryError> {
        self.call(Method::GET, &format!("/agents/{backend}/efforts"))
            .await
    }

    async fn enable(&self, backend: &str) -> Result<BackendInfo, AgentDirectoryError> {
        self.call(Method::POST, &format!("/agents/{backend}/enable"))
            .await
    }
}
