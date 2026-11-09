//! Wiring: event store + hub + agent runtime + MCP + web server.

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chillax_agents::{
    AcpAgentDirectory, AcpPromptEditor, AgentRuntime, EnvFiles, McpEndpoint, RuntimeConfig,
    SharedBackends, shared_backends,
};
use chillax_catalog::{CatalogPaths, CatalogRecruiter};
use chillax_core::AgentLimit;
use chillax_core::config::{TrackerConfig, VcsKind};
use chillax_core::vcs::VcsHost;
use chillax_core::{
    Actor, AgentDirectory, AttachmentStore, Command, CompanyHandle, Engine, EventKind, Hub,
    InstanceInfo, Recruiter,
};
use chillax_dispatch::{AppSupervisor, DispatchConfig, Dispatcher};
use chillax_mcp::{AttachmentAccess, ChillaxMcp, McpAccess};
use chillax_server::{ServerInfo, ServerOptions, Theme};
use chillax_store::{FileAttachmentStore, SqliteLog};
use chillax_tracker::{LocalTracker, Tracker};
use chillax_vcs::{GithubGit, LocalGit};

use crate::instance::{self, BASE_PORT, Registry};
use crate::project::Project;

/// Reject tracker kinds other than the built-in one.
pub(crate) fn check_tracker_kind(tracker: &TrackerConfig) -> Result<()> {
    match tracker.kind.as_str() {
        "local" => Ok(()),
        other => bail!("unknown tracker kind \"{other}\"; only \"local\" is supported"),
    }
}

pub struct Company {
    pub hub: Arc<Hub>,
    pub url: String,
    /// The persona catalog; `None` when `[catalog] enabled = false`.
    pub recruiter: Option<Arc<dyn Recruiter>>,
    /// Agent backends and models for hiring and Settings.
    pub agents: Arc<dyn AgentDirectory>,
    /// Image attachments (`.chillax/state/attachments`).
    pub attachments: Arc<dyn AttachmentStore>,
    /// The dev, QA and prod apps Chillax keeps running (ADR 0015).
    pub apps: Arc<AppSupervisor>,
    runtime: Arc<AgentRuntime>,
    dispatcher: tokio::task::JoinHandle<()>,
    /// Finds apps listening in the repository or its worktrees.
    detector: tokio::task::JoinHandle<()>,
    /// Writes the stakeholder's agent limit back to `chillax.toml`.
    limit_saver: tokio::task::JoinHandle<()>,
    config_reload: tokio::task::JoinHandle<()>,
    /// Reports to herdr when Chillax runs in one of its panes (ADR 0018).
    herdr: Option<crate::herdr::HerdrHandle>,
    /// Our registry entry, removed on shutdown.
    registration: Option<(Registry, InstanceInfo)>,
}

/// Hire `[team] default_hires` into a project whose event log is still empty, so a new
/// project starts with a Project Manager. A later restart, or a PM who was let go, leaves
/// events behind, so nobody is hired twice. Failures only warn: startup never depends on it.
async fn seed_default_team(project: &Project, hub: &Hub) {
    if hub.snapshot().await.last_seq != 0 {
        return;
    }
    for id in &project.config.team.default_hires {
        let Some(role) = project.roles.get(id) else {
            tracing::warn!(role = %id, "unknown role in [team] default_hires; skipped");
            continue;
        };
        let hire = Command::HireEmployee {
            role: role.id.clone(),
            name: None,
            backend: None,
            model: None,
            effort: None,
            keep_defaults: false,
        };
        if let Err(error) = hub.send(Actor::Stakeholder, hire).await {
            tracing::warn!(role = %id, %error, "could not hire default team member");
        }
    }
}

/// The limit to run with and the event that records it, when state differs. The config
/// is the source of truth at boot, clamped to what the hardware fits.
fn reconcile_limit(
    configured: usize,
    hardware_max: usize,
    current: Option<AgentLimit>,
) -> (usize, Option<EventKind>) {
    let n = configured.min(hardware_max).max(1);
    if configured > hardware_max {
        tracing::warn!(
            configured,
            hardware_max,
            "max_concurrent_agents exceeds what this machine fits; using the maximum"
        );
    }
    let wanted = AgentLimit {
        limit: n,
        hardware_max,
    };
    let event = (current != Some(wanted))
        .then_some(EventKind::MaxConcurrentAgentsChanged { n, hardware_max });
    (n, event)
}

