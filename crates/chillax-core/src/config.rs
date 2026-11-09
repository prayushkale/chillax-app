//! `chillax.toml`. Secrets never live here (AGENTS.md rule 7).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    pub project: ProjectConfig,
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub tracker: TrackerConfig,
    /// Agent backends by name (`[agents.claude]`) and how their permission requests are
    /// handled (`[agents] permissions`).
    #[serde(default)]
    pub agents: AgentsConfig,
    #[serde(default)]
    pub budget: BudgetConfig,
    #[serde(default)]
    pub catalog: CatalogConfig,
    /// Environments work moves through.
    #[serde(default)]
    pub env: EnvConfig,
    /// Where branches live: only local, or also on GitHub.
    #[serde(default)]
    pub vcs: VcsConfig,
    /// What happens after a release is deployed.
    #[serde(default)]
    pub release: ReleaseConfig,
    /// Reporting to the herdr terminal workspace manager (ADR 0018).
    #[serde(default)]
    pub herdr: HerdrConfig,
    /// Who is hired automatically in a new project (ADR 0020 addendum).
    #[serde(default)]
    pub team: TeamConfig,
    /// USD per million tokens by model name, for estimating cost when a backend reports
    /// tokens but no cost (ADR 0023). Empty by default: Chillax ships no vendor prices.
    #[serde(default)]
    pub pricing: BTreeMap<String, crate::pricing::ModelPrice>,
}

impl Config {
    /// This config with the fields that can change while Chillax runs taken from `new`:
    /// worktree mode, the `[env.*]` commands and timeouts, the agent limit, the release
    /// sync and the project manager's timings. Everything else stays as it is.
    pub fn with_live_fields_of(&self, new: &Config) -> Config {
        let mut out = self.clone();
        out.vcs.worktrees = new.vcs.worktrees;
        out.env.dev.test = new.env.dev.test.clone();
        out.env.dev.test_timeout_secs = new.env.dev.test_timeout_secs;
        let (qa, new_qa) = (&mut out.env.qa, &new.env.qa);
        qa.up = new_qa.up.clone();
        qa.test = new_qa.test.clone();
        qa.down = new_qa.down.clone();
        qa.timeout_secs = new_qa.timeout_secs;
        out.env.prod.deploy = new.env.prod.deploy.clone();
        out.env.prod.timeout_secs = new.env.prod.timeout_secs;
        out.budget = new.budget.clone();
        out.release = new.release.clone();
        out.team.checkin_minutes = new.team.checkin_minutes;
        out.team.stall_minutes = new.team.stall_minutes;
        out.team.problem_grace_minutes = new.team.problem_grace_minutes;
        out.team.problem_max_attempts = new.team.problem_max_attempts;
        out.team.problem_retry_minutes = new.team.problem_retry_minutes;
        out
    }

    /// The settings in `new` that differ from `self` and cannot change while Chillax
    /// runs (ports, backends, branches, the tracker, ...). Compare against the config
    /// after [`Config::with_live_fields_of`]; the names are section paths for a message.
    pub fn restart_needed(&self, new: &Config) -> Vec<&'static str> {
        let live = self.with_live_fields_of(new);
        let mut out = Vec::new();
        let mut check = |name: &'static str, differs: bool| {
            if differs {
                out.push(name);
            }
        };
        check("[project]", live.project != new.project);
        check("[server]", live.server != new.server);
        check("[tracker]", live.tracker != new.tracker);
        check("[agents]", live.agents != new.agents);
        check("[catalog]", live.catalog != new.catalog);
        check("[herdr]", live.herdr != new.herdr);
        check("[pricing]", live.pricing != new.pricing);
        check("[team]", live.team != new.team);
        check("[vcs] kind/remote", live.vcs != new.vcs);
        check("[env] files/branches/apps", live.env != new.env);
        out
    }
}

/// Role hired by default in a new project.
pub const DEFAULT_PM_ROLE: &str = "project-manager";

fn default_hires() -> Vec<String> {
    vec![DEFAULT_PM_ROLE.to_string()]
}

fn default_max_team_size() -> u32 {
    8
}

fn default_checkin_minutes() -> u32 {
    10
}

fn default_stall_minutes() -> u32 {
    20
}

fn default_problem_grace_minutes() -> u32 {
    15
}

fn default_problem_max_attempts() -> u32 {
    2
}

fn default_problem_retry_minutes() -> u32 {
    5
}

fn default_max_open_suggestions() -> u32 {
    3
}

/// `[team]`: who a brand-new project starts with, and who may hire later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamConfig {
    /// Role ids hired on the first start of a project whose event log is empty. An empty
    /// list opts out.
    #[serde(default = "default_hires")]
    pub default_hires: Vec<String>,
    /// Role ids whose employees may hire teammates directly. Empty means nobody may.
    /// A hiring role cannot itself be hired by an employee.
    #[serde(default = "default_hires")]
    pub hiring_roles: Vec<String>,
    /// Most employees a hiring role may grow the team to; the stakeholder is not capped.
    #[serde(default = "default_max_team_size")]
    pub max_team_size: u32,
    /// Minutes between the project manager's board check-ins while work is open; 0 turns
    /// them off.
    #[serde(default = "default_checkin_minutes")]
    pub checkin_minutes: u32,
    /// Minutes open sprint work may go without moving before hung agent turns are closed
    /// and the project manager is asked to get it going; 0 turns the watchdog off.
    #[serde(default = "default_stall_minutes")]
    pub stall_minutes: u32,
    /// Minutes the project manager gets to handle a problem he can solve before the
    /// stakeholder is told; 0 alerts at once, as before (ADR 0032).
    #[serde(default = "default_problem_grace_minutes")]
    pub problem_grace_minutes: u32,
    /// Most problem briefings the project manager gets for one problem.
    #[serde(default = "default_problem_max_attempts")]
    pub problem_max_attempts: u32,
    /// Minutes between briefings about the same problem.
    #[serde(default = "default_problem_retry_minutes")]
    pub problem_retry_minutes: u32,
    /// Most suggestions one employee may have waiting for the project manager.
    #[serde(default = "default_max_open_suggestions")]
    pub max_open_suggestions: u32,
}

