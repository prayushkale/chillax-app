//! Running instances of Chillax (ADR 0017): who answers at a URL, the registry of
//! instances in `~/.chillax/instances`, and picking a port. All the IO for
//! `chillax_core::InstanceSource` lives here.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
use async_trait::async_trait;
use chillax_core::{InstanceInfo, InstanceSource};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;

/// The port tried first; with no `[server] port` the next 50 are tried after it.
pub const BASE_PORT: u16 = 4747;
/// How many ports above the base are tried.
pub const PORT_SPAN: u16 = 50;

const PROBE_TIMEOUT: Duration = Duration::from_millis(500);
const LIST_PROBE_TIMEOUT: Duration = Duration::from_millis(300);

/// The `project` part of `/api/v1/health`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ProjectId {
    pub name: String,
    pub root: String,
}

/// What `/api/v1/health` says. `project` is absent on servers older than ADR 0017.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Health {
    #[serde(default)]
    pub project: Option<ProjectId>,
}

/// Whose server answers at a URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    /// Nothing (that speaks Chillax) answers.
    Absent,
    /// The project we asked about.
    Same,
    /// Another project, or a server too old to say (`None`).
    Other(Option<ProjectId>),
}

/// Compare roots by their canonical spelling.
pub fn canonical(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// `Err(true)` when the connection was refused (nobody listens), `Err(false)` for any
/// other failure such as a timeout.
async fn probe_inner(url: &str, timeout: Duration) -> Result<Health, bool> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .no_proxy()
        .build()
        .map_err(|_| false)?;
    let resp = client
        .get(format!("{url}/api/v1/health"))
        .send()
        .await
        .map_err(|e| e.is_connect())?;
    if !resp.status().is_success() {
        return Err(false);
    }
    // A 200 that is not our JSON still means something answers; it just has no identity.
    Ok(resp
        .json::<Health>()
        .await
        .unwrap_or(Health { project: None }))
}

/// The health of the Chillax server at `url`, if one answers.
#[cfg_attr(not(test), allow(dead_code))]
pub async fn probe(url: &str) -> Option<Health> {
    probe_inner(url, PROBE_TIMEOUT).await.ok()
}

fn identity_of(health: Result<Health, bool>, root: &str) -> Identity {
    match health {
        Err(_) => Identity::Absent,
        Ok(Health { project: Some(p) }) if p.root == root => Identity::Same,
        Ok(Health { project }) => Identity::Other(project),
    }
}

/// Whether the server at `url` belongs to the project at `root`.
pub async fn identity(url: &str, root: &Path) -> Identity {
    identity_of(probe_inner(url, PROBE_TIMEOUT).await, &canonical(root))
}

/// The registry of running instances: one JSON file per project root in a directory
/// outside every repository. `dir` is `None` when there is no home to put it in.
#[derive(Debug, Clone)]
pub struct Registry {
    dir: Option<PathBuf>,
}

impl Registry {
    /// `$CHILLAX_HOME/instances`, else `~/.chillax/instances`. The only place that reads
    /// the environment.
    pub fn from_env() -> Self {
        let home = std::env::var_os("CHILLAX_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .or_else(|| std::env::var_os("USERPROFILE"))
                    .map(|h| PathBuf::from(h).join(".chillax"))
            });
        if home.is_none() {
            tracing::warn!("no home directory: the instance registry is disabled");
        }
        Self {
            dir: home.map(|h| h.join("instances")),
        }
    }

    /// A registry in `dir` itself (tests).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: Some(dir.into()),
        }
    }

    /// A registry that remembers nothing.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn disabled() -> Self {
        Self { dir: None }
    }

    /// The file for the project at `root`: the first 16 hex digits of the SHA-256 of its
    /// canonical path.
    pub fn path_for(&self, root: &str) -> Option<PathBuf> {
        let digest = Sha256::digest(root.as_bytes());
        let name: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
        self.dir.as_ref().map(|d| d.join(format!("{name}.json")))
    }

    /// Write (replace) the entry for `info.root`, atomically.
    pub fn register(&self, info: &InstanceInfo) -> Result<()> {
        let (Some(dir), Some(path)) = (&self.dir, self.path_for(&info.root)) else {
            bail!("no home directory for the instance registry");
        };
        create_private_dir(dir)?;
        let mut file = tempfile::NamedTempFile::new_in(dir)?;
        std::io::Write::write_all(&mut file, &serde_json::to_vec_pretty(info)?)?;
        file.persist(&path)?;
        Ok(())
    }

    /// Remove our entry; leaves it alone when another process has taken it over.
    pub fn unregister(&self, info: &InstanceInfo) {
        let Some(path) = self.path_for(&info.root) else {
            return;
        };
        let Some(current) = read_entry(&path) else {
            return;
        };
        if current.url == info.url && current.pid == info.pid {
            let _ = std::fs::remove_file(path);
        }
    }

    /// The entry registered for `root`, alive or not.
    pub fn entry_for(&self, root: &str) -> Option<InstanceInfo> {
        read_entry(&self.path_for(root)?)
    }

    fn entries(&self) -> Vec<(PathBuf, InstanceInfo)> {
        let Some(dir) = &self.dir else {
            return Vec::new();
        };
        let Ok(read) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        read.filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .filter_map(|p| read_entry(&p).map(|i| (p, i)))
            .collect()
    }

    /// Whether registering can work at all (a home exists).
    pub fn is_enabled(&self) -> bool {
        self.dir.is_some()
    }
}