/// Write the limit the stakeholder sets back to `chillax.toml`, so it survives a restart.
/// Only the stakeholder's changes are written: the start-up event comes from the file.
fn persist_limit_changes(project: &Project, hub: &Hub) -> tokio::task::JoinHandle<()> {
    let mut rx = hub.subscribe();
    let path = Project::dir(&project.root).join("chillax.toml");
    chillax_core::task::spawn_logged("persist-agent-limit", async move {
        loop {
            let event = match rx.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            let EventKind::MaxConcurrentAgentsChanged { n, .. } = event.kind else {
                continue;
            };
            if event.actor != Actor::Stakeholder {
                continue;
            }
            let path = path.clone();
            match tokio::task::spawn_blocking(move || crate::config_write::set_budget_max(&path, n))
                .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::warn!(%error, "could not save the agent limit to chillax.toml")
                }
                Err(error) => tracing::warn!(%error, "saving the agent limit failed"),
            }
        }
    })
}

impl Company {
    /// Open the event store and rebuild state. No agents or server yet.
    pub async fn open(project: &Project) -> Result<Arc<Hub>> {
        let state_dir = project.state_dir();
        std::fs::create_dir_all(&state_dir)?;
        let log = SqliteLog::open(&state_dir.join("chillax.db"))
            .await
            .context("opening .chillax/state/chillax.db")?;
        let engine = Engine::new(
            project.config.project.name.clone(),
            project.roles.clone(),
            project.workflow.clone(),
        )
        .with_team(project.config.team.clone())
        .with_hardware_max(crate::hardware::hardware_max());
        Ok(Arc::new(Hub::open(engine, Arc::new(log)).await?))
    }

    /// Open the company and start everything that runs in the background.
    pub async fn start(project: &Project) -> Result<Self> {
        Self::start_with(project, Registry::from_env(), BASE_PORT).await
    }