impl Default for TeamConfig {
    fn default() -> Self {
        Self {
            default_hires: default_hires(),
            hiring_roles: default_hires(),
            max_team_size: default_max_team_size(),
            checkin_minutes: default_checkin_minutes(),
            stall_minutes: default_stall_minutes(),
            problem_grace_minutes: default_problem_grace_minutes(),
            problem_max_attempts: default_problem_max_attempts(),
            problem_retry_minutes: default_problem_retry_minutes(),
            max_open_suggestions: default_max_open_suggestions(),
        }
    }
}

/// When Chillax talks to herdr.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HerdrMode {
    /// Only when Chillax runs inside a herdr pane.
    #[default]
    Auto,
    /// Always try; warn when herdr is not there.
    On,
    Off,
}

impl<'de> Deserialize<'de> for HerdrMode {
    /// Accepts `"auto"`, `"true"`, `"false"` and the TOML booleans.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Bool(bool),
            Text(String),
        }
        match Raw::deserialize(d)? {
            Raw::Bool(true) => Ok(Self::On),
            Raw::Bool(false) => Ok(Self::Off),
            Raw::Text(t) => match t.as_str() {
                "auto" => Ok(Self::Auto),
                "true" => Ok(Self::On),
                "false" => Ok(Self::Off),
                other => Err(serde::de::Error::custom(format!(
                    "herdr enabled must be \"auto\", \"true\" or \"false\", not `{other}`"
                ))),
            },
        }
    }
}

/// `[herdr]`: report state and notifications to herdr when Chillax runs in one of its panes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HerdrConfig {
    #[serde(default)]
    pub enabled: HerdrMode,
    /// Send "needs you" and release notifications through herdr.
    #[serde(default = "yes")]
    pub notifications: bool,
    /// Send the inbox, QA and project tokens beside the state.
    #[serde(default = "yes")]
    pub metadata: bool,
}

impl Default for HerdrConfig {
    fn default() -> Self {
        Self {
            enabled: HerdrMode::Auto,
            notifications: true,
            metadata: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VcsKind {
    /// Branches and merges stay in the local repository.
    #[default]
    Local,
    /// Branches are pushed, work is reviewed and merged as GitHub pull requests (via `gh`).
    Github,
}

/// `[release]`: what follows a deployed release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseConfig {
    /// Fast-forward `dev`, `qa` and the prod branch to one commit, push them and check the
    /// prod branch out in the main checkout after a deploy. On by default.
    #[serde(default = "default_true")]
    pub sync_after_deploy: bool,
}

impl Default for ReleaseConfig {
    fn default() -> Self {
        Self {
            sync_after_deploy: true,
        }
    }
}

fn default_true() -> bool {
    true
}

/// `[vcs]`: how finished work travels.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VcsConfig {
    #[serde(default)]
    pub kind: VcsKind,
    /// The git remote branches are pushed to.
    #[serde(default = "default_remote")]
    pub remote: String,
    /// Where ticket work happens (ADR 0025).
    #[serde(default)]
    pub worktrees: WorktreeMode,
}

impl Default for VcsConfig {
    fn default() -> Self {
        Self {
            kind: VcsKind::Local,
            remote: default_remote(),
            worktrees: WorktreeMode::default(),
        }
    }
}

/// `[vcs] worktrees`: how much disk ticket work may use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorktreeMode {
    /// Only the `dev`, `qa` and `main` worktrees exist. A ticket's branch is checked out
    /// in the dev worktree, one ticket at a time.
    Shared,
    /// A worktree per ticket under `.chillax/worktrees/<id>`; tickets run in parallel
    /// (default, ADR 0038).
    #[default]
    PerTicket,
}

fn default_remote() -> String {
    "origin".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvConfig {
    /// Gitignored files in the repository root (`.env` and the like) that every worktree
    /// shares: Chillax links them into each worktree and sets their values on the
    /// processes it starts. Names only; the values never live in config.
    #[serde(default = "default_env_files")]
    pub files: Vec<String>,
    #[serde(default)]
    pub dev: DevEnv,
    #[serde(default)]
    pub qa: QaEnv,
    #[serde(default)]
    pub prod: ProdEnv,
}

impl Default for EnvConfig {
    fn default() -> Self {
        Self {
            files: default_env_files(),
            dev: DevEnv::default(),
            qa: QaEnv::default(),
            prod: ProdEnv::default(),
        }
    }
}

fn default_env_files() -> Vec<String> {
    vec![".env".into(), ".env.local".into()]
}

/// How Chillax runs an environment's app (ADR 0015). Every field is optional; without
/// `run` nothing is supervised. The command comes from the committed config and runs in
/// the user's shell with the same trust as `up` and `test`: Chillax does not sandbox it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppConfig {
    /// Shell command that starts the app and keeps running, e.g.
    /// `npm run dev -- --port {port}`. `{port}` is replaced and `PORT` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    /// The localhost port the app listens on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// The address shown for the app; default `http://localhost:<port>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Directory inside the checkout to run in (relative, no `..`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_cwd: Option<String>,
}

/// One app to supervise: an [`AppConfig`] with a command and a port, resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppSpec {
    /// `dev`, `qa` or `prod`.
    pub env: String,
    pub command: String,
    pub port: u16,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// The environments an app can run in, in display order.
pub const APP_ENVS: [&str; 3] = ["dev", "qa", "prod"];

impl AppConfig {
    fn spec(&self, env: &str) -> Option<AppSpec> {
        let command = self
            .run
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())?;
        let port = self.port?;
        Some(AppSpec {
            env: env.to_string(),
            command: command.to_string(),
            port,
            url: self
                .url
                .clone()
                .unwrap_or_else(|| format!("http://localhost:{port}")),
            cwd: self.run_cwd.clone(),
        })
    }
}