fn read_entry(path: &Path) -> Option<InstanceInfo> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn create_private_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)?;
    Ok(())
}

#[async_trait]
impl InstanceSource for Registry {
    /// Live instances only: each entry's URL must answer as that entry's project. Entries
    /// whose server is gone or belongs to someone else are deleted (a crash leaves them).
    async fn list(&self) -> Vec<InstanceInfo> {
        let mut probes = tokio::task::JoinSet::new();
        for (path, info) in self.entries() {
            probes.spawn(async move {
                let health = probe_inner(&info.url, LIST_PROBE_TIMEOUT).await;
                // A timeout may just be a busy server; only prune what is surely dead.
                let prune = matches!(health, Err(true))
                    || matches!(&health, Ok(h) if h.project.as_ref().is_none_or(|p| p.root != info.root));
                let alive = matches!(identity_of(health, &info.root), Identity::Same);
                (path, info, alive, prune)
            });
        }
        let mut live = Vec::new();
        while let Some(Ok((path, info, alive, prune))) = probes.join_next().await {
            if alive {
                live.push(info);
            } else if prune {
                let _ = std::fs::remove_file(path);
            }
        }
        live.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.root.cmp(&b.root)));
        live
    }
}

/// The URL of the running instance of the project at `root`: its registry entry when that
/// answers as the project, else `configured` (the `[server]` address) when that does.
pub async fn find(registry: &Registry, root: &Path, configured: &str) -> Option<String> {
    let canon = canonical(root);
    if let Some(entry) = registry.entry_for(&canon)
        && identity_of(probe_inner(&entry.url, PROBE_TIMEOUT).await, &canon) == Identity::Same
    {
        return Some(entry.url);
    }
    (identity_of(probe_inner(configured, PROBE_TIMEOUT).await, &canon) == Identity::Same)
        .then(|| configured.to_string())
}

/// Describe who holds a port, for error messages.
pub fn describe_holder(identity: &Identity) -> String {
    match identity {
        Identity::Other(Some(p)) => format!("project {} ({})", p.name, p.root),
        Identity::Other(None) => "another Chillax".into(),
        Identity::Same => "this project".into(),
        Identity::Absent => "another program".into(),
    }
}

/// Bind the server's listener. With an explicit port only that port is tried; otherwise
/// `base`, then up to [`PORT_SPAN`] ports above it. Binding is the check, so two
/// processes starting at once cannot pick the same port.
pub async fn bind_server(
    bind: &str,
    base: u16,
    explicit: bool,
    root: &Path,
    host_for_url: &str,
) -> Result<TcpListener> {
    if explicit {
        return match TcpListener::bind((bind, base)).await {
            Ok(l) => Ok(l),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                let who = identity(&format!("http://{host_for_url}:{base}"), root).await;
                bail!(
                    "port {base} is in use by {}; pick another [server] port in \
                     .chillax/chillax.toml (or remove it to use the next free one)",
                    describe_holder(&who)
                )
            }
            Err(e) => bail!("cannot listen on {bind}:{base}: {e}"),
        };
    }
    for port in base..=base.saturating_add(PORT_SPAN) {
        match TcpListener::bind((bind, port)).await {
            Ok(l) => return Ok(l),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(e) => bail!("cannot listen on {bind}:{port}: {e}"),
        }
    }
    bail!(
        "no free port between {base} and {}; set [server] port in .chillax/chillax.toml",
        base.saturating_add(PORT_SPAN)
    )
}