    /// [`Self::start`] with the instance registry and the base port given (tests).
    pub async fn start_with(project: &Project, registry: Registry, base_port: u16) -> Result<Self> {
        let problems = project.config.app_problems();
        if !problems.is_empty() {
            bail!(
                "invalid app settings in .chillax/chillax.toml:\n  {}",
                problems.join("\n  ")
            );
        }
        let server = &project.config.server;
        let host = display_host(&server.bind);
        // An explicit port is used as given; otherwise the configured (default) port is
        // the first one tried (ADR 0017).
        let first_port = if server.port_explicit {
            server.port
        } else {
            base_port
        };
        let configured = format!("http://{host}:{first_port}");
        if let Some(running) = instance::find(&registry, &project.root, &configured).await {
            bail!("chillax is already running for this project at {running}");
        }
        let listener = instance::bind_server(
            &server.bind,
            first_port,
            server.port_explicit,
            &project.root,
            host,
        )
        .await?;
        let port = listener.local_addr()?.port();
        let url = format!("http://{host}:{port}");

        let hub = Self::open(project).await?;
        let company: Arc<dyn CompanyHandle> = hub.clone();
        check_tracker_kind(&project.config.tracker)?;
        seed_default_team(project, &hub).await;
        let tracker: Arc<dyn Tracker> = Arc::new(LocalTracker::new(company.clone()));

        let access = Arc::new(McpAccess::default());
        let attachments: Arc<dyn AttachmentStore> = Arc::new(FileAttachmentStore::new(
            project.state_dir().join("attachments"),
        ));
        // Agents may attach files from ticket worktrees and QA scratch folders only.
        let attach_roots = vec![
            Project::dir(&project.root).join("worktrees"),
            project.state_dir().join("qa"),
        ];
        let mcp = ChillaxMcp::new(
            company.clone(),
            tracker,
            Some(AttachmentAccess {
                store: attachments.clone(),
                roots: attach_roots,
            }),
            access.clone(),
        );
        let mcp_router = axum::Router::new().nest_service("/mcp", mcp.http_service());

        let backends = shared_backends(project.config.agents.backends.clone());
        let env_files = EnvFiles::new(project.root.clone(), &project.config.env);
        let agents = directory(project, backends.clone());
        // The stakeholder can change the limit while running; every reader shares this.
        let snapshot = hub.snapshot().await;
        let hardware_max = hub
            .engine()
            .await
            .hardware_max
            .unwrap_or_else(crate::hardware::hardware_max);
        let (limit, change) = reconcile_limit(
            project.config.budget.max_concurrent_agents as usize,
            hardware_max,
            snapshot.agent_limit,
        );
        if let Some(kind) = change
            && let Err(error) = hub.record(Actor::System, kind).await
        {
            tracing::warn!(%error, "could not record the agent limit");
        }
        let live_limit = Arc::new(AtomicUsize::new(limit));
        let limit_saver = persist_limit_changes(project, &hub);
        let runtime = AgentRuntime::new(
            hub.clone(),
            RuntimeConfig {
                company: project.config.project.name.clone(),
                cwd: project.root.clone(),
                backends: backends.clone(),
                mcp: Some(McpEndpoint {
                    url: format!("{url}/mcp"),
                    issue_token: Arc::new(move |employee| access.issue(employee)),
                }),
                max_concurrent: limit,
                max_concurrent_live: Some(live_limit.clone()),
                permissions: project.config.agents.permissions,
                pricing: Arc::new(chillax_core::PriceTable::new(&project.config.pricing)),
                chat_idle: std::time::Duration::from_secs(
                    project.config.agents.chat_idle_minutes.max(1) * 60,
                ),
                env_files: env_files.clone(),
                ..RuntimeConfig::default()
            },
        );
        // No agent process survives a restart, so anything that still looks alive in the
        // log is left over from a run that quit without closing its sessions.
        for employee in hub.snapshot().await.stale_agents() {
            tracing::info!(%employee, "resetting stale agent status");
            if let Err(error) = hub
                .record(Actor::System, EventKind::AgentStopped { employee })
                .await
            {
                tracing::warn!(%error, "could not reset stale agent status");
            }
        }
        runtime.start();
        // The dev loop picks up unfinished tickets straight away, so a restart resumes
        // work; `chillax pause` stops it.
        let dev = &project.config.env.dev;
        let mut dispatch = DispatchConfig::new(project.root.clone(), dev.base_branch.clone());
        dispatch.test = dev.test.clone();
        dispatch.test_timeout = Duration::from_secs(dev.test_timeout_secs);
        dispatch.qa = project.config.env.qa.clone();
        dispatch.prod = project.config.env.prod.clone();
        dispatch.max_concurrent = limit;
        dispatch.max_concurrent_live = Some(live_limit);
        dispatch.worktrees = project.config.vcs.worktrees;
        dispatch.sync_after_deploy = project.config.release.sync_after_deploy;
        dispatch.apps = project.config.env.apps();
        dispatch.env_files = env_files;
        dispatch.checkin = Duration::from_secs(u64::from(project.config.team.checkin_minutes) * 60);
        let team = &project.config.team;
        dispatch.stall = Duration::from_secs(u64::from(team.stall_minutes) * 60);
        dispatch.problem_grace = Duration::from_secs(u64::from(team.problem_grace_minutes) * 60);
        dispatch.problem_max_attempts = team.problem_max_attempts;
        dispatch.problem_retry = Duration::from_secs(u64::from(team.problem_retry_minutes) * 60);
        let dispatcher = Dispatcher::new(hub.clone(), runtime.clone(), vcs_host(project), dispatch);
        let apps = dispatcher.apps();
        let dispatcher_task = dispatcher.start();
        let config_reload = {
            let dispatcher = dispatcher.clone();
            // Branches, ports and the like stay as started; the reloader warns about them.
            crate::reload::start(project, hub.clone(), hardware_max, move |config| {
                dispatcher.reload(chillax_dispatch::LiveSettings::from_project(config))
            })
        };
        let detector = chillax_dispatch::AppDetector::new(
            hub.clone(),
            Arc::new(chillax_dispatch::LsofProbe),
            project.root.clone(),
            vec![port],
        )
        .start(Duration::from_secs(10));
        let herdr = crate::herdr::start(
            &project.config.herdr,
            &project.config.project.name,
            hub.clone(),
        );

        let theme = match project.config.server.theme.as_str() {
            "default" => Theme::embedded(),
            name => {
                let dir = Project::dir(&project.root).join("themes").join(name);
                if !dir.join("index.html").is_file() {
                    bail!("theme `{name}` not found at {}", dir.display());
                }
                Theme::directory(dir)
            }
        };
        let recruiter = recruiter(project, company.clone(), backends).await;
        if project.config.catalog.enabled && project.config.catalog.refresh_on_start {
            let r = &recruiter;
            // Background: the TUI and server are usable on the cached catalog meanwhile.
            let r = Arc::clone(r);
            tokio::spawn(async move {
                // Failures are logged by the recruiter; the cache stays in use.
                if let Ok(status) = r.refresh().await {
                    tracing::info!(
                        catalog = %status.name,
                        personas = status.personas,
                        commit = status.commit.as_deref().unwrap_or(""),
                        "persona catalog refreshed"
                    );
                }
            });
        }
        let recruiter: Option<Arc<dyn Recruiter>> = Some(recruiter);
        let root = instance::canonical(&project.root);
        let registry_shared = Arc::new(registry.clone());
        let app = chillax_server::router_with(
            company,
            recruiter.clone(),
            Some(agents.clone()),
            Some(attachments.clone()),
            Some(mcp_router),
            theme,
            ServerOptions::default()
                .with_info(ServerInfo {
                    name: project.config.project.name.clone(),
                    root: root.clone(),
                })
                .with_instances(registry_shared),
        );
        chillax_core::task::spawn_logged_result("web-server", async move {
            axum::serve(listener, app).await
        });
        let info = InstanceInfo {
            root,
            name: project.config.project.name.clone(),
            url: url.clone(),
            pid: std::process::id(),
            started_at: chrono::Utc::now().to_rfc3339(),
        };
        let registration = match registry.register(&info) {
            Ok(()) => Some((registry, info)),
            Err(error) => {
                tracing::warn!(%error, "could not register in the instance registry");
                if port != server.port {
                    tracing::warn!(
                        %url,
                        "using port {port} instead of {}, and other commands cannot find this \
                         instance through the registry; they look at {configured}",
                        server.port
                    );
                }
                None
            }
        };
        tracing::info!(%url, "chillax started");
        Ok(Self {
            hub,
            url,
            recruiter,
            agents,
            attachments,
            apps,
            runtime,
            dispatcher: dispatcher_task,
            detector,
            limit_saver,
            config_reload,
            herdr,
            registration,
        })
    }