impl EnvConfig {
    /// The apps to run: environments with both a `run` command and a `port`.
    pub fn apps(&self) -> Vec<AppSpec> {
        [
            ("dev", &self.dev.app),
            ("qa", &self.qa.app),
            ("prod", &self.prod.app),
        ]
        .into_iter()
        .filter_map(|(env, app)| app.spec(env))
        .collect()
    }

    /// The env files whose values are set on processes for `stage` (`dev`, `qa` or
    /// `prod`), later ones winning: the shared files, then the stage's own file
    /// (`env_file`, by default `.env.<stage>`). Paths are relative to the repository root.
    pub fn files_for(&self, stage: &str) -> Vec<String> {
        let own = match stage {
            "dev" => self.dev.env_file.clone(),
            "qa" => self.qa.env_file.clone(),
            "prod" => self.prod.env_file.clone(),
            _ => None,
        };
        let mut files = self.files.clone();
        files.push(own.unwrap_or_else(|| format!(".env.{stage}")));
        files
    }
}

impl Config {
    /// Problems with the app settings that would only show up as a confusing crash later.
    pub fn app_problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let mut taken: Vec<(String, u16)> = vec![("the chillax server".into(), self.server.port)];
        for (env, app) in [
            ("dev", &self.env.dev.app),
            ("qa", &self.env.qa.app),
            ("prod", &self.env.prod.app),
        ] {
            if app.run.as_deref().is_some_and(|r| !r.trim().is_empty()) && app.port.is_none() {
                problems.push(format!("[env.{env}] has `run` but no `port`"));
            }
            if app.run.is_none() && (app.port.is_some() || app.url.is_some()) {
                problems.push(format!(
                    "[env.{env}] has a port or url but no `run` command"
                ));
            }
            if let Some(dir) = &app.run_cwd
                && !inside_repo(dir)
            {
                problems.push(format!(
                    "[env.{env}] run_cwd `{dir}` must be a relative path inside the repository"
                ));
            }
            let Some(port) = app.port else { continue };
            if port == 0 {
                problems.push(format!("[env.{env}] port must not be 0"));
            }
            if let Some((other, _)) = taken.iter().find(|(_, p)| *p == port) {
                problems.push(format!(
                    "[env.{env}] port {port} is already used by {other}"
                ));
            }
            taken.push((format!("[env.{env}]"), port));
        }
        let mut files: Vec<String> = APP_ENVS
            .iter()
            .flat_map(|stage| self.env.files_for(stage))
            .collect();
        files.sort();
        files.dedup();
        for file in files.iter().filter(|f| !inside_repo(f)) {
            problems.push(format!(
                "env file `{file}` must be a relative path inside the repository"
            ));
        }
        problems
    }
}

/// A relative path that stays inside the repository: no root, no `..`.
fn inside_repo(path: &str) -> bool {
    let path = std::path::Path::new(path);
    path.is_relative()
        && path
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
}

/// The shared `qa` branch and the commands that bring its environment up, test it and
/// tear it down. Every command is optional.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QaEnv {
    #[serde(default = "default_qa_branch")]
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub up: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub down: Option<String>,
    /// Applies to each of `up`, `test` and `down`.
    #[serde(default = "default_test_timeout")]
    pub timeout_secs: u64,
    /// Overrides for QA on top of `[env] files`; default `.env.qa`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_file: Option<String>,
    /// The app Chillax keeps running from the `qa` branch (ADR 0015).
    #[serde(flatten)]
    pub app: AppConfig,
}

impl Default for QaEnv {
    fn default() -> Self {
        Self {
            branch: default_qa_branch(),
            up: None,
            test: None,
            down: None,
            timeout_secs: default_test_timeout(),
            env_file: None,
            app: AppConfig::default(),
        }
    }
}

/// Where releases land. Without a `deploy` command a release is just a tag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProdEnv {
    #[serde(default = "default_prod_branch")]
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deploy: Option<String>,
    #[serde(default = "default_test_timeout")]
    pub timeout_secs: u64,
    /// Overrides for prod on top of `[env] files`; default `.env.prod`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_file: Option<String>,
    /// The app Chillax keeps running from the latest release tag (ADR 0015).
    #[serde(flatten)]
    pub app: AppConfig,
}

impl Default for ProdEnv {
    fn default() -> Self {
        Self {
            branch: default_prod_branch(),
            deploy: None,
            timeout_secs: default_test_timeout(),
            env_file: None,
            app: AppConfig::default(),
        }
    }
}

fn default_qa_branch() -> String {
    "qa".into()
}
fn default_prod_branch() -> String {
    "main".into()
}

/// Where finished work lands and how Chillax checks it before review.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DevEnv {
    /// Tickets branch from here and are merged back into it.
    #[serde(default = "default_base_branch")]
    pub base_branch: String,
    /// Shell command run in a ticket's worktree on `request_review`. Without one, the
    /// tests count as skipped and every review request goes to a reviewer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test: Option<String>,
    #[serde(default = "default_test_timeout")]
    pub test_timeout_secs: u64,
    /// Overrides for dev on top of `[env] files`; default `.env.dev`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_file: Option<String>,
    /// The app Chillax keeps running from the `dev` branch (ADR 0015).
    #[serde(flatten)]
    pub app: AppConfig,
}

impl Default for DevEnv {
    fn default() -> Self {
        Self {
            base_branch: default_base_branch(),
            test: None,
            test_timeout_secs: default_test_timeout(),
            env_file: None,
            app: AppConfig::default(),
        }
    }
}

