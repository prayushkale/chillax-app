//! The app supervisor (ADR 0015): keeps the dev, QA and prod apps running on
//! their localhost ports. It owns the processes; the pure decision of when to start,
//! restart or stop one is `chillax_core::apps::app_actions`, and everything that happens
//! is recorded as `App*` events through the hub.
//!
//! Each app is `sh -c <run>` in its own process group, so stopping it ends the whole tree
//! (SIGTERM, then SIGKILL after a grace period). Output is appended to
//! `.chillax/state/env/<env>.log`. A crash is retried with growing waits, a few times;
//! then the app is marked crashed and the stakeholder is told. Chillax does not sandbox
//! the command: it comes from the committed config and runs as the user.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chillax_agents::{EnvFiles, procgroup};
use chillax_core::config::AppSpec;
use chillax_core::{Actor, Command, CompanyHandle, Hub};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::process::{Child, Command as Process};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// How much of an app's output an event carries when it crashes.
const TAIL_BYTES: usize = 4000;

#[derive(Debug, Clone)]
pub struct AppsConfig {
    /// How long a process gets to exit after SIGTERM before SIGKILL.
    pub grace: Duration,
    /// First wait before restarting a crashed app; it doubles per crash in a row.
    pub backoff_base: Duration,
    pub backoff_cap: Duration,
    /// Crashes in a row after which Chillax gives up.
    pub max_attempts: u32,
    /// An app that ran this long counts as healthy: its crash counter starts over.
    pub stable_after: Duration,
    /// A log larger than this is moved to `<env>.log.1` when the app next starts.
    pub log_cap: u64,
    /// The project's env files, set on each app for its environment.
    pub env_files: EnvFiles,
}

impl Default for AppsConfig {
    fn default() -> Self {
        Self {
            grace: Duration::from_secs(5),
            backoff_base: Duration::from_secs(2),
            backoff_cap: Duration::from_secs(30),
            max_attempts: 5,
            stable_after: Duration::from_secs(60),
            log_cap: 1_000_000,
            env_files: EnvFiles::default(),
        }
    }
}

struct Handle {
    /// Which start this is, so a supervisor that gave up removes only itself.
    generation: u64,
    commit: String,
    stop: oneshot::Sender<()>,
    join: JoinHandle<()>,
}

pub struct AppSupervisor {
    hub: Arc<Hub>,
    log_dir: PathBuf,
    config: AppsConfig,
    specs: HashMap<String, AppSpec>,
    running: Mutex<HashMap<String, Handle>>,
    generation: AtomicU64,
    /// The process groups this supervisor started, so dropping it ends only its own.
    own: Mutex<BTreeSet<u32>>,
}

/// End every supervised process now, blocking: apps, agents and shell steps (they share
/// one registry in [`chillax_agents::procgroup`]). For panic hooks and signal handlers; the
/// normal path is [`AppSupervisor::shutdown`] and the runtime's own.
pub fn kill_all_processes(grace: Duration) {
    procgroup::kill_all(grace);
}

fn signal_blocking(pgid: u32, signal: &str) -> bool {
    procgroup::signal_blocking(pgid, signal)
}

async fn signal_group(pgid: u32, signal: &str) -> bool {
    procgroup::signal(pgid, signal).await
}

/// Whether any process of the group is still there (`kill -0`).
async fn group_alive(pgid: u32) -> bool {
    procgroup::alive(pgid).await
}

/// End a group: SIGTERM, wait up to `grace` for the leader and the rest, then SIGKILL.
async fn terminate(pgid: u32, child: &mut Child, grace: Duration) {
    signal_group(pgid, "TERM").await;
    let deadline = Instant::now() + grace;
    let _ = tokio::time::timeout(grace, child.wait()).await;
    while group_alive(pgid).await && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if group_alive(pgid).await {
        signal_group(pgid, "KILL").await;
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
}

fn forget(pgid: u32) {
    procgroup::forget(pgid);
}

/// The last bytes of a log, for a crash report.
async fn log_tail(path: &Path) -> String {
    let Ok(mut file) = tokio::fs::File::open(path).await else {
        return String::new();
    };
    let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(TAIL_BYTES as u64);
    if file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
        return String::new();
    }
    let mut bytes = Vec::new();
    let _ = file.read_to_end(&mut bytes).await;
    let text = String::from_utf8_lossy(&bytes);
    let text = text.trim();
    // A cut in the middle of a line is not worth showing half of.
    if start > 0
        && let Some((_, rest)) = text.split_once('\n')
    {
        return format!("…{rest}");
    }
    text.to_string()
}