    /// Stop the dev loop, close every agent session (which kills the agent processes and
    /// records them as stopped) and end the apps Chillax started. Call before the process
    /// exits.
    pub async fn shutdown(&self) {
        if let Some((registry, info)) = &self.registration {
            registry.unregister(info);
        }
        self.dispatcher.abort();
        self.detector.abort();
        self.limit_saver.abort();
        self.config_reload.abort();
        if let Some(herdr) = &self.herdr {
            herdr.shutdown().await;
        }
        self.runtime.shutdown_all(AGENT_STOP_TIMEOUT).await;
        self.apps.shutdown().await;
        // Whatever agents and QA steps left running: SIGTERM, then SIGKILL after 5s.
        chillax_agents::procgroup::terminate_all(AGENT_STOP_TIMEOUT).await;
    }
}

/// How long quitting waits for agent processes to die.
const AGENT_STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// Make sure agents and apps die with Chillax even when it does not exit normally: a
/// panic (apps only, synchronously), or SIGTERM / SIGHUP (a closed terminal), which
/// run a bounded [`Company::shutdown`]. A second signal exits at once. `restore` puts the
/// terminal back first.
pub fn end_apps_on_abort(company: Arc<Company>, restore: fn()) {
    let previous = std::panic::take_hook();
    let entry = company
        .registration
        .as_ref()
        .and_then(|(registry, info)| registry.path_for(&info.root));
    std::panic::set_hook(Box::new(move |info| {
        chillax_dispatch::kill_all_processes(Duration::from_millis(300));
        if let Some(path) = &entry {
            let _ = std::fs::remove_file(path);
        }
        previous(info);
    }));
    #[cfg(unix)]
    tokio::spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut term), Ok(mut hup)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::hangup()),
        ) else {
            return;
        };
        let code = tokio::select! {
            _ = term.recv() => 143,
            _ = hup.recv() => 129,
        };
        restore();
        tracing::info!(code, "chillax stopped by a signal");
        let stop = tokio::time::timeout(SIGNAL_SHUTDOWN_TIMEOUT, company.shutdown());
        tokio::select! {
            result = stop => {
                if result.is_err() {
                    tracing::warn!("shutdown timed out; killing the apps");
                }
            }
            // A second signal means "now".
            _ = term.recv() => {}
            _ = hup.recv() => {}
        }
        chillax_dispatch::kill_all_processes(Duration::from_secs(1));
        std::process::exit(code);
    });
    #[cfg(not(unix))]
    let _ = (company, restore);
}

