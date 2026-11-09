//! Running Chillax instances (ADR 0017): one process per project, listed in a shared
//! registry outside every repository. This module holds the data and the read trait; the
//! file IO and probing live in the CLI.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// One running Chillax process, as listed in the registry and by `/api/v1/instances`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceInfo {
    /// The project's root directory, canonical; the identity of the project.
    pub root: String,
    /// `[project] name`.
    pub name: String,
    /// Where the instance's web UI and API answer, e.g. `http://127.0.0.1:4748`.
    pub url: String,
    /// Process id; for display only (liveness is decided by probing `url`).
    pub pid: u32,
    /// RFC 3339 start time.
    pub started_at: String,
}

/// Where to find the running instances. Implementations return only instances that are
/// alive, sorted by name.
#[async_trait]
pub trait InstanceSource: Send + Sync {
    async fn list(&self) -> Vec<InstanceInfo>;
}