fn default_base_branch() -> String {
    "dev".into()
}
fn default_test_timeout() -> u64 {
    600
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProjectConfig {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(from = "RawServerConfig")]
pub struct ServerConfig {
    pub bind: String,
    /// The port to listen on. Without an explicit one this is only the first port tried
    /// (ADR 0017).
    pub port: u16,
    /// `[server] port` is set to something other than the default 4747: no fallback to
    /// another port.
    #[serde(skip)]
    pub port_explicit: bool,
    pub theme: String,
}

#[derive(Deserialize)]
struct RawServerConfig {
    #[serde(default = "default_bind")]
    bind: String,
    port: Option<u16>,
    #[serde(default = "default_theme")]
    theme: String,
}

impl From<RawServerConfig> for ServerConfig {
    fn from(raw: RawServerConfig) -> Self {
        Self {
            bind: raw.bind,
            port: raw.port.unwrap_or_else(default_port),
            // Older starter files spell out the default; that must not pin the port.
            port_explicit: raw.port.is_some_and(|p| p != default_port()),
            theme: raw.theme,
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            port: default_port(),
            port_explicit: false,
            theme: default_theme(),
        }
    }
}

fn default_bind() -> String {
    "127.0.0.1".into()
}
fn default_port() -> u16 {
    4747
}
fn default_theme() -> String {
    "default".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackerConfig {
    /// Only `local` (built in, ADR 0002) is supported; the CLI rejects other kinds.
    #[serde(default = "default_tracker")]
    pub kind: String,
}

impl Default for TrackerConfig {
    fn default() -> Self {
        Self {
            kind: default_tracker(),
        }
    }
}

fn default_tracker() -> String {
    "local".into()
}

/// The `[agents]` table: `permissions` plus one sub-table per backend. Derefs to the
/// backend map, so `config.agents["claude"]` and `.contains_key()` read as before.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentsConfig {
    /// How sessions answer an agent's permission requests.
    #[serde(default)]
    pub permissions: PermissionMode,
    /// Minutes without a turn after which a chat session is closed (5 suits cost-sensitive
    /// setups). It restarts on the next message.
    #[serde(default = "default_chat_idle_minutes")]
    pub chat_idle_minutes: u64,
    #[serde(flatten)]
    pub backends: BTreeMap<String, AgentConfig>,
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            permissions: PermissionMode::default(),
            chat_idle_minutes: default_chat_idle_minutes(),
            backends: BTreeMap::new(),
        }
    }
}

fn default_chat_idle_minutes() -> u64 {
    10
}

impl std::ops::Deref for AgentsConfig {
    type Target = BTreeMap<String, AgentConfig>;
    fn deref(&self) -> &Self::Target {
        &self.backends
    }
}

impl std::ops::DerefMut for AgentsConfig {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.backends
    }
}

/// How an agent's `session/request_permission` calls are answered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Approve every request (preferring "always allow") and switch the agent to its
    /// bypass or auto mode when it offers one. Unattended work never stalls on a prompt.
    #[default]
    Auto,
    /// Role policy decides: `auto_approve` inside the worktree, the stakeholder otherwise.
    Policy,
}

/// How to launch an ACP agent subprocess.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables (non-secret) for the agent process.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// The backend reports token usage as a running total for the session rather than per
    /// turn, so the runtime records the difference (ADR 0023).
    #[serde(default)]
    pub tokens_cumulative: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetConfig {
    #[serde(default = "default_concurrency")]
    pub max_concurrent_agents: u32,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            max_concurrent_agents: default_concurrency(),
        }
    }
}

fn default_concurrency() -> u32 {
    3
}

/// The persona catalog hiring draws from. Its text is untrusted: it is
/// reviewed before adoption and never grants tools or permissions (ADR 0003).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogConfig {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "default_catalog_name")]
    pub name: String,
    #[serde(default = "default_catalog_url")]
    pub url: String,
    /// Branch or tag to follow.
    #[serde(default = "default_catalog_ref", rename = "ref")]
    pub git_ref: String,
    /// Fetch the latest catalog in the background whenever Chillax starts.
    #[serde(default = "yes")]
    pub refresh_on_start: bool,
    /// Persona paths shown first, before the stakeholder searches.
    #[serde(default = "default_featured")]
    pub featured: Vec<String>,
    /// Which trusted role a persona inherits its policy from, before the stakeholder
    /// changes it. Keys are `*name pattern*`s or divisions (top-level directories);
    /// `"*"` is the fallback. Merged over [`default_base_roles`]; an empty value turns a
    /// default off. See [`crate::recruit::suggest_base`].
    #[serde(default)]
    pub base_roles: BTreeMap<String, String>,
}

impl Default for CatalogConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            name: default_catalog_name(),
            url: default_catalog_url(),
            git_ref: default_catalog_ref(),
            refresh_on_start: true,
            featured: default_featured(),
            base_roles: BTreeMap::new(),
        }
    }
}

impl CatalogConfig {
    /// The base-role rules in effect: the defaults with the project's table on top.
    pub fn base_role_rules(&self) -> BTreeMap<String, String> {
        let mut rules = default_base_roles();
        for (key, role) in &self.base_roles {
            let key = key.trim().to_lowercase();
            if role.trim().is_empty() {
                rules.remove(&key);
            } else {
                rules.insert(key, role.trim().to_string());
            }
        }
        rules
    }
}

/// Built-in suggestions for the starter roles.
pub fn default_base_roles() -> BTreeMap<String, String> {
    [
        ("*architect*", "tech-lead"),
        ("*reviewer*", "tech-lead"),
        ("*tester*", "qa-engineer"),
        ("*manager*", "project-manager"),
        ("engineering", "fullstack-developer"),
        ("testing", "qa-engineer"),
        ("project-management", "project-manager"),
        ("product", "project-manager"),
        ("*", "fullstack-developer"),
    ]
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .into()
}