/// Covers the agent wait, the apps' own stop and the final sweep of process groups.
const SIGNAL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(12);

/// The version-control adapter `[vcs]` asks for. A GitHub project without the remote it
/// names falls back to local git, so Chillax still works; `chillax doctor` says why.
fn vcs_host(project: &Project) -> Arc<dyn VcsHost> {
    let base = project.config.env.dev.base_branch.clone();
    let prod = project.config.env.prod.branch.clone();
    let shared = project.config.env.files.clone();
    let vcs = &project.config.vcs;
    if vcs.kind == VcsKind::Github {
        if crate::project::remote_url(&project.root, &vcs.remote).is_some() {
            return Arc::new(
                GithubGit::new(project.root.clone(), base, vcs.remote.clone())
                    .with_prod(prod)
                    .with_shared_files(shared),
            );
        }
        tracing::warn!(
            remote = %vcs.remote,
            "[vcs] kind is github but the repository has no such remote; using local git"
        );
    }
    Arc::new(
        LocalGit::new(project.root.clone(), base)
            .with_prod(prod)
            .with_shared_files(shared),
    )
}

/// The persona catalog over the project's cache, with whatever is cached already
/// indexed. No network here; callers decide when to refresh. Always built: with the
/// catalog disabled, only catalog calls fail (`Disabled`); drafting from a role works.
pub async fn recruiter(
    project: &Project,
    company: Arc<dyn CompanyHandle>,
    backends: SharedBackends,
) -> Arc<CatalogRecruiter> {
    let config = &project.config.catalog;
    let state = project.state_dir();
    let editor = AcpPromptEditor::new(backends, state.join("editor"));
    let recruiter = CatalogRecruiter::new(
        config.clone(),
        CatalogPaths {
            checkout: state
                .join("catalogs")
                .join(chillax_core::engine::slug(&config.name)),
            roles: Project::dir(&project.root).join("roles"),
            project: project.root.clone(),
        },
        company,
        Arc::new(editor),
    );
    if let Err(err) = recruiter.load_cached().await {
        tracing::warn!(%err, "could not read the cached persona catalog");
    }
    recruiter
}

/// The agent backends over `backends`; enabling a preset appends it to the
/// project's `chillax.toml`.
pub fn directory(project: &Project, backends: SharedBackends) -> Arc<dyn AgentDirectory> {
    let root = project.root.clone();
    Arc::new(AcpAgentDirectory::new(
        backends,
        project.state_dir().join("probe"),
        Arc::new(move |name, agent| {
            crate::project::append_agent(&root, name, agent).map_err(|e| format!("{e:#}"))
        }),
    ))
}

fn display_host(bind: &str) -> &str {
    match bind {
        "0.0.0.0" | "::" => "127.0.0.1",
        other => other,
    }
}

/// The URL of this project's running instance, if any: its registry entry, else the
/// configured address, either only when it answers as this project (ADR 0017).
pub async fn live_url(project: &Project) -> Option<String> {
    instance::find(&Registry::from_env(), &project.root, &server_url(project)).await
}

pub fn server_url(project: &Project) -> String {
    let server = &project.config.server;
    format!("http://{}:{}", display_host(&server.bind), server.port)
}

#[cfg(test)]
mod tests {

    use super::*;

    /// Two projects started with one registry and the same base port get different
    /// ports, both show up in the registry, and quitting removes the entry.
    #[tokio::test]
    async fn two_projects_share_a_registry_on_different_ports() {
        let home = tempfile::tempdir().expect("home");
        let registry = Registry::at(home.path().join("instances"));
        let base = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("probe port");
        let base_port = base.local_addr().expect("addr").port();
        drop(base);

        let mut projects = Vec::new();
        for name in ["alpha", "beta"] {
            let dir = tempfile::tempdir().expect("tempdir");
            let dot = dir.path().join(".chillax");
            std::fs::create_dir_all(dot.join("state")).expect("state dir");
            std::fs::write(
                dot.join("chillax.toml"),
                format!("[project]\nname = \"{name}\"\n[catalog]\nenabled = false\n"),
            )
            .expect("config");
            projects.push((dir, name));
        }
        let mut companies = Vec::new();
        for (dir, _) in &projects {
            let project = Project::load(dir.path()).expect("loads");
            companies.push(
                Company::start_with(&project, registry.clone(), base_port)
                    .await
                    .expect("starts"),
            );
        }
        assert_ne!(companies[0].url, companies[1].url);
        let listed = chillax_core::InstanceSource::list(&registry).await;
        let names: Vec<_> = listed.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["alpha", "beta"]);

