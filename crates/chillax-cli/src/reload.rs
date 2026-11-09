//! Live reload of `.chillax/chillax.toml` and `workflow.yaml`.
//!
//! A light task polls the files' modification time. A change that parses is applied for
//! the fields that can change while running (`Config::with_live_fields_of`); the rest
//! raise one warning and one inbox alert asking for a restart. A file that does not
//! parse changes nothing and raises an alert, so a typo never wipes the running config.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use chillax_core::config::Config;
use chillax_core::workflow::Workflow;
use chillax_core::{Actor, Command, CompanyHandle, Hub};
use tokio::task::JoinHandle;

use crate::project::Project;

/// How often the files are looked at.
const POLL: Duration = Duration::from_secs(2);

type Stamp = Option<(SystemTime, u64)>;

fn stamp(path: &std::path::Path) -> Stamp {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

pub(crate) struct Reloader {
    config_path: PathBuf,
    workflow_path: PathBuf,
    /// The settings in force: the file as loaded, with live changes applied since.
    running: Config,
    workflow: Workflow,
    config_stamp: Stamp,
    workflow_stamp: Stamp,
    /// Restart-only settings already reported, so one edit raises one alert.
    reported: BTreeSet<String>,
    hardware_max: usize,
}

impl Reloader {
    pub(crate) fn new(project: &Project, hardware_max: usize) -> Self {
        let dir = Project::dir(&project.root);
        let config_path = dir.join("chillax.toml");
        let workflow_path = dir.join("workflow.yaml");
        Self {
            config_stamp: stamp(&config_path),
            workflow_stamp: stamp(&workflow_path),
            config_path,
            workflow_path,
            running: project.config.clone(),
            workflow: project.workflow.clone(),
            reported: BTreeSet::new(),
            hardware_max,
        }
    }

    /// Look at the files once; `apply` receives the config to use when live fields changed.
    pub(crate) async fn check(&mut self, hub: &Hub, apply: &(dyn Fn(&Config) + Sync)) {
        let stamp_now = stamp(&self.config_path);
        if stamp_now != self.config_stamp {
            self.config_stamp = stamp_now;
            self.reload_config(hub, apply).await;
        }
        let stamp_now = stamp(&self.workflow_path);
        if stamp_now != self.workflow_stamp {
            self.workflow_stamp = stamp_now;
            self.reload_workflow(hub).await;
        }
    }

    async fn reload_config(&mut self, hub: &Hub, apply: &(dyn Fn(&Config) + Sync)) {
        let parsed = tokio::fs::read_to_string(&self.config_path)
            .await
            .map_err(|e| e.to_string())
            .and_then(|text| Config::from_toml(&text).map_err(|e| e.to_string()));
        let new = match parsed {
            Ok(new) => new,
            Err(error) => {
                tracing::warn!(%error, "chillax.toml changed but cannot be read; keeping the old settings");
                alert(
                    hub,
                    "chillax.toml has an error",
                    format!("The old settings stay in force until it is fixed. {error}"),
                )
                .await;
                return;
            }
        };
        let live = self.running.with_live_fields_of(&new);
        if live != self.running {
            tracing::info!("chillax.toml changed; applying the settings that can change live");
            if live.budget != self.running.budget {
                self.set_agent_limit(hub, live.budget.max_concurrent_agents as usize)
                    .await;
            }
            apply(&live);
            self.running = live;
        }
        let restart: BTreeSet<String> = self
            .running
            .restart_needed(&new)
            .into_iter()
            .map(String::from)
            .collect();
        let fresh: Vec<&String> = restart.difference(&self.reported).collect();
        if !fresh.is_empty() {
            let names = fresh
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            tracing::warn!(settings = %names, "chillax.toml changed settings that need a restart");
            alert(
                hub,
                "Restart Chillax to apply config changes",
                format!("These settings only take effect at startup: {names}."),
            )
            .await;
        }
        self.reported = restart;
    }

    async fn set_agent_limit(&self, hub: &Hub, wanted: usize) {
        let hardware_max = hub.engine().await.hardware_max.unwrap_or(self.hardware_max);
        let n = wanted.clamp(1, hardware_max.max(1));
        if n != wanted {
            tracing::warn!(
                wanted,
                hardware_max,
                "max_concurrent_agents exceeds what this machine fits; using the maximum"
            );
        }
        if hub.snapshot().await.agent_limit.map(|a| a.limit) == Some(n) {
            return;
        }
        if let Err(error) = hub
            .send(Actor::Stakeholder, Command::SetMaxConcurrentAgents { n })
            .await
        {
            tracing::warn!(%error, "could not apply the new agent limit");
        }
    }

    async fn reload_workflow(&mut self, hub: &Hub) {
        let parsed = match tokio::fs::read_to_string(&self.workflow_path).await {
            Ok(text) => Workflow::from_yaml(&text).map_err(|e| e.to_string()),
            // A deleted file falls back to the default, as at startup.
            Err(_) => Ok(Workflow::default()),
        };
        match parsed {
            Ok(workflow) if workflow == self.workflow => {}
            Ok(workflow) => {
                tracing::warn!("workflow.yaml changed; it applies after a restart");
                alert(
                    hub,
                    "Restart Chillax to apply config changes",
                    "workflow.yaml only takes effect at startup.".into(),
                )
                .await;
                self.workflow = workflow;
            }
            Err(error) => {
                tracing::warn!(%error, "workflow.yaml changed but cannot be read");
                alert(
                    hub,
                    "workflow.yaml has an error",
                    format!("The old workflow stays in force until it is fixed. {error}"),
                )
                .await;
            }
        }
    }
}

async fn alert(hub: &Hub, title: &str, detail: String) {
    let cmd = Command::RaiseAlert {
        title: title.into(),
        detail,
    };
    if let Err(error) = hub.send(Actor::System, cmd).await {
        tracing::warn!(%error, "could not raise a config alert");
    }
}

/// Poll the config files for the life of the process, handing live changes to `apply`.
pub(crate) fn start(
    project: &Project,
    hub: Arc<Hub>,
    hardware_max: usize,
    apply: impl Fn(&Config) + Send + Sync + 'static,
) -> JoinHandle<()> {
    let mut reloader = Reloader::new(project, hardware_max);
    chillax_core::task::spawn_logged("config-reload", async move {
        let mut tick = tokio::time::interval(POLL);
        loop {
            tick.tick().await;
            reloader.check(&hub, &apply).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use chillax_core::CompanyHandle;
    use chillax_core::engine::Engine;
    use chillax_core::{MemoryLog, RoleCatalog};

    use super::*;

    fn write(dir: &std::path::Path, name: &str, text: &str) {
        std::fs::write(dir.join(".chillax").join(name), text).expect("write");
    }

    struct Rig {
        dir: tempfile::TempDir,
        hub: Arc<Hub>,
        reloader: Reloader,
        applied: Arc<Mutex<Vec<Config>>>,
    }

    impl Rig {
        async fn new(toml: &str) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(dir.path().join(".chillax")).expect("dir");
            write(dir.path(), "chillax.toml", toml);
            let project = Project::load(dir.path()).expect("loads");
            let engine = Engine::new("Acme", RoleCatalog::default(), Workflow::default());
            let hub = Arc::new(
                Hub::open(engine, Arc::new(MemoryLog::default()))
                    .await
                    .expect("hub"),
            );
            Self {
                reloader: Reloader::new(&project, 8),
                dir,
                hub,
                applied: Arc::default(),
            }
        }

        /// Edit the file and make sure its stamp differs (mtime can be coarse).
        async fn edit(&mut self, toml: &str) {
            write(
                self.dir.path(),
                "chillax.toml",
                &format!("{toml}\n# {}\n", rand_tag()),
            );
            self.check().await;
        }

        async fn check(&mut self) {
            let applied = self.applied.clone();
            self.reloader
                .check(&self.hub, &move |c| {
                    applied.lock().expect("lock").push(c.clone());
                })
                .await;
        }

        async fn alerts(&self) -> Vec<String> {
            self.hub
                .snapshot()
                .await
                .inbox
                .iter()
                .filter_map(|i| match i {
                    chillax_core::InboxItem::Alert { title, .. } => Some(title.clone()),
                    _ => None,
                })
                .collect()
        }
    }

    fn rand_tag() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    }

    const BASE: &str = "[project]\nname = \"x\"\n";

    #[tokio::test]
    async fn new_worktree_mode_and_env_steps_are_applied_live() {
        let mut rig = Rig::new(BASE).await;
        rig.edit(&format!(
            "{BASE}[vcs]\nworktrees = \"per-ticket\"\n[env.qa]\nup = \"migrate\"\n"
        ))
        .await;
        {
            let applied = rig.applied.lock().expect("lock");
            let [config] = applied.as_slice() else {
                panic!("expected one reload, got {}", applied.len());
            };
            assert_eq!(
                config.vcs.worktrees,
                chillax_core::config::WorktreeMode::PerTicket
            );
            assert_eq!(config.env.qa.up.as_deref(), Some("migrate"));
        }
        assert!(rig.alerts().await.is_empty());
    }

    #[tokio::test]
    async fn an_invalid_file_keeps_the_old_config_and_alerts() {
        let mut rig = Rig::new(BASE).await;
        rig.edit("[project\nnot toml").await;
        assert!(rig.applied.lock().expect("lock").is_empty());
        assert_eq!(rig.alerts().await, ["chillax.toml has an error"]);
        // Fixing it applies the change on top of the unchanged running config.
        rig.edit(&format!("{BASE}[env.qa]\nup = \"x\"\n")).await;
        assert_eq!(rig.applied.lock().expect("lock").len(), 1);
    }

    #[tokio::test]
    async fn restart_only_changes_alert_once_and_are_not_applied() {
        let mut rig = Rig::new(BASE).await;
        rig.edit(&format!("{BASE}[server]\nport = 9999\n")).await;
        assert!(rig.applied.lock().expect("lock").is_empty());
        assert_eq!(rig.alerts().await.len(), 1);
        // An unrelated live edit does not repeat the alert.
        rig.edit(&format!(
            "{BASE}[server]\nport = 9999\n[env.qa]\nup = \"x\"\n"
        ))
        .await;
        assert_eq!(rig.applied.lock().expect("lock").len(), 1);
        assert_eq!(rig.alerts().await.len(), 1);
        let applied = rig.applied.lock().expect("lock");
        assert_ne!(applied[0].server.port, 9999);
    }
}