fn yes() -> bool {
    true
}
fn default_catalog_name() -> String {
    "agency-agents".into()
}
fn default_catalog_url() -> String {
    "https://github.com/msitarzewski/agency-agents.git".into()
}
fn default_catalog_ref() -> String {
    "main".into()
}
fn default_featured() -> Vec<String> {
    [
        "project-management/project-manager-senior.md",
        "engineering/engineering-senior-developer.md",
        "engineering/engineering-frontend-developer.md",
        "engineering/engineering-backend-architect.md",
        "engineering/engineering-code-reviewer.md",
        "testing/testing-reality-checker.md",
        "testing/testing-api-tester.md",
        "engineering/engineering-devops-automator.md",
        "design/design-ux-architect.md",
        "engineering/engineering-technical-writer.md",
    ]
    .map(String::from)
    .to_vec()
}

impl Config {
    pub fn from_toml(source: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(source)
    }

    /// The config `chillax init` writes for a new project.
    pub fn starter(project: &str) -> String {
        format!(
            r#"[project]
name = "{project}"

[server]
bind = "127.0.0.1"       # localhost only by default
# port = 4747           # default; when taken by another project, the next free port is used
theme = "default"

[tracker]
kind = "local"           # built-in tracker (ADR 0002)

[agents]
permissions = "auto"     # approve every agent permission request; "policy" asks per role rules
# chat_idle_minutes = 10   # close an idle chat session after this long; 5 saves money
# Cost: give QA, reviewer and PM roles a cheaper model (`model:` in a role file, or pick one
# when hiring); keep the strongest model for developers.

[agents.claude]
command = "npx"
args = ["-y", "@agentclientprotocol/claude-agent-acp"]

# [agents.codex]
# command = "codex-acp"

# [agents.gemini]
# command = "gemini"
# args = ["--acp"]

# [agents.command-code]
# command = "command-code"
# args = ["acp"]

# Prices (USD per million tokens) let Chillax estimate the cost of turns where the agent
# reports tokens but no cost. Chillax ships no prices; copy them from your vendor's page.
# Keys match the model name exactly or by prefix; add aliases (`sonnet`) as their own keys.
# [pricing."claude-sonnet-4-5"]
# input_per_mtok = 3.0
# output_per_mtok = 15.0
# cached_per_mtok = 0.3        # optional, defaults to the input price
# cache_write_per_mtok = 3.75  # optional, defaults to the input price

[budget]
max_concurrent_agents = 3

# Roles hired automatically on the first start of a new project. [] hires nobody.
[team]
default_hires = ["project-manager"]
# Roles that may hire teammates themselves, and the team size they may not exceed.
hiring_roles = ["project-manager"]
max_team_size = 8
# Minutes between the project manager's board check-ins while work is open (0 = off).
checkin_minutes = 10
# Minutes sprint work may sit without moving before Chillax closes hung agent turns and
# asks the project manager to get it going; a second quiet stretch alerts you (0 = off).
stall_minutes = 20
# Problems the project manager can solve (a failing action, a crashed teammate, a crashing
# app) go to him first. He gets this many minutes and briefings before the stakeholder is
# told what he tried (0 = alert at once). Sign-in, billing, approvals and releases always
# alert at once.
problem_grace_minutes = 15
problem_max_attempts = 2
problem_retry_minutes = 5

# Where branches live. "github" pushes each ticket's branch, opens a pull request into
# `dev`, merges it through `gh`, and releases through a pull request into `main`.
# Needs a GitHub remote and `gh auth login`; `chillax doctor` checks both.
[vcs]
kind = "local"
# kind = "github"
# remote = "origin"
# "per-ticket" (default): a worktree per ticket, tickets run in parallel. "shared": ticket
# work runs on a branch in the one dev worktree, one ticket at a time, which saves disk.
# worktrees = "per-ticket"

# Inside herdr (https://herdr.dev), Chillax reports its state and sends notifications to it.
# [herdr]
# enabled = "auto"          # "auto": only inside a herdr pane; "true" or "false" to force
# notifications = true
# metadata = true

# Gitignored env files in the repository root that every worktree shares.
# Chillax links them into each worktree and sets their values on agents, `test`/`deploy`
# commands and apps. `.env.dev`, `.env.qa` and `.env.prod` (or `env_file` in [env.<stage>])
# override them for one stage. Keep the values out of this file.
# [env]
# files = [".env", ".env.local"]

# Where finished work is merged, and how Chillax checks it before a review.
[env.dev]
base_branch = "dev"
# test = "cargo test"       # run in the ticket's worktree; without it tests are skipped
# Chillax can run the app for you from the `dev` checkout and restart it after each merge.
# `run` is a shell command (same trust as `test`; Chillax does not sandbox it). {{port}} and
# $PORT hold `port`. Output goes to .chillax/state/env/dev.log.
# run = "npm run dev -- --port {{port}}"
# port = 3000
# url = "http://localhost:3000"   # optional; shown and clickable in the Environments view
# run_cwd = "web"                 # optional; a folder inside the repository

# The shared QA environment: `dev` is promoted to this branch, then QA agents test it.
[env.qa]
branch = "qa"
# up = "docker compose -f compose.qa.yml up -d"
# test = "make e2e"
# down = "docker compose -f compose.qa.yml down"
# run = "npm start -- --port {{port}}"   # restarted on every promotion
# port = 3001

# Releases merge `qa` here and are tagged. Without `deploy`, a release is just a tag.
[env.prod]
branch = "main"
# deploy = "./scripts/deploy.sh"
# run = "npm start -- --port {{port}}"   # runs the latest release tag from a detached worktree
# port = 3002

# After a release is deployed, Chillax fast-forwards `dev`, `qa` and `main` to one commit,
# pushes them (never forced) and checks `main` out in this directory when that is safe.
# Anything it cannot do safely is skipped and raised as an alert. Set false to opt out.
[release]
sync_after_deploy = true

# Persona catalog for hiring (refreshed on start; reviewed before use).
[catalog]
enabled = true
name = "agency-agents"
url = "https://github.com/msitarzewski/agency-agents.git"
ref = "main"
refresh_on_start = true

# Base role a persona gets by default (you can still change it while reviewing).
# Name patterns win over divisions; "*" is the fallback. Merged over the defaults.
# [catalog.base_roles]
# "*architect*" = "tech-lead"
# engineering = "fullstack-developer"
# "*" = "fullstack-developer"
"#
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn team_defaults_to_a_project_manager_and_can_opt_out() {
        let base = "[project]\nname = \"a\"\n";
        let config = Config::from_toml(base).expect("parses");
        assert_eq!(config.team.default_hires, ["project-manager"]);
        let config = Config::from_toml(&format!(
            "{base}[team]\ndefault_hires = [\"qa\", \"tech-lead\"]\n"
        ))
        .expect("parses");
        assert_eq!(config.team.default_hires, ["qa", "tech-lead"]);
        let config =
            Config::from_toml(&format!("{base}[team]\ndefault_hires = []\n")).expect("parses");
        assert!(config.team.default_hires.is_empty());
        let starter = Config::from_toml(&Config::starter("acme")).expect("starter parses");
        assert_eq!(starter.team.default_hires, ["project-manager"]);
    }

    #[test]
    fn hiring_policy_defaults_overrides_and_old_team_tables_parse() {
        let base = "[project]\nname = \"a\"\n";
        let config = Config::from_toml(base).expect("parses");
        assert_eq!(config.team.hiring_roles, ["project-manager"]);
        assert_eq!(config.team.max_team_size, 8);
        assert_eq!(config.team.checkin_minutes, 10);
        assert_eq!(config.team.stall_minutes, 20);
        assert_eq!(config.team.problem_grace_minutes, 15);
        assert_eq!(config.team.problem_max_attempts, 2);
        assert_eq!(config.team.problem_retry_minutes, 5);
        let config = Config::from_toml(&format!(
            "{base}[team]\nproblem_grace_minutes = 0\nproblem_max_attempts = 4\nproblem_retry_minutes = 1\n"
        ))
        .expect("parses");
        assert_eq!(config.team.problem_grace_minutes, 0);
        assert_eq!(config.team.problem_max_attempts, 4);
        assert_eq!(config.team.problem_retry_minutes, 1);
        let config =
            Config::from_toml(&format!("{base}[team]\ncheckin_minutes = 0\n")).expect("parses");
        assert_eq!(config.team.checkin_minutes, 0);
        let config = Config::from_toml(&format!(
            "{base}[team]\nhiring_roles = [\"lead\"]\nmax_team_size = 3\n"
        ))
        .expect("parses");
        assert_eq!(config.team.hiring_roles, ["lead"]);
        assert_eq!(config.team.max_team_size, 3);
        assert_eq!(config.team.default_hires, ["project-manager"]);
        let config = Config::from_toml(&format!("{base}[team]\ndefault_hires = [\"qa\"]\n"))
            .expect("old table parses");
        assert_eq!(config.team.hiring_roles, ["project-manager"]);
        assert_eq!(config.team.max_team_size, 8);
        let config =
            Config::from_toml(&format!("{base}[team]\nhiring_roles = []\n")).expect("parses");
        assert!(config.team.hiring_roles.is_empty());
        let starter = Config::from_toml(&Config::starter("acme")).expect("starter parses");
        assert_eq!(starter.team, TeamConfig::default());
    }

    #[test]
    fn vcs_defaults_to_local_and_can_be_switched_to_github() {
        let config = Config::from_toml(&Config::starter("acme")).expect("starter parses");
        assert_eq!(config.vcs.kind, VcsKind::Local);
        assert_eq!(config.vcs.remote, "origin");
        assert_eq!(config.vcs.worktrees, WorktreeMode::PerTicket);
        let config = Config::from_toml("[project]\nname = \"a\"\n[vcs]\nworktrees = \"shared\"\n")
            .expect("parses");
        assert_eq!(config.vcs.worktrees, WorktreeMode::Shared);
        let config = Config::from_toml(
            "[project]\nname = \"a\"\n[vcs]\nkind = \"github\"\nremote = \"upstream\"\n",
        )
        .expect("parses");
        assert_eq!(config.vcs.kind, VcsKind::Github);
        assert_eq!(config.vcs.remote, "upstream");
    }

    #[test]
    fn herdr_is_auto_by_default_and_accepts_text_or_booleans() {
        let parse = |extra: &str| {
            Config::from_toml(&format!("[project]\nname = \"x\"\n{extra}")).expect("parses")
        };
        assert_eq!(parse("").herdr, HerdrConfig::default());
        assert_eq!(parse("").herdr.enabled, HerdrMode::Auto);
        assert_eq!(
            parse("[herdr]\nenabled = \"auto\"\n").herdr.enabled,
            HerdrMode::Auto
        );
        assert_eq!(
            parse("[herdr]\nenabled = \"true\"\n").herdr.enabled,
            HerdrMode::On
        );
        assert_eq!(
            parse("[herdr]\nenabled = true\n").herdr.enabled,
            HerdrMode::On
        );
        let off = parse("[herdr]\nenabled = false\nnotifications = false\nmetadata = false\n");
        assert_eq!(off.herdr.enabled, HerdrMode::Off);
        assert!(!off.herdr.notifications && !off.herdr.metadata);
        assert!(
            Config::from_toml("[project]\nname = \"x\"\n[herdr]\nenabled = \"maybe\"\n").is_err()
        );
        assert!(parse("[herdr]\n").herdr.notifications);
    }

    fn parse(extra: &str) -> Config {
        Config::from_toml(&format!("[project]\nname = \"x\"\n{extra}")).expect("parses")
    }

    #[test]
    fn live_fields_are_taken_from_the_new_config() {
        let old = parse("");
        let new = parse(
            "[vcs]\nworktrees = \"per-ticket\"\n[env.qa]\nup = \"migrate\"\n\
             [budget]\nmax_concurrent_agents = 5\n[release]\nsync_after_deploy = false\n\
             [team]\ncheckin_minutes = 7\nstall_minutes = 5\n",
        );
        let live = old.with_live_fields_of(&new);
        assert_eq!(live, new);
        assert!(old.restart_needed(&new).is_empty());
    }

    #[test]
    fn fixed_fields_are_reported_and_kept() {
        let old = parse("");
        let new = parse("[server]\nport = 9999\n[env.qa]\nbranch = \"staging\"\nup = \"x\"\n");
        let live = old.with_live_fields_of(&new);
        assert_eq!(live.env.qa.up.as_deref(), Some("x"));
        assert_eq!(live.env.qa.branch, old.env.qa.branch);
        assert_eq!(
            old.restart_needed(&new),
            ["[server]", "[env] files/branches/apps"]
        );
    }

    #[test]
    fn apps_are_optional_and_resolve_a_url() {
        let config = Config::from_toml(&Config::starter("acme")).expect("starter parses");
        assert!(config.env.apps().is_empty());
        assert!(config.app_problems().is_empty());
        let config = Config::from_toml(
            "[project]\nname = \"x\"\n[env.dev]\nrun = \"npm run dev -- --port {port}\"\nport = 3000\n\
             run_cwd = \"web\"\n[env.qa]\nrun = \"npm start\"\nport = 3001\nurl = \"http://qa.localhost:3001\"\n\
             up = \"make up\"\n",
        )
        .expect("parses");
        let apps = config.env.apps();
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0].env, "dev");
        assert_eq!(apps[0].url, "http://localhost:3000");
        assert_eq!(apps[0].cwd.as_deref(), Some("web"));
        assert_eq!(apps[1].url, "http://qa.localhost:3001");
        assert_eq!(config.env.qa.up.as_deref(), Some("make up"));
        assert!(config.app_problems().is_empty());
    }