/// Something is already listening on the port.
async fn port_in_use(port: u16) -> bool {
    tokio::time::timeout(
        Duration::from_millis(300),
        tokio::net::TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    .is_ok_and(|connected| connected.is_ok())
}

impl AppSupervisor {
    pub fn new(
        hub: Arc<Hub>,
        log_dir: impl Into<PathBuf>,
        specs: Vec<AppSpec>,
        config: AppsConfig,
    ) -> Arc<Self> {
        Arc::new(Self {
            hub,
            log_dir: log_dir.into(),
            config,
            specs: specs.into_iter().map(|s| (s.env.clone(), s)).collect(),
            running: Mutex::default(),
            generation: AtomicU64::new(0),
            own: Mutex::default(),
        })
    }

    fn track(&self, pgid: u32) {
        procgroup::register(pgid);
        self.own.lock().expect("own lock poisoned").insert(pgid);
    }

    fn untrack(&self, pgid: u32) {
        forget(pgid);
        self.own.lock().expect("own lock poisoned").remove(&pgid);
    }

    pub fn has_apps(&self) -> bool {
        !self.specs.is_empty()
    }

    /// The commit each app with a process (or a crash retry pending) runs.
    pub fn live(&self) -> BTreeMap<String, String> {
        self.running
            .lock()
            .expect("running lock poisoned")
            .iter()
            .map(|(env, h)| (env.clone(), h.commit.clone()))
            .collect()
    }

    pub fn log_path(&self, env: &str) -> PathBuf {
        self.log_dir.join(format!("{env}.log"))
    }

    /// Start `env`'s app in `dir` on `commit`, stopping what ran before. Returns once
    /// the first attempt is recorded (`AppStarted`, or `AppCrashed`).
    pub async fn start(
        self: &Arc<Self>,
        env: &str,
        dir: PathBuf,
        commit: String,
        why: &str,
    ) -> Result<(), String> {
        let spec = self
            .specs
            .get(env)
            .cloned()
            .ok_or_else(|| format!("{env} has no app configured"))?;
        let cwd = match &spec.cwd {
            Some(sub) => dir.join(sub),
            None => dir,
        };
        if !cwd.is_dir() {
            return Err(format!("{} does not exist in the checkout", cwd.display()));
        }
        let restarted = self.halt(env).await;
        if restarted {
            self.record(Command::RecordAppRestarted {
                env: env.to_string(),
                reason: why.to_string(),
                attempt: 0,
            })
            .await;
        }
        let (stop_tx, stop_rx) = oneshot::channel();
        let (ready_tx, ready_rx) = oneshot::channel();
        let supervisor = Arc::clone(self);
        let run_commit = commit.clone();
        let generation = self.generation.fetch_add(1, Ordering::Relaxed);
        let join = chillax_core::task::spawn_logged("app-supervisor", async move {
            supervisor
                .supervise(spec, cwd, run_commit, generation, stop_rx, ready_tx)
                .await;
        });
        self.running.lock().expect("running lock poisoned").insert(
            env.to_string(),
            Handle {
                generation,
                commit,
                stop: stop_tx,
                join,
            },
        );
        let _ = ready_rx.await;
        Ok(())
    }

    /// End `env`'s app and wait until it is gone. The caller records `AppStopped`.
    pub async fn stop(&self, env: &str) {
        self.halt(env).await;
    }

    /// End every app (Chillax is quitting). Nothing is recorded: the next start finds the
    /// log saying "running" and no process, and starts them again.
    pub async fn shutdown(&self) {
        let envs: Vec<String> = self
            .running
            .lock()
            .expect("running lock poisoned")
            .keys()
            .cloned()
            .collect();
        for env in envs {
            self.halt(&env).await;
        }
    }

    /// Whether an app was running (or waiting to retry) and is now ended.
    async fn halt(&self, env: &str) -> bool {
        let handle = self
            .running
            .lock()
            .expect("running lock poisoned")
            .remove(env);
        let Some(handle) = handle else {
            return false;
        };
        let _ = handle.stop.send(());
        let _ = tokio::time::timeout(self.config.grace + Duration::from_secs(5), handle.join).await;
        true
    }

    async fn record(&self, cmd: Command) {
        if let Err(error) = self.hub.send(Actor::System, cmd).await {
            tracing::warn!(%error, "could not record an app event");
        }
    }

    /// Open the log for appending, moving a big one aside first.
    async fn open_log(&self, env: &str, header: &str) -> std::io::Result<std::fs::File> {
        tokio::fs::create_dir_all(&self.log_dir).await?;
        let path = self.log_path(env);
        if tokio::fs::metadata(&path)
            .await
            .is_ok_and(|m| m.len() > self.config.log_cap)
        {
            let _ = tokio::fs::rename(&path, self.log_dir.join(format!("{env}.log.1"))).await;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        use std::io::Write;
        writeln!(file, "--- chillax: {header} ---")?;
        Ok(file)
    }

    /// Spawn the app. The error is what to tell the stakeholder.
    async fn launch(&self, spec: &AppSpec, cwd: &Path, commit: &str) -> Result<Child, String> {
        if port_in_use(spec.port).await {
            return Err(format!(
                "port {} is already in use. Stop whatever listens there (an earlier run of \
                 this app, perhaps) or change `port` in [env.{}].",
                spec.port, spec.env
            ));
        }
        let command = spec.command.replace("{port}", &spec.port.to_string());
        let header = format!(
            "starting `{command}` at {} in {}",
            commit.get(..7).unwrap_or(commit),
            cwd.display()
        );
        let log = self
            .open_log(&spec.env, &header)
            .await
            .map_err(|e| format!("cannot write the log: {e}"))?;
        let err_log = log
            .try_clone()
            .map_err(|e| format!("cannot write the log: {e}"))?;
        let vars = self.config.env_files.vars(&spec.env).await;
        let mut process = Process::new("sh");
        process
            .args(["-c", &command])
            .current_dir(cwd)
            .envs(&vars)
            .env("PORT", spec.port.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(err_log))
            .kill_on_drop(true);
        #[cfg(unix)]
        process.process_group(0);
        let child = process
            .spawn()
            .map_err(|e| format!("could not run `{command}`: {e}"))?;
        if let Some(pid) = child.id() {
            self.track(pid);
        }
        Ok(child)
    }

    /// The life of one app: run it, restart it when it crashes, until told to stop or
    /// out of attempts.
    async fn supervise(
        self: Arc<Self>,
        spec: AppSpec,
        cwd: PathBuf,
        commit: String,
        generation: u64,
        mut stop: oneshot::Receiver<()>,
        ready: oneshot::Sender<()>,
    ) {
        let env = spec.env.clone();
        let mut ready = Some(ready);
        let mut attempt = 0u32;
        loop {
            let began = Instant::now();
            let (exit, tail) = match self.launch(&spec, &cwd, &commit).await {
                Err(why) => (None, why),
                Ok(mut child) => {
                    let pgid = child.id();
                    self.record(Command::RecordAppStarted {
                        env: env.clone(),
                        port: spec.port,
                        url: spec.url.clone(),
                        commit: commit.clone(),
                    })
                    .await;
                    if let Some(tx) = ready.take() {
                        let _ = tx.send(());
                    }
                    tokio::select! {
                        status = child.wait() => {
                            // Whatever the leader left behind goes too.
                            if let Some(pgid) = pgid {
                                signal_group(pgid, "KILL").await;
                                self.untrack(pgid);
                            }
                            let code = status.ok().and_then(|s| s.code());
                            (code, log_tail(&self.log_path(&env)).await)
                        }
                        _ = &mut stop => {
                            if let Some(pgid) = pgid {
                                terminate(pgid, &mut child, self.config.grace).await;
                                self.untrack(pgid);
                            }
                            return;
                        }
                    }
                }
            };
            if began.elapsed() >= self.config.stable_after {
                attempt = 0;
            }
            attempt += 1;
            let will_retry = attempt <= self.config.max_attempts;
            self.record(Command::RecordAppCrashed {
                env: env.clone(),
                exit,
                tail,
                will_retry,
                commit: commit.clone(),
            })
            .await;
            if let Some(tx) = ready.take() {
                let _ = tx.send(());
            }
            if !will_retry {
                // Out of attempts: no process, not live. Only if still ours.
                let mut running = self.running.lock().expect("running lock poisoned");
                if running
                    .get(&env)
                    .is_some_and(|h| h.generation == generation)
                {
                    running.remove(&env);
                }
                return;
            }
            let wait = self
                .config
                .backoff_base
                .saturating_mul(1 << (attempt - 1).min(10))
                .min(self.config.backoff_cap);
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = &mut stop => return,
            }
            self.record(Command::RecordAppRestarted {
                env: env.clone(),
                reason: format!("crashed; retry {attempt} of {}", self.config.max_attempts),
                attempt,
            })
            .await;
        }
    }
}

impl Drop for AppSupervisor {
    /// A supervisor that goes away takes its apps with it.
    fn drop(&mut self) {
        let own: Vec<u32> = self
            .own
            .lock()
            .map(|g| g.iter().copied().collect())
            .unwrap_or_default();
        for pgid in &own {
            signal_blocking(*pgid, "KILL");
            forget(*pgid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chillax_core::apps::AppStatus;
    use chillax_core::{AppInfo, Engine, EventKind, MemoryLog, RoleCatalog, Workflow};

    fn free_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("addr").port()
    }

    fn spec(env: &str, command: &str, port: u16) -> AppSpec {
        AppSpec {
            env: env.into(),
            command: command.into(),
            port,
            url: format!("http://localhost:{port}"),
            cwd: None,
        }
    }

    fn fast() -> AppsConfig {
        AppsConfig {
            grace: Duration::from_millis(300),
            backoff_base: Duration::from_millis(20),
            backoff_cap: Duration::from_millis(80),
            max_attempts: 2,
            stable_after: Duration::from_secs(60),
            log_cap: 1_000_000,
            env_files: EnvFiles::default(),
        }
    }

    struct Fixture {
        hub: Arc<Hub>,
        apps: Arc<AppSupervisor>,
        dir: tempfile::TempDir,
    }

    async fn fixture(specs: Vec<AppSpec>, config: AppsConfig) -> Fixture {
        let engine = Engine::new("t", RoleCatalog::new([]), Workflow::default());
        let hub = Arc::new(
            Hub::open(engine, Arc::new(MemoryLog::default()))
                .await
                .expect("hub"),
        );
        let infos = specs
            .iter()
            .map(|s| AppInfo {
                env: s.env.clone(),
                port: s.port,
                url: s.url.clone(),
            })
            .collect();
        hub.send(Actor::System, Command::ConfigureApps { apps: infos })
            .await
            .expect("configure");
        let dir = tempfile::tempdir().expect("tempdir");
        let apps = AppSupervisor::new(hub.clone(), dir.path().join("logs"), specs, config);
        Fixture { hub, apps, dir }
    }

    async fn until<F: FnMut() -> bool>(what: &str, mut f: F) {
        for _ in 0..200 {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("timed out waiting for {what}");
    }

    fn alive(pid: &str) -> bool {
        std::process::Command::new("kill")
            .args(["-0", pid])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_default()
    }

    async fn app_status(f: &Fixture, env: &str) -> AppStatus {
        f.hub.snapshot().await.app(env).expect("app").status
    }

    #[tokio::test]
    async fn an_app_starts_with_port_substitution_logs_and_is_recorded() {
        let port = free_port();
        let f = fixture(
            vec![spec("dev", "echo port={port} env=$PORT; sleep 30", port)],
            fast(),
        )
        .await;
        f.apps
            .start("dev", f.dir.path().into(), "abc1234".into(), "first")
            .await
            .expect("start");
        let state = f.hub.snapshot().await;
        let app = state.app("dev").expect("dev");
        assert_eq!(app.status, AppStatus::Running);
        assert_eq!(app.commit.as_deref(), Some("abc1234"));
        assert_eq!(
            f.apps.live().get("dev").map(String::as_str),
            Some("abc1234")
        );
        let log = f.apps.log_path("dev");
        until("output in the log", || {
            read(&log).contains(&format!("port={port} env={port}"))
        })
        .await;
        assert!(read(&log).contains("--- chillax: starting"));
        f.apps.shutdown().await;
    }

    #[tokio::test]
    async fn an_app_gets_its_environments_env_files_but_port_wins() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(root.path().join(".env"), "FOO=shared\nPORT=1\n").expect("write");
        std::fs::write(root.path().join(".env.qa"), "FOO=qa\n").expect("write");
        let port = free_port();
        let f = fixture(
            vec![spec("qa", "echo foo=$FOO port=$PORT; sleep 30", port)],
            AppsConfig {
                env_files: EnvFiles::new(root.path(), &chillax_core::config::EnvConfig::default()),
                ..fast()
            },
        )
        .await;
        f.apps
            .start("qa", f.dir.path().into(), "abc1234".into(), "first")
            .await
            .expect("start");
        let log = f.apps.log_path("qa");
        until("output in the log", || {
            read(&log).contains(&format!("foo=qa port={port}"))
        })
        .await;
        f.apps.shutdown().await;
    }

    #[tokio::test]
    async fn stopping_ends_the_whole_process_tree() {
        let port = free_port();
        let f = fixture(
            vec![spec("dev", "sleep 60 & echo $! > child.pid; wait", port)],
            fast(),
        )
        .await;
        f.apps
            .start("dev", f.dir.path().into(), "c1".into(), "first")
            .await
            .expect("start");
        let pidfile = f.dir.path().join("child.pid");
        until("the child pid", || !read(&pidfile).trim().is_empty()).await;
        let pid = read(&pidfile).trim().to_string();
        assert!(alive(&pid));
        f.apps.stop("dev").await;
        assert!(f.apps.live().is_empty());
        until("the grandchild to be gone", || !alive(&pid)).await;
    }

    #[tokio::test]
    async fn a_process_that_ignores_sigterm_is_killed_after_the_grace_period() {
        let port = free_port();
        let f = fixture(
            vec![spec(
                "dev",
                "trap '' TERM; sleep 60 & echo $! > child.pid; while true; do sleep 1; done",
                port,
            )],
            fast(),
        )
        .await;
        f.apps
            .start("dev", f.dir.path().into(), "c1".into(), "first")
            .await
            .expect("start");
        let pidfile = f.dir.path().join("child.pid");
        until("the child pid", || !read(&pidfile).trim().is_empty()).await;
        let pid = read(&pidfile).trim().to_string();
        let began = Instant::now();
        f.apps.stop("dev").await;
        assert!(began.elapsed() < Duration::from_secs(5));
        until("the stubborn tree to be gone", || !alive(&pid)).await;
    }

    #[tokio::test]
    async fn starting_again_on_new_code_restarts_and_says_why() {
        let port = free_port();
        let f = fixture(
            vec![spec("dev", "echo $$ >> runs.txt; sleep 60", port)],
            fast(),
        )
        .await;
        let mut rx = f.hub.subscribe();
        f.apps
            .start("dev", f.dir.path().into(), "c1".into(), "first")
            .await
            .expect("start");
        let runs_file = f.dir.path().join("runs.txt");
        until("the first run", || read(&runs_file).lines().count() == 1).await;
        f.apps
            .start("dev", f.dir.path().into(), "c2".into(), "dev moved")
            .await
            .expect("restart");
        assert_eq!(f.apps.live().get("dev").map(String::as_str), Some("c2"));
        until("both runs", || read(&runs_file).lines().count() == 2).await;
        let runs = read(&runs_file);
        let pids: Vec<&str> = runs.lines().collect();
        assert_eq!(pids.len(), 2, "{runs}");
        until("the first run to be gone", || !alive(pids[0])).await;
        assert!(alive(pids[1]));
        let mut kinds = Vec::new();
        while let Ok(event) = rx.try_recv() {
            kinds.push(event.kind);
        }
        assert!(
            kinds.iter().any(
                |k| matches!(k, EventKind::AppRestarted { reason, .. } if reason == "dev moved")
            ),
            "{kinds:?}"
        );
        assert_eq!(app_status(&f, "dev").await, AppStatus::Running);
        f.apps.shutdown().await;
    }

    #[tokio::test]
    async fn a_crashing_app_is_retried_with_backoff_then_given_up_on() {
        let port = free_port();
        let f = fixture(vec![spec("dev", "echo boom; exit 3", port)], fast()).await;
        let mut rx = f.hub.subscribe();
        f.apps
            .start("dev", f.dir.path().into(), "c1".into(), "first")
            .await
            .expect("start");
        until("the app to be given up on", || f.apps.live().is_empty()).await;
        let state = f.hub.snapshot().await;
        let app = state.app("dev").expect("dev");
        assert_eq!(app.status, AppStatus::Crashed);
        assert_eq!(app.reason.as_deref(), Some("exited with 3"));
        assert_eq!(app.restarts, 2);
        let mut crashes = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let EventKind::AppCrashed {
                exit,
                tail,
                will_retry,
                ..
            } = event.kind
            {
                crashes.push((exit, tail, will_retry));
            }
        }
        assert_eq!(crashes.len(), 3, "{crashes:?}");
        assert!(crashes[0].1.contains("boom"), "{crashes:?}");
        assert_eq!(crashes[0].0, Some(3));
        assert_eq!(
            crashes.iter().map(|c| c.2).collect::<Vec<_>>(),
            [true, true, false]
        );
    }

    #[tokio::test]
    async fn a_port_that_is_taken_is_reported_instead_of_started() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let f = fixture(vec![spec("dev", "sleep 60", port)], fast()).await;
        let mut rx = f.hub.subscribe();
        f.apps
            .start("dev", f.dir.path().into(), "c1".into(), "first")
            .await
            .expect("start");
        let mut tails = Vec::new();
        until("a crash report", || {
            while let Ok(event) = rx.try_recv() {
                if let EventKind::AppCrashed { tail, .. } = event.kind {
                    tails.push(tail);
                }
            }
            !tails.is_empty()
        })
        .await;
        assert!(tails[0].contains("already in use"), "{tails:?}");
        f.apps.shutdown().await;
        drop(listener);
    }

    #[tokio::test]
    async fn shutdown_ends_every_app_and_the_run_directory_is_respected() {
        let (a, b) = (free_port(), free_port());
        let f = fixture(
            vec![
                spec(
                    "dev",
                    "pwd > where.txt; sleep 60 & echo $! > dev.pid; wait",
                    a,
                ),
                spec("qa", "sleep 60 & echo $! > qa.pid; wait", b),
            ],
            fast(),
        )
        .await;
        std::fs::create_dir(f.dir.path().join("web")).expect("mkdir");
        let mut dev = spec(
            "dev",
            "pwd > where.txt; sleep 60 & echo $! > dev.pid; wait",
            a,
        );
        dev.cwd = Some("web".into());
        // A supervisor with a sub-directory for dev.
        let apps = AppSupervisor::new(
            f.hub.clone(),
            f.dir.path().join("logs"),
            vec![dev, spec("qa", "sleep 60 & echo $! > qa.pid; wait", b)],
            fast(),
        );
        apps.start("dev", f.dir.path().into(), "c".into(), "x")
            .await
            .expect("dev");
        apps.start("qa", f.dir.path().into(), "c".into(), "x")
            .await
            .expect("qa");
        let web = f.dir.path().join("web");
        until("pid files", || {
            !read(&web.join("dev.pid")).trim().is_empty()
                && !read(&f.dir.path().join("qa.pid")).trim().is_empty()
        })
        .await;
        assert!(read(&web.join("where.txt")).trim().ends_with("web"));
        let pids = [
            read(&web.join("dev.pid")).trim().to_string(),
            read(&f.dir.path().join("qa.pid")).trim().to_string(),
        ];
        apps.shutdown().await;
        assert!(apps.live().is_empty());
        for pid in &pids {
            until("a process to be gone", || !alive(pid)).await;
        }
    }

    #[tokio::test]
    async fn a_big_log_is_moved_aside_when_the_app_starts_again() {
        let port = free_port();
        let mut config = fast();
        config.log_cap = 50;
        let f = fixture(
            vec![spec(
                "dev",
                "echo 0123456789012345678901234567890123456789; sleep 60",
                port,
            )],
            config,
        )
        .await;
        for commit in ["c1", "c2", "c3"] {
            f.apps
                .start("dev", f.dir.path().into(), commit.into(), "again")
                .await
                .expect("start");
            let log = f.apps.log_path("dev");
            until("output", || read(&log).contains("0123456789")).await;
        }
        assert!(f.dir.path().join("logs/dev.log.1").exists());
        assert!(read(&f.apps.log_path("dev")).len() < 400);
        f.apps.shutdown().await;
    }
}
