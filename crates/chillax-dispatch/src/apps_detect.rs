//! Automatic detection of running apps. Agents' dev servers outlive their
//! sessions, and not every agent remembers `report_app`, so Chillax looks for listening
//! TCP ports whose process runs inside the repository or one of its worktrees, records
//! each new one as `AppReported` (reporter `chillax`), and records `down` when a reported
//! app stops answering. The OS lookup sits behind [`AppProbe`] so tests use a fake.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chillax_core::{Actor, Command, CompanyHandle, EventKind, Hub};
use tokio::process::Command as Process;
use tokio::task::JoinHandle;

/// A process listening on a TCP port, with the directory it runs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listener {
    pub port: u16,
    pub cwd: PathBuf,
}

/// How Chillax learns what listens and whether a port still answers.
#[async_trait]
pub trait AppProbe: Send + Sync {
    /// Every TCP listener whose working directory could be found. Empty when the OS tool
    /// is missing or fails.
    async fn listeners(&self) -> Vec<Listener>;
    /// Whether something accepts connections on `port` at localhost.
    async fn is_up(&self, port: u16) -> bool;
}

/// The probe for macOS and Linux: `lsof` for listeners, a TCP connect for liveness.
pub struct LsofProbe;

const LSOF_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(700);

async fn lsof(args: &[&str]) -> Option<String> {
    let run = Process::new("lsof")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(LSOF_TIMEOUT, run).await {
        // lsof exits 1 when nothing matches; the output is still what we want.
        Ok(Ok(out)) => Some(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(Err(error)) => {
            tracing::debug!(%error, "lsof is not available; apps are not detected");
            None
        }
        Err(_) => {
            tracing::debug!("lsof timed out");
            None
        }
    }
}

#[async_trait]
impl AppProbe for LsofProbe {
    async fn listeners(&self) -> Vec<Listener> {
        let Some(out) = lsof(&["-iTCP", "-sTCP:LISTEN", "-nP", "-Fpn"]).await else {
            return Vec::new();
        };
        let ports = parse_listen_ports(&out);
        if ports.is_empty() {
            return Vec::new();
        }
        let pids = ports
            .keys()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let Some(out) = lsof(&["-a", "-p", &pids, "-d", "cwd", "-Fn"]).await else {
            return Vec::new();
        };
        let cwds = parse_cwds(&out);
        let mut found = Vec::new();
        for (pid, ports) in ports {
            if let Some(cwd) = cwds.get(&pid) {
                for port in ports {
                    found.push(Listener {
                        port,
                        cwd: cwd.clone(),
                    });
                }
            }
        }
        found
    }

    async fn is_up(&self, port: u16) -> bool {
        matches!(
            tokio::time::timeout(
                CONNECT_TIMEOUT,
                tokio::net::TcpStream::connect(("localhost", port))
            )
            .await,
            Ok(Ok(_))
        )
    }
}

/// Parse `lsof -iTCP -sTCP:LISTEN -nP -Fpn`: `p<pid>` lines, each followed by the
/// `n<addr>:<port>` lines of that process. Returns the listening ports per pid.
pub fn parse_listen_ports(out: &str) -> BTreeMap<u32, BTreeSet<u16>> {
    let mut map: BTreeMap<u32, BTreeSet<u16>> = BTreeMap::new();
    let mut pid = None;
    for line in out.lines() {
        if let Some(p) = line.strip_prefix('p') {
            pid = p.trim().parse().ok();
        } else if let Some(name) = line.strip_prefix('n')
            && let Some(pid) = pid
            && let Some((_, port)) = name.rsplit_once(':')
            && let Ok(port) = port.trim().parse::<u16>()
        {
            map.entry(pid).or_default().insert(port);
        }
    }
    map
}

/// Parse `lsof -a -p PIDS -d cwd -Fn`: the working directory (`n/path`) per pid.
pub fn parse_cwds(out: &str) -> BTreeMap<u32, PathBuf> {
    let mut map = BTreeMap::new();
    let mut pid = None;
    for line in out.lines() {
        if let Some(p) = line.strip_prefix('p') {
            pid = p.trim().parse().ok();
        } else if let Some(path) = line.strip_prefix('n')
            && let Some(pid) = pid
            && path.starts_with('/')
        {
            map.insert(pid, PathBuf::from(path));
        }
    }
    map
}

/// The label for a listener running in `cwd`: the worktree directory name under
/// `.chillax/worktrees`, `_main` for the repository itself, `None` elsewhere.
pub fn label_for(root: &Path, cwd: &Path) -> Option<String> {
    let rel = cwd.strip_prefix(root).ok()?;
    let mut parts = rel.components().map(|c| c.as_os_str().to_string_lossy());
    if parts.next().as_deref() == Some(".chillax") {
        if parts.next().as_deref() == Some("worktrees") {
            return parts.next().map(|w| w.into_owned());
        }
        // Chillax's own state directory is not an app.
        return None;
    }
    Some("_main".to_string())
}

pub struct AppDetector {
    hub: Arc<Hub>,
    probe: Arc<dyn AppProbe>,
    root: PathBuf,
    /// Ports never reported: Chillax's own server.
    ignore: Vec<u16>,
}

impl AppDetector {
    pub fn new(hub: Arc<Hub>, probe: Arc<dyn AppProbe>, root: PathBuf, ignore: Vec<u16>) -> Self {
        // lsof reports canonical paths (/private/var on macOS).
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        Self {
            hub,
            probe,
            root,
            ignore,
        }
    }

    /// Check every `every` until the returned task is aborted.
    pub fn start(self, every: Duration) -> JoinHandle<()> {
        chillax_core::task::spawn_logged("app-detector", async move {
            let mut tick = tokio::time::interval(every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                self.tick().await;
            }
        })
    }

    /// One pass: report new listeners, then mark reported apps that stopped answering.
    pub async fn tick(&self) {
        let engine = self.hub.engine().await;
        let state = &engine.state;
        let managed: BTreeSet<u16> = state.apps.iter().map(|a| a.port).collect();
        let known: BTreeSet<u16> = state.reported_apps.iter().filter_map(|a| a.port).collect();

        for l in self.probe.listeners().await {
            if self.ignore.contains(&l.port) || managed.contains(&l.port) || known.contains(&l.port)
            {
                continue;
            }
            let Some(dir) = label_for(&self.root, &l.cwd) else {
                continue;
            };
            let cmd = Command::ReportApp {
                env: Some(format!("{dir} :{}", l.port)),
                url: None,
                port: Some(l.port),
                ticket: None,
                note: Some(format!("detected in {}", l.cwd.display())),
                down: false,
            };
            if let Err(error) = self.hub.send(Actor::System, cmd).await {
                tracing::warn!(%error, port = l.port, "could not record a detected app");
            }
        }

        for app in &state.reported_apps {
            let Some(port) = app.port else { continue };
            if self.probe.is_up(port).await {
                continue;
            }
            let kind = EventKind::AppReported {
                env: app.label.clone(),
                url: app.url.clone(),
                port: app.port,
                ticket: app.ticket.clone(),
                employee: app.employee.clone(),
                worktree: app.worktree.clone(),
                note: app.note.clone(),
                down: true,
            };
            if let Err(error) = self.hub.record(Actor::System, kind).await {
                tracing::warn!(%error, port, reporter = %app.employee, "could not mark an app down");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lsof_listener_output() {
        let out = "p101\nn*:4175\nn[::1]:4175\np202\nn127.0.0.1:3000\nn*:5173\n";
        let m = parse_listen_ports(out);
        assert_eq!(m[&101], BTreeSet::from([4175]));
        assert_eq!(m[&202], BTreeSet::from([3000, 5173]));
    }

    #[test]
    fn parses_lsof_cwd_output() {
        let out = "p101\nfcwd\nn/repo/.chillax/worktrees/_qa\np202\nfcwd\nn/repo\n";
        let m = parse_cwds(out);
        assert_eq!(m[&101], PathBuf::from("/repo/.chillax/worktrees/_qa"));
        assert_eq!(m[&202], PathBuf::from("/repo"));
    }

    #[test]
    fn labels_come_from_the_worktree_directory() {
        let root = Path::new("/repo");
        let l = |p: &str| label_for(root, Path::new(p));
        assert_eq!(l("/repo/.chillax/worktrees/_qa/web"), Some("_qa".into()));
        assert_eq!(l("/repo/.chillax/worktrees/T-4"), Some("T-4".into()));
        assert_eq!(l("/repo"), Some("_main".into()));
        assert_eq!(l("/repo/web"), Some("_main".into()));
        assert_eq!(l("/repo/.chillax/state"), None);
        assert_eq!(l("/other"), None);
    }

    use chillax_core::apps::DETECTED_BY;
    use chillax_core::{Engine, MemoryLog, RoleCatalog, Workflow};
    use std::sync::Mutex;

    #[derive(Default)]
    struct Fake {
        listeners: Mutex<Vec<Listener>>,
        up: Mutex<BTreeSet<u16>>,
    }

    #[async_trait]
    impl AppProbe for Fake {
        async fn listeners(&self) -> Vec<Listener> {
            self.listeners.lock().unwrap().clone()
        }
        async fn is_up(&self, port: u16) -> bool {
            self.up.lock().unwrap().contains(&port)
        }
    }

    async fn setup() -> (Arc<Hub>, Arc<Fake>, AppDetector) {
        let engine = Engine::new("t", RoleCatalog::new([]), Workflow::default());
        let hub = Arc::new(
            Hub::open(engine, Arc::new(MemoryLog::default()))
                .await
                .expect("hub"),
        );
        let fake = Arc::new(Fake::default());
        let detector = AppDetector::new(
            hub.clone(),
            fake.clone(),
            PathBuf::from("/nonexistent-repo"),
            vec![9000],
        );
        (hub, fake, detector)
    }

    fn listener(port: u16, cwd: &str) -> Listener {
        Listener {
            port,
            cwd: PathBuf::from(cwd),
        }
    }

    #[tokio::test]
    async fn a_new_listener_is_recorded_once_and_marked_down_when_it_goes() {
        let (hub, fake, detector) = setup().await;
        *fake.listeners.lock().unwrap() = vec![
            listener(4175, "/nonexistent-repo/.chillax/worktrees/_qa"),
            listener(9000, "/nonexistent-repo"),
            listener(5000, "/elsewhere"),
        ];
        fake.up.lock().unwrap().extend([4175, 9000, 5000]);
        detector.tick().await;
        detector.tick().await;
        let apps = hub.engine().await.state.reported_apps;
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].label, "_qa :4175");
        assert_eq!(apps[0].port, Some(4175));
        assert_eq!(apps[0].employee, DETECTED_BY);
        assert_eq!(hub.engine().await.state.last_seq, 1);

        fake.listeners.lock().unwrap().clear();
        fake.up.lock().unwrap().clear();
        detector.tick().await;
        assert!(hub.engine().await.state.reported_apps.is_empty());
    }
}