/// What `chillax projects` prints: one block per instance, the current project marked.
pub fn render_projects(list: &[InstanceInfo], current: Option<&str>) -> String {
    if list.is_empty() {
        return "No Chillax instances are running.\n".into();
    }
    let mut out = String::new();
    for i in list {
        let mark = if current == Some(i.root.as_str()) {
            "*"
        } else {
            " "
        };
        out.push_str(&format!(
            "{mark} {}  {}\n    {}  (pid {}, started {})\n",
            i.name, i.url, i.root, i.pid, i.started_at
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::routing::get;
    use serde_json::json;

    use super::*;

    fn info(root: &str, name: &str, url: &str) -> InstanceInfo {
        InstanceInfo {
            root: root.into(),
            name: name.into(),
            url: url.into(),
            pid: std::process::id(),
            started_at: "2026-10-24T10:00:00Z".into(),
        }
    }

    /// A fake Chillax whose health says `body`; returns its URL.
    pub(crate) async fn serve_health(body: serde_json::Value) -> String {
        let app = Router::new().route(
            "/api/v1/health",
            get(move || {
                let body = body.clone();
                async move { axum::Json(body) }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move { axum::serve(listener, app).await });
        url
    }

    fn health_of(name: &str, root: &Path) -> serde_json::Value {
        json!({"status":"ok","version":"x","api":"v1",
               "project":{"name":name,"root":canonical(root)}})
    }

    #[tokio::test]
    async fn identity_tells_same_other_and_absent_apart() {
        let a = tempfile::tempdir().expect("a");
        let b = tempfile::tempdir().expect("b");
        let url = serve_health(health_of("A", a.path())).await;
        assert_eq!(identity(&url, a.path()).await, Identity::Same);
        match identity(&url, b.path()).await {
            Identity::Other(Some(p)) => assert_eq!(p.name, "A"),
            other => panic!("{other:?}"),
        }
        // An old server without `project` is never "same".
        let old = serve_health(json!({"status":"ok"})).await;
        assert_eq!(identity(&old, a.path()).await, Identity::Other(None));
        // Nothing listens on a port that was just freed.
        let free = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let dead = format!("http://{}", free.local_addr().expect("addr"));
        drop(free);
        assert_eq!(identity(&dead, a.path()).await, Identity::Absent);
        assert!(probe(&dead).await.is_none());
        assert!(probe(&url).await.is_some());
    }

    #[test]
    fn registry_writes_atomically_replaces_and_unregisters_only_its_own() {
        let home = tempfile::tempdir().expect("home");
        let reg = Registry::at(home.path().join("instances"));
        let mine = info("/work/a", "A", "http://127.0.0.1:4747");
        reg.register(&mine).expect("register");
        assert_eq!(reg.entry_for("/work/a"), Some(mine.clone()));
        let path = reg.path_for("/work/a").expect("path");
        assert_eq!(path.file_stem().expect("stem").len(), 16);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path.parent().expect("dir"))
                .expect("meta")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700);
        }

        // A later instance of the same project replaces the entry; no temp files remain.
        let mut newer = mine.clone();
        newer.url = "http://127.0.0.1:4748".into();
        newer.pid += 1;
        reg.register(&newer).expect("overwrite");
        assert_eq!(reg.entry_for("/work/a"), Some(newer.clone()));
        assert_eq!(reg.entries().len(), 1);
        assert_eq!(
            std::fs::read_dir(path.parent().expect("dir"))
                .expect("dir")
                .count(),
            1
        );

        // The old process quitting must not delete the new process's entry.
        reg.unregister(&mine);
        assert_eq!(reg.entry_for("/work/a"), Some(newer.clone()));
        reg.unregister(&newer);
        assert_eq!(reg.entry_for("/work/a"), None);
    }

    #[test]
    fn a_registry_without_a_home_is_silent() {
        let reg = Registry::disabled();
        assert!(reg.register(&info("/x", "x", "http://h")).is_err());
        assert!(reg.entry_for("/x").is_none());
        reg.unregister(&info("/x", "x", "http://h"));
    }

    #[tokio::test]
    async fn list_keeps_live_entries_sorted_and_prunes_dead_ones() {
        let home = tempfile::tempdir().expect("home");
        let reg = Registry::at(home.path().join("instances"));
        let a = tempfile::tempdir().expect("a");
        let z = tempfile::tempdir().expect("z");
        let url_z = serve_health(health_of("Zed", z.path())).await;
        let url_a = serve_health(health_of("Alpha", a.path())).await;
        reg.register(&info(&canonical(z.path()), "Zed", &url_z))
            .expect("z");
        reg.register(&info(&canonical(a.path()), "Alpha", &url_a))
            .expect("a");
        // Dead: nothing listens.
        let free = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let dead_url = format!("http://{}", free.local_addr().expect("addr"));
        drop(free);
        reg.register(&info("/gone", "Gone", &dead_url))
            .expect("gone");
        // Hijacked: the URL now serves a different project.
        let other = tempfile::tempdir().expect("other");
        let url_o = serve_health(health_of("Other", other.path())).await;
        reg.register(&info("/moved", "Moved", &url_o))
            .expect("moved");

        let names: Vec<_> = reg.list().await.into_iter().map(|i| i.name).collect();
        assert_eq!(names, ["Alpha", "Zed"]);
        assert_eq!(reg.entries().len(), 2, "dead entries are deleted");
    }

    #[tokio::test]
    async fn find_uses_the_registry_then_the_configured_url_and_checks_identity() {
        let home = tempfile::tempdir().expect("home");
        let reg = Registry::at(home.path().join("instances"));
        let a = tempfile::tempdir().expect("a");
        let b = tempfile::tempdir().expect("b");
        let url_a = serve_health(health_of("A", a.path())).await;
        let url_b = serve_health(health_of("B", b.path())).await;

        // The configured URL belongs to B: A does not find it.
        assert_eq!(find(&reg, a.path(), &url_b).await, None);
        // Without a registry entry A finds its own server on the configured URL.
        assert_eq!(find(&reg, a.path(), &url_a).await, Some(url_a.clone()));
        // The registry wins when it answers as A.
        reg.register(&info(&canonical(a.path()), "A", &url_a))
            .expect("register");
        assert_eq!(find(&reg, a.path(), &url_b).await, Some(url_a.clone()));
    }

    #[tokio::test]
    async fn without_an_explicit_port_the_next_free_one_is_used() {
        let root = tempfile::tempdir().expect("root");
        let first = bind_server("127.0.0.1", 0, false, root.path(), "127.0.0.1")
            .await
            .expect("first");
        let taken = first.local_addr().expect("addr").port();
        let second = bind_server("127.0.0.1", taken, false, root.path(), "127.0.0.1")
            .await
            .expect("second");
        let got = second.local_addr().expect("addr").port();
        assert!(
            got > taken && got <= taken.saturating_add(PORT_SPAN),
            "{got} vs {taken}"
        );
    }

    #[tokio::test]
    async fn an_explicit_port_that_is_taken_names_the_holder() {
        let a = tempfile::tempdir().expect("a");
        let b = tempfile::tempdir().expect("b");
        // A real "other project" server on a known port.
        let app = Router::new().route(
            "/api/v1/health",
            get({
                let body = health_of("A", a.path());
                move || {
                    let body = body.clone();
                    async move { axum::Json(body) }
                }
            }),
        );
        let l = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = l.local_addr().expect("addr").port();
        tokio::spawn(async move { axum::serve(l, app).await });
        let err = bind_server("127.0.0.1", port, true, b.path(), "127.0.0.1")
            .await
            .expect_err("taken");
        let msg = format!("{err:#}");
        assert!(
            msg.contains(&format!("port {port} is in use by project A")),
            "{msg}"
        );
        assert!(msg.contains("pick another [server] port"), "{msg}");

        // A plain listener that is not Chillax gets the generic wording.
        let plain = TcpListener::bind("127.0.0.1:0").await.expect("plain");
        let port = plain.local_addr().expect("addr").port();
        let err = bind_server("127.0.0.1", port, true, b.path(), "127.0.0.1")
            .await
            .expect_err("taken");
        assert!(format!("{err:#}").contains("in use by another program"));
    }

    #[test]
    fn projects_render_marks_the_current_one() {
        let list = vec![
            info("/work/a", "A", "http://127.0.0.1:4747"),
            info("/work/b", "B", "http://127.0.0.1:4748"),
        ];
        let out = render_projects(&list, Some("/work/b"));
        assert!(out.contains("  A  http://127.0.0.1:4747"), "{out}");
        assert!(out.contains("* B  http://127.0.0.1:4748"), "{out}");
        assert!(out.contains("/work/b"), "{out}");
        assert!(render_projects(&[], None).contains("No Chillax instances"));
    }
}