    #[test]
    fn app_ports_must_be_distinct_and_not_the_server_port() {
        let config = Config::from_toml(
            "[project]\nname = \"x\"\n[env.dev]\nrun = \"a\"\nport = 3000\n\
             [env.qa]\nrun = \"b\"\nport = 3000\n[env.prod]\nrun = \"c\"\nport = 4747\n",
        )
        .expect("parses");
        let problems = config.app_problems();
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].contains("[env.qa] port 3000"), "{problems:?}");
        assert!(problems[1].contains("the chillax server"), "{problems:?}");
    }

    #[test]
    fn a_run_command_needs_a_port_and_a_port_needs_a_command() {
        let config = Config::from_toml(
            "[project]\nname = \"x\"\n[env.dev]\nrun = \"a\"\n[env.qa]\nport = 3001\n\
             [env.prod]\nrun = \"c\"\nport = 3002\nrun_cwd = \"../x\"\n",
        )
        .expect("parses");
        let problems = config.app_problems();
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(config.env.apps().iter().all(|a| a.env == "prod"));
    }

    #[test]
    fn env_files_are_shared_then_overridden_per_stage() {
        let config = Config::from_toml(&Config::starter("acme")).expect("starter parses");
        assert_eq!(
            config.env.files_for("qa"),
            [".env", ".env.local", ".env.qa"]
        );
        let config = Config::from_toml(
            "[project]\nname = \"x\"\n[env]\nfiles = [\"config/.env\"]\n\
             [env.prod]\nenv_file = \"config/.env.live\"\n",
        )
        .expect("parses");
        assert_eq!(config.env.files_for("dev"), ["config/.env", ".env.dev"]);
        assert_eq!(
            config.env.files_for("prod"),
            ["config/.env", "config/.env.live"]
        );
        assert!(config.app_problems().is_empty());
    }

    #[test]
    fn env_files_must_stay_inside_the_repository() {
        let config = Config::from_toml(
            "[project]\nname = \"x\"\n[env]\nfiles = [\"/etc/secrets\"]\n\
             [env.qa]\nenv_file = \"../.env\"\n",
        )
        .expect("parses");
        let problems = config.app_problems();
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].contains("../.env"), "{problems:?}");
        assert!(problems[1].contains("/etc/secrets"), "{problems:?}");
    }

    #[test]
    fn chat_idle_minutes_parsed() {
        let config = Config::from_toml("[project]\nname = \"x\"\n").expect("parses");
        assert_eq!(config.agents.chat_idle_minutes, 10);
        let config = Config::from_toml(
            "[project]\nname = \"x\"\n[agents]\nchat_idle_minutes = 5\n[agents.claude]\ncommand = \"npx\"\n",
        )
        .expect("parses");
        assert_eq!(config.agents.chat_idle_minutes, 5);
        assert_eq!(config.agents.len(), 1);
    }

    #[test]
    fn only_a_non_default_server_port_is_explicit() {
        let parse = |t: &str| Config::from_toml(t).expect("parses").server;
        let none = parse("[project]\nname = \"x\"\n");
        assert!(!none.port_explicit && none.port == 4747);
        let old = parse("[project]\nname = \"x\"\n[server]\nport = 4747\n");
        assert!(!old.port_explicit);
        let set = parse("[project]\nname = \"x\"\n[server]\nport = 5000\n");
        assert!(set.port_explicit && set.port == 5000);
    }

    #[test]
    fn starter_config_round_trips() {
        let config = Config::from_toml(&Config::starter("acme")).expect("starter parses");
        assert_eq!(config.project.name, "acme");
        assert_eq!(config.server.port, 4747);
        assert!(!config.server.port_explicit);
        assert_eq!(config.tracker.kind, "local");
        assert_eq!(config.agents["claude"].command, "npx");
        assert_eq!(config.catalog.git_ref, "main");
        assert_eq!(config.env.dev.base_branch, "dev");
        assert_eq!(config.env.dev.test, None);
    }

    #[test]
    fn pricing_is_optional_validated_and_the_starter_example_parses() {
        let config = Config::from_toml("[project]\nname = \"x\"\n").expect("parses");
        assert!(config.pricing.is_empty());
        assert!(
            Config::from_toml(&Config::starter("a"))
                .expect("starter")
                .pricing
                .is_empty()
        );
        let config = Config::from_toml(
            "[project]\nname = \"x\"\n[pricing.\"claude-sonnet-4-5\"]\ninput_per_mtok = 3.0\n\
             output_per_mtok = 15.0\n[agents.a]\ncommand = \"c\"\ntokens_cumulative = true\n",
        )
        .expect("parses");
        assert_eq!(
            config.pricing["claude-sonnet-4-5"].output_micros_per_mtok,
            15_000_000
        );
        assert!(config.agents["a"].tokens_cumulative);
        assert!(
            Config::from_toml(
                "[project]\nname = \"x\"\n[pricing.m]\ninput_per_mtok = -1.0\noutput_per_mtok = 1.0\n"
            )
            .is_err()
        );
        assert!(
            Config::from_toml("[project]\nname = \"x\"\n[pricing.m]\ninput_per_mtok = 1.0\n")
                .is_err()
        );
    }

    #[test]
    fn agent_permissions_default_to_auto_and_sit_beside_the_backends() {
        let config = Config::from_toml("[project]\nname = \"x\"\n").expect("parses");
        assert_eq!(config.agents.permissions, PermissionMode::Auto);
        let config = Config::from_toml(
            "[project]\nname = \"x\"\n[agents]\npermissions = \"policy\"\n\
             [agents.claude]\ncommand = \"npx\"\n[agents.codex]\ncommand = \"codex-acp\"\n",
        )
        .expect("parses");
        assert_eq!(config.agents.permissions, PermissionMode::Policy);
        assert_eq!(config.agents.len(), 2);
        assert_eq!(config.agents["codex"].command, "codex-acp");
        // Backends without the `[agents]` header still parse, as in older configs.
        let old =
            Config::from_toml("[project]\nname = \"x\"\n[agents.claude]\ncommand = \"npx\"\n")
                .expect("parses");
        assert_eq!(old.agents.permissions, PermissionMode::Auto);
        assert!(old.agents.contains_key("claude"));
    }

    #[test]
    fn the_dev_environment_is_configurable() {
        let config = Config::from_toml(
            "[project]\nname = \"x\"\n[env.dev]\nbase_branch = \"develop\"\ntest = \"make test\"\n",
        )
        .expect("parses");
        assert_eq!(config.env.dev.base_branch, "develop");
        assert_eq!(config.env.dev.test.as_deref(), Some("make test"));
        assert_eq!(config.env.dev.test_timeout_secs, 600);
    }

    #[test]
    fn qa_and_prod_environments_default_to_their_branches_with_no_commands() {
        let config = Config::from_toml("[project]\nname = \"x\"\n").expect("parses");
        assert_eq!(config.env.qa.branch, "qa");
        assert_eq!(config.env.qa.up, None);
        assert_eq!(config.env.qa.test, None);
        assert_eq!(config.env.qa.down, None);
        assert_eq!(config.env.qa.timeout_secs, 600);
        assert_eq!(config.env.prod.branch, "main");
        assert_eq!(config.env.prod.deploy, None);
        assert_eq!(config.env.prod.timeout_secs, 600);
    }

    #[test]
    fn qa_and_prod_environments_are_configurable() {
        let config = Config::from_toml(
            "[project]\nname = \"x\"\n\
             [env.qa]\nbranch = \"staging\"\nup = \"make up\"\ntest = \"make e2e\"\n\
             down = \"make down\"\ntimeout_secs = 90\n\
             [env.prod]\nbranch = \"release\"\ndeploy = \"./deploy.sh\"\ntimeout_secs = 30\n",
        )
        .expect("parses");
        assert_eq!(config.env.qa.branch, "staging");
        assert_eq!(config.env.qa.up.as_deref(), Some("make up"));
        assert_eq!(config.env.qa.test.as_deref(), Some("make e2e"));
        assert_eq!(config.env.qa.down.as_deref(), Some("make down"));
        assert_eq!(config.env.qa.timeout_secs, 90);
        assert_eq!(config.env.prod.branch, "release");
        assert_eq!(config.env.prod.deploy.as_deref(), Some("./deploy.sh"));
        assert_eq!(config.env.prod.timeout_secs, 30);
    }

    #[test]
    fn minimal_config_uses_defaults() {
        let config = Config::from_toml("[project]\nname = \"x\"\n").expect("parses");
        assert_eq!(config.server.bind, "127.0.0.1");
        assert!(config.agents.is_empty());
        assert!(config.catalog.enabled);
        assert_eq!(config.catalog.featured.len(), 10);
        assert_eq!(config.catalog.base_role_rules(), default_base_roles());
    }

    #[test]
    fn base_role_table_merges_over_the_defaults() {
        let config = Config::from_toml(
            "[project]\nname = \"x\"\n[catalog.base_roles]\n\
             \"*Architect*\" = \"fullstack-developer\"\n\
             design = \"frontend-developer\"\n\
             \"*manager*\" = \"\"\n",
        )
        .expect("parses");
        let rules = config.catalog.base_role_rules();
        assert_eq!(rules["*architect*"], "fullstack-developer");
        assert_eq!(rules["design"], "frontend-developer");
        assert!(!rules.contains_key("*manager*"));
        assert_eq!(rules["engineering"], "fullstack-developer");
    }

    #[test]
    fn legacy_tracker_keys_still_parse() {
        let config = Config::from_toml(
            "[project]\nname = \"x\"\n[tracker]\nkind = \"legacy\"\n\
             url = \"http://localhost:8080\"\n[tracker.states]\nready = \"Todo\"\n",
        )
        .expect("parses");
        assert_eq!(config.tracker.kind, "legacy");
    }
}