        // Starting the same project again is refused, naming where it runs.
        let again = Project::load(projects[0].0.path()).expect("loads");
        let err = Company::start_with(&again, registry.clone(), base_port)
            .await
            .err()
            .expect("already running");
        assert!(format!("{err:#}").contains(&companies[0].url), "{err:#}");

        companies[0].shutdown().await;
        let left = chillax_core::InstanceSource::list(&registry).await;
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].name, "beta");
        companies[1].shutdown().await;
        assert!(
            chillax_core::InstanceSource::list(&registry)
                .await
                .is_empty()
        );
    }

    /// Start a project from `toml`, return how many employees the log holds after each
    /// of the given steps (`true` = let the first employee go before restarting).
    async fn employees_after_starts(toml: &str, let_go_between: bool) -> (usize, usize) {
        let home = tempfile::tempdir().expect("home");
        let registry = Registry::at(home.path().join("instances"));
        let dir = tempfile::tempdir().expect("tempdir");
        let dot = dir.path().join(".chillax");
        std::fs::create_dir_all(dot.join("state")).expect("state dir");
        std::fs::write(dot.join("chillax.toml"), toml).expect("config");
        let project = Project::load(dir.path()).expect("loads");
        let first = Company::start_with(&project, registry.clone(), 0)
            .await
            .expect("starts");
        let before = first.hub.snapshot().await.employees;
        if let_go_between && let Some(e) = before.first() {
            let cmd = Command::LetGo {
                employee: e.employee.id.clone(),
            };
            first
                .hub
                .send(Actor::Stakeholder, cmd)
                .await
                .expect("let go");
        }
        let n = before.len();
        first.shutdown().await;
        let second = Company::start_with(&project, registry, 0)
            .await
            .expect("restarts");
        let after = second.hub.snapshot().await.employees;
        second.shutdown().await;
        (n, after.len())
    }

    const BARE: &str = "[project]\nname = \"t\"\n[catalog]\nenabled = false\n";

    #[tokio::test]
    async fn new_project_hires_a_project_manager_once() {
        assert_eq!(employees_after_starts(BARE, false).await, (1, 1));
    }

    #[tokio::test]
    async fn a_project_manager_who_was_let_go_is_not_rehired() {
        let (_, after) = employees_after_starts(BARE, true).await;
        assert_eq!(after, 0);
    }

    #[tokio::test]
    async fn default_hires_can_be_empty_or_unknown() {
        let empty = format!("{BARE}[team]\ndefault_hires = []\n");
        assert_eq!(employees_after_starts(&empty, false).await, (0, 0));
        let unknown = format!("{BARE}[team]\ndefault_hires = [\"nope\"]\n");
        assert_eq!(employees_after_starts(&unknown, false).await, (0, 0));
    }

    #[test]
    fn tracker_kind_errors_name_the_fix() {
        let tracker = |kind: &str| TrackerConfig { kind: kind.into() };
        assert!(check_tracker_kind(&tracker("local")).is_ok());
        let other = check_tracker_kind(&tracker("jira")).expect_err("unknown");
        assert_eq!(
            other.to_string(),
            "unknown tracker kind \"jira\"; only \"local\" is supported"
        );
    }
}

#[cfg(test)]
mod limit_tests {
    use super::*;

    #[test]
    fn the_config_wins_at_boot_and_is_clamped_to_the_hardware() {
        let (n, event) = reconcile_limit(3, 8, None);
        assert_eq!(n, 3);
        assert_eq!(
            event,
            Some(EventKind::MaxConcurrentAgentsChanged {
                n: 3,
                hardware_max: 8
            })
        );
        assert_eq!(reconcile_limit(20, 8, None).0, 8);
        assert_eq!(reconcile_limit(0, 8, None).0, 1);
    }

    #[test]
    fn nothing_is_recorded_when_state_already_matches() {
        let current = Some(AgentLimit {
            limit: 3,
            hardware_max: 8,
        });
        assert_eq!(reconcile_limit(3, 8, current).1, None);
        assert!(reconcile_limit(3, 4, current).1.is_some());
    }
}
