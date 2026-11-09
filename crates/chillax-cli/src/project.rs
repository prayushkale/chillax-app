//! Locating and loading a project's `.chillax/` directory.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command as Process;

use anyhow::{Context, Result, bail};
use chillax_core::config::AgentConfig;
use chillax_core::{Config, Role, RoleCatalog, Workflow};
use rust_embed::RustEmbed;

/// The built-in role catalog, embedded into the binary.
#[derive(RustEmbed)]
#[folder = "../../roles/"]
struct BuiltinRoles;

pub struct Project {
    pub root: PathBuf,
    pub config: Config,
    pub roles: RoleCatalog,
    pub workflow: Workflow,
}

impl Project {
    pub fn dir(root: &Path) -> PathBuf {
        root.join(".chillax")
    }

    pub fn state_dir(&self) -> PathBuf {
        Self::dir(&self.root).join("state")
    }

    /// Find the project from `start`: the git top level that contains `.chillax/`.
    /// Commands should call `ensure_project` instead, which sets a new folder up first.
    ///
    /// A linked worktree resolves to the main worktree: init commits `.chillax/`, so every
    /// checkout (an agent's worktree, say) holds its own copy, but state belongs in one place.
    pub fn discover(start: &Path) -> Result<Self> {
        let root = main_worktree_root(start)?;
        if !Self::dir(&root).join("chillax.toml").is_file() {
            bail!(
                "no .chillax/chillax.toml in {}; run `chillax init` first",
                root.display()
            );
        }
        Self::load(&root)
    }

    pub fn load(root: &Path) -> Result<Self> {
        let dir = Self::dir(root);
        let config_path = dir.join("chillax.toml");
        let config = Config::from_toml(
            &std::fs::read_to_string(&config_path)
                .with_context(|| format!("reading {}", config_path.display()))?,
        )
        .with_context(|| format!("parsing {}", config_path.display()))?;

        let workflow_path = dir.join("workflow.yaml");
        let workflow = if workflow_path.is_file() {
            Workflow::from_yaml(&std::fs::read_to_string(&workflow_path)?)
                .with_context(|| format!("parsing {}", workflow_path.display()))?
        } else {
            Workflow::default()
        };

        Ok(Self {
            root: root.to_path_buf(),
            roles: load_roles(&dir.join("roles"))?,
            config,
            workflow,
        })
    }
}

/// Add an `[agents.<name>]` table to `.chillax/chillax.toml`. Appends rather than
/// rewriting so the stakeholder's comments and layout survive.
pub fn append_agent(root: &Path, name: &str, agent: &AgentConfig) -> Result<()> {
    let path = Project::dir(root).join("chillax.toml");
    let current =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let config =
        Config::from_toml(&current).with_context(|| format!("parsing {}", path.display()))?;
    if config.agents.contains_key(name) {
        return Ok(());
    }
    // JSON string escapes are valid TOML basic strings.
    let quote = |s: &str| serde_json::to_string(s).expect("strings serialize");
    let mut block = format!("\n[agents.{name}]\ncommand = {}\n", quote(&agent.command));
    if !agent.args.is_empty() {
        let args: Vec<String> = agent.args.iter().map(|a| quote(a)).collect();
        block.push_str(&format!("args = [{}]\n", args.join(", ")));
    }
    let mut updated = current;
    if !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&block);
    Config::from_toml(&updated).context("the new [agents] table did not parse")?;
    std::fs::write(&path, updated).with_context(|| format!("writing {}", path.display()))
}

/// Built-in roles, then `.chillax/roles/*.md` (same id overrides, new ids add).
pub fn load_roles(overrides: &Path) -> Result<RoleCatalog> {
    let mut catalog = RoleCatalog::default();
    for name in BuiltinRoles::iter().filter(|n| n.ends_with(".md")) {
        let file = BuiltinRoles::get(&name).expect("embedded file listed by iter exists");
        let source = std::str::from_utf8(&file.data).context("built-in role is not UTF-8")?;
        catalog.insert(Role::parse(source).with_context(|| format!("built-in role {name}"))?);
    }
    if overrides.is_dir() {
        let mut paths: Vec<_> = std::fs::read_dir(overrides)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "md"))
            .collect();
        paths.sort();
        for path in paths {
            let source = std::fs::read_to_string(&path)?;
            catalog.insert(
                Role::parse(&source).with_context(|| format!("parsing {}", path.display()))?,
            );
        }
    }
    Ok(catalog)
}

pub fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Process::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .context("running git (is it installed?)")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn git_root(start: &Path) -> Result<PathBuf> {
    let root = git(start, &["rev-parse", "--show-toplevel"])
        .context("chillax runs inside a git repository (run `chillax init --git` to create one)")?;
    Ok(PathBuf::from(root))
}

/// Load the project for `cwd`, setting the folder up first when it has no `.chillax/`
/// (ADR 0020). Every command except `init`, `projects` and `herdr status|start` goes
/// through here. Prints one notice line to stderr when it had to set something up.
pub fn ensure_project(cwd: &Path) -> Result<Project> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    ensure_project_with(cwd, None, home.as_deref(), &mut std::io::stderr())
}

/// `identity`, `home` and `out` are injected so tests need no git config, real home or
/// terminal.
pub(crate) fn ensure_project_with(
    cwd: &Path,
    identity: Option<(&str, &str)>,
    home: Option<&Path>,
    out: &mut impl Write,
) -> Result<Project> {
    if let Ok(project) = Project::discover(cwd) {
        return Ok(project);
    }
    let in_repo = git_root(cwd).is_ok();
    let root = if in_repo {
        main_worktree_root(cwd)?
    } else {
        cwd.to_path_buf()
    };
    // A linked worktree (an agent's checkout) belongs to the main project.
    if in_repo && let Ok(project) = Project::discover(&root) {
        return Ok(project);
    }
    check_autoinit_allowed(&root, in_repo, home)?;
    let root = root.canonicalize().unwrap_or(root);
    init_with(&root, !in_repo, identity)?;
    let notice = if !in_repo {
        format!(
            "Set up Chillax in {}: created a git repository (main) with a first commit, \
             .chillax/, and dev/qa branches.",
            root.display()
        )
    } else if cwd.canonicalize().ok().as_deref() != Some(root.as_path()) {
        format!(
            "Set up Chillax at the repository root {} (.chillax/, dev/qa branches).",
            root.display()
        )
    } else {
        format!(
            "Set up Chillax in {} (.chillax/, dev/qa branches).",
            root.display()
        )
    };
    writeln!(out, "{notice}").ok();
    Project::discover(&root)
}

/// The top level of the main worktree: for a linked worktree the parent of the common
/// git dir, otherwise `--show-toplevel`.
fn main_worktree_root(cwd: &Path) -> Result<PathBuf> {
    let top = git_root(cwd)?;
    let dirs = git(
        cwd,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
            "--git-dir",
        ],
    )?;
    let mut lines = dirs.lines();
    if let (Some(common), Some(own)) = (lines.next(), lines.next())
        && common != own
        && let Some(parent) = Path::new(common).parent()
    {
        return Ok(parent.to_path_buf());
    }
    Ok(top)
}

/// Refuse to set up the home directory, `/`, or (outside git) a folder that contains the
/// home directory: those are almost never meant to be a project. Only auto-setup calls
/// this; an explicit `chillax init` is deliberate.
fn check_autoinit_allowed(root: &Path, in_repo: bool, home: Option<&Path>) -> Result<()> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let home = home.map(|h| h.canonicalize().unwrap_or_else(|_| h.to_path_buf()));
    let bad = root.parent().is_none()
        || home
            .as_ref()
            .is_some_and(|h| root == *h || (!in_repo && h.starts_with(&root)));
    if bad {
        bail!(
            "refusing to set up Chillax in {}: it looks like your home or system root. \
             cd into a project folder, or run `chillax init` there on purpose.",
            root.display()
        );
    }
    Ok(())
}

/// The URL of `remote`, if the repository has it.
pub fn remote_url(root: &Path, remote: &str) -> Option<String> {
    git(root, &["remote", "get-url", remote])
        .ok()
        .filter(|url| !url.is_empty())
}

/// Whether a git remote URL points at github.com (https, ssh or scp-like).
pub fn is_github_url(url: &str) -> bool {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .or_else(|| url.strip_prefix("ssh://"))
        .unwrap_or(url);
    // Drop credentials (`git@`, `user:token@`), then the host ends at `/` or `:`.
    let rest = rest.rsplit_once('@').map_or(rest, |(_, host)| host);
    let host = rest.split(['/', ':']).next().unwrap_or("");
    host.eq_ignore_ascii_case("github.com")
}

/// `gh auth status` succeeds. Err when `gh` is not installed.
pub fn gh_authenticated() -> Result<bool> {
    let out = Process::new("gh")
        .args(["auth", "status"])
        .output()
        .context("running gh (install the GitHub CLI: https://cli.github.com)")?;
    Ok(out.status.success())
}

/// `chillax init`: create `.chillax/`, gitignore entries, and `dev`/`qa` branches.
pub fn init(start: &Path) -> Result<Vec<String>> {
    init_with(start, false, None)
}

/// `chillax init --git`: also create the git repository (and its first commit) if
/// `start` is not inside one.
pub fn init_with_git(start: &Path) -> Result<Vec<String>> {
    init_with(start, true, None)
}

const FIRST_COMMIT_HINT: &str = "git needs user.name and user.email to make the first commit; \
     run `git config user.name ...` and `git config user.email ...`, then re-run";

/// `identity` is for tests only: it supplies the commit author so they do not depend on
/// the machine's git config. Chillax never sets the user's identity.
fn init_with(
    start: &Path,
    create_git: bool,
    identity: Option<(&str, &str)>,
) -> Result<Vec<String>> {
    let mut done = Vec::new();
    if create_git && git_root(start).is_err() {
        git(start, &["init", "-q", "-b", "main"])?;
        done.push("created a git repository on branch main".into());
    }
    let root = git_root(start)
        .context("this folder is not a git repository; run `chillax init --git` to create one")?;
    let unborn = git(&root, &["rev-parse", "--verify", "HEAD"]).is_err();
    // A user's own uncommitted `.gitignore` edits must not end up in our commit.
    let gitignore_was_dirty = !unborn
        && git(&root, &["status", "--porcelain", "--", ".gitignore"])
            .map(|out| !out.is_empty())
            .unwrap_or(false);
    let dir = Project::dir(&root);
    // Paths (relative to the root) that init itself created or changed.
    let mut touched: Vec<String> = Vec::new();

    for sub in ["roles", "themes", "state", "worktrees"] {
        std::fs::create_dir_all(dir.join(sub))?;
    }
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "my-company".into());
    for (rel, contents) in [
        (".chillax/chillax.toml", Config::starter(&name)),
        (
            ".chillax/workflow.yaml",
            chillax_core::workflow::DEFAULT_WORKFLOW_YAML.to_string(),
        ),
        (
            ".chillax/roles/README.txt",
            "Put project roles here as <id>.md (YAML frontmatter + prompt).\n\
             A file with the same id as a built-in role overrides it; see `chillax roles`.\n"
                .to_string(),
        ),
    ] {
        if write_new(&root.join(rel), &contents, &mut done)? {
            touched.push(rel.into());
        }
    }
    let gitignore_changed = ensure_gitignore(&root, &mut done)?;

    if unborn {
        // One first commit holds the project files and the ignore entries, never runtime state.
        git(&root, &["add", "-A"])?;
        let mut args = identity_args(identity);
        args.extend(["commit", "-q", "--allow-empty", "-m", "Initial commit"].map(String::from));
        git_owned(&root, &args).context(FIRST_COMMIT_HINT)?;
        done.push("made the first commit".into());
        touched.clear();
    } else if gitignore_changed {
        if gitignore_was_dirty {
            done.push(
                ".gitignore had uncommitted changes, so Chillax's entries were left out of its \
                 commit; commit .gitignore yourself"
                    .into(),
            );
        } else {
            touched.push(".gitignore".into());
        }
    }

    // The branches the config names; a project that renamed them gets those. They are cut
    // from the prod branch, the same way `LocalGit::ensure_branches` does it.
    let env = std::fs::read_to_string(dir.join("chillax.toml"))
        .ok()
        .and_then(|text| Config::from_toml(&text).ok())
        .map(|c| c.env)
        .unwrap_or_default();
    let prod = env.prod.branch.as_str();
    if !touched.is_empty() {
        commit_setup(&root, prod, &touched, identity, &mut done);
    }
    let source = git(&root, &["rev-parse", &format!("refs/heads/{prod}")])
        .or_else(|_| git(&root, &["rev-parse", "HEAD"]))?;
    for branch in [env.dev.base_branch.as_str(), env.qa.branch.as_str()] {
        let exists = git(
            &root,
            &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
        )
        .is_ok();
        if !exists && branch != prod {
            git(&root, &["branch", branch, &source])?;
            done.push(format!("created branch {branch}"));
        }
    }
    if let Some(url) = remote_url(&root, "origin")
        && is_github_url(&url)
    {
        done.push(format!(
            "found a GitHub remote ({url}): to open a pull request per ticket, set \
             `kind = \"github\"` under [vcs] in .chillax/chillax.toml (needs `gh auth login`; \
             `chillax doctor` checks it)"
        ));
    }
    Ok(done)
}

/// Returns whether the file was created or changed.
fn ensure_gitignore(root: &Path, done: &mut Vec<String>) -> Result<bool> {
    let gitignore = root.join(".gitignore");
    let existing = std::fs::read_to_string(&gitignore).unwrap_or_default();
    let missing: Vec<_> = [".chillax/state/", ".chillax/worktrees/"]
        .into_iter()
        .filter(|entry| !existing.lines().any(|l| l.trim() == *entry))
        .collect();
    if !missing.is_empty() {
        let mut text = existing.clone();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("\n# Chillax runtime state\n");
        for entry in &missing {
            text.push_str(entry);
            text.push('\n');
        }
        std::fs::write(&gitignore, text)?;
        done.push(format!("added {} to .gitignore", missing.join(", ")));
        return Ok(true);
    }
    Ok(false)
}

/// Returns whether the file was created.
fn write_new(path: &Path, contents: &str, done: &mut Vec<String>) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
    done.push(format!("created {}", path.display()));
    Ok(true)
}

fn identity_args(identity: Option<(&str, &str)>) -> Vec<String> {
    match identity {
        Some((n, e)) => vec![
            "-c".into(),
            format!("user.name={n}"),
            "-c".into(),
            format!("user.email={e}"),
        ],
        None => Vec::new(),
    }
}

fn git_owned(dir: &Path, args: &[String]) -> Result<String> {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    git(dir, &refs)
}

/// Commit the files init wrote so the prod branch stays clean (a dirty prod checkout blocks
/// releases) and `dev`/`qa` inherit the config. The pathspec keeps anything the user has
/// staged out of the commit. Problems become notes, never errors; Chillax never sets an identity.
fn commit_setup(
    root: &Path,
    prod: &str,
    paths: &[String],
    identity: Option<(&str, &str)>,
    done: &mut Vec<String>,
) {
    let on_prod = git(root, &["symbolic-ref", "--short", "-q", "HEAD"]).is_ok_and(|b| b == prod);
    if !on_prod {
        done.push(format!(
            "HEAD is not on {prod}, so the files Chillax wrote ({}) are uncommitted; commit them \
             on {prod}, or releases will be blocked",
            paths.join(", ")
        ));
        return;
    }
    let mut add = vec!["add".to_string(), "--".to_string()];
    add.extend(paths.iter().cloned());
    let mut commit = identity_args(identity);
    commit.extend(["commit", "-q", "-m", "chore: set up Chillax", "--"].map(String::from));
    commit.extend(paths.iter().cloned());
    match git_owned(root, &add).and_then(|_| git_owned(root, &commit)) {
        Ok(_) => done.push(format!("committed {} on {prod}", paths.join(", "))),
        Err(e) => done.push(format!(
            "could not commit the files Chillax wrote ({e:#}); commit them yourself on {prod}, \
             or releases will be blocked"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_remotes_are_recognised_in_every_url_shape() {
        for url in [
            "https://github.com/acme/shop.git",
            "git@github.com:acme/shop.git",
            "ssh://git@github.com/acme/shop.git",
            "https://user:token@github.com/acme/shop",
        ] {
            assert!(is_github_url(url), "{url}");
        }
        for url in [
            "https://gitlab.com/acme/shop.git",
            "git@example.com:github.com/shop.git",
            "https://github.com.evil.example/acme/shop",
            "/srv/git/shop.git",
        ] {
            assert!(!is_github_url(url), "{url}");
        }
    }

    const TEST_IDENTITY: (&str, &str) = ("Test", "test@example.com");

    #[test]
    fn init_with_git_creates_repo_commit_and_branches() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("hello.txt"), "hi").expect("write");
        init_with(dir.path(), true, Some(TEST_IDENTITY)).expect("init --git");
        let root = dir.path();
        assert_eq!(
            git(root, &["rev-parse", "--abbrev-ref", "HEAD"]).expect("head"),
            "main"
        );
        assert!(git(root, &["rev-parse", "--verify", "HEAD"]).is_ok());
        assert!(
            git(root, &["ls-files", "hello.txt"])
                .expect("ls")
                .contains("hello.txt")
        );
        for branch in ["dev", "qa"] {
            assert!(
                git(
                    root,
                    &["rev-parse", "--verify", &format!("refs/heads/{branch}")]
                )
                .is_ok(),
                "{branch}"
            );
        }
        assert!(root.join(".chillax/chillax.toml").is_file());
        assert!(Project::discover(root).is_ok());
    }

    #[test]
    fn init_in_repo_without_commits_makes_first_commit_and_branches() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        git(root, &["init", "-q", "-b", "main"]).expect("git init");
        init_with(root, false, Some(TEST_IDENTITY)).expect("init");
        assert!(git(root, &["rev-parse", "--verify", "HEAD"]).is_ok());
        for branch in ["dev", "qa"] {
            assert!(
                git(
                    root,
                    &["rev-parse", "--verify", &format!("refs/heads/{branch}")]
                )
                .is_ok(),
                "{branch}"
            );
        }
    }

    #[test]
    fn init_without_git_flag_in_plain_dir_errors_with_hint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = format!("{:#}", init(dir.path()).expect_err("not a repo"));
        assert!(err.contains("chillax init --git"), "{err}");
        assert!(!dir.path().join(".git").exists());
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        git(dir.path(), &["init", "-q", "-b", "main"]).expect("git init");
        git(
            dir.path(),
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
        )
        .expect("commit");
        dir
    }

    #[test]
    fn builtin_roles_parse() {
        let catalog = load_roles(Path::new("/nonexistent")).expect("roles load");
        let ids: Vec<_> = catalog.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "fullstack-developer",
                "project-manager",
                "qa-engineer",
                "tech-lead"
            ]
        );
        let pm = catalog.get("project-manager").expect("pm");
        assert!(pm.may_use_tool("create_sprint"));
        assert!(pm.prompt.contains("{{company}}"));
    }

    #[test]
    fn the_project_manager_can_read_the_board_and_hire_to_staff_it() {
        let catalog = load_roles(Path::new("/nonexistent")).expect("roles load");
        for role in catalog.iter() {
            for tool in &role.mcp_tools {
                assert!(
                    chillax_mcp::TOOLS.iter().any(|t| t.name == tool),
                    "{} lists unknown tool {tool}",
                    role.id
                );
            }
        }
        let pm = catalog.get("project-manager").expect("pm");
        assert!(pm.may_use_tool("board_digest"));
        assert!(pm.may_use_tool("hire_teammate"));
        assert!(pm.prompt.contains("Keep the sprint on track"));
        assert!(pm.prompt.contains("hire_teammate"));
        let dev = catalog.get("fullstack-developer").expect("dev");
        assert!(!dev.may_use_tool("board_digest"));
    }

    #[test]
    fn init_is_idempotent_and_loadable() {
        let repo = repo();
        let first = init(repo.path()).expect("init");
        assert!(first.iter().any(|l| l.contains("created branch dev")));
        assert!(first.iter().any(|l| l.contains(".gitignore")));
        let second = init(repo.path()).expect("re-init");
        assert!(second.is_empty(), "{second:?}");

        let gitignore = std::fs::read_to_string(repo.path().join(".gitignore")).expect("gitignore");
        assert!(gitignore.contains(".chillax/state/"));

        let project = Project::discover(repo.path()).expect("discover");
        assert_eq!(project.config.server.port, 4747);
        assert_eq!(project.workflow.initial_state(), "backlog");
    }

    #[test]
    fn init_creates_the_branches_the_config_names() {
        let repo = repo();
        std::fs::create_dir_all(repo.path().join(".chillax")).expect("dir");
        std::fs::write(
            repo.path().join(".chillax/chillax.toml"),
            "[project]\nname = \"x\"\n[env.dev]\nbase_branch = \"develop\"\n[env.qa]\nbranch = \"staging\"\n",
        )
        .expect("config");
        let done = init(repo.path()).expect("init");
        assert!(
            done.iter().any(|l| l.contains("created branch develop")),
            "{done:?}"
        );
        assert!(
            done.iter().any(|l| l.contains("created branch staging")),
            "{done:?}"
        );
        assert!(!done.iter().any(|l| l.contains("branch qa")), "{done:?}");
    }

    #[test]
    fn project_roles_override_builtins() {
        let repo = repo();
        init(repo.path()).expect("init");
        std::fs::write(
            repo.path().join(".chillax/roles/project-manager.md"),
            "---\nid: project-manager\ntitle: Product Owner\n---\nCustom.\n",
        )
        .expect("write override");
        let project = Project::discover(repo.path()).expect("discover");
        assert_eq!(
            project
                .roles
                .get("project-manager")
                .map(|r| r.title.as_str()),
            Some("Product Owner")
        );
    }

    #[test]
    fn discover_requires_init() {
        let repo = repo();
        let err = Project::discover(repo.path())
            .err()
            .expect("not initialised");
        assert!(err.to_string().contains("chillax init"));
    }

    #[test]
    fn appended_agents_load_and_keep_comments() {
        let repo = repo();
        init(repo.path()).expect("init");
        let preset = chillax_core::agents::known_presets()
            .into_iter()
            .find(|(name, ..)| *name == "gemini")
            .expect("gemini preset")
            .2;
        append_agent(repo.path(), "gemini", &preset).expect("appends");
        append_agent(repo.path(), "gemini", &preset).expect("second time is a no-op");
        let text = std::fs::read_to_string(Project::dir(repo.path()).join("chillax.toml"))
            .expect("config");
        assert_eq!(text.matches("\n[agents.gemini]").count(), 1);
        assert!(text.contains("# localhost only by default"));
        let project = Project::load(repo.path()).expect("loads");
        assert_eq!(project.config.agents["gemini"].args, ["--acp"]);
    }

    #[test]
    fn appended_command_code_preset_loads() {
        let repo = repo();
        init(repo.path()).expect("init");
        let preset = chillax_core::agents::known_presets()
            .into_iter()
            .find(|(name, ..)| *name == "command-code")
            .expect("command-code preset")
            .2;
        append_agent(repo.path(), "command-code", &preset).expect("appends");
        let project = Project::load(repo.path()).expect("loads");
        assert_eq!(
            project.config.agents["command-code"].command,
            "command-code"
        );
        assert_eq!(project.config.agents["command-code"].args, ["acp"]);
    }

    fn branches(root: &Path) -> Vec<(String, String)> {
        git(
            root,
            &[
                "for-each-ref",
                "--format=%(refname:short) %(tree)",
                "refs/heads",
            ],
        )
        .expect("refs")
        .lines()
        .map(|l| {
            let (a, b) = l.split_once(' ').expect("pair");
            (a.to_string(), b.to_string())
        })
        .collect()
    }

    #[test]
    fn init_on_an_unborn_repo_with_and_without_the_git_flag_makes_all_branches() {
        for create in [true, false] {
            let dir = tempfile::tempdir().expect("tempdir");
            if !create {
                git(dir.path(), &["init", "-q", "-b", "trunk"]).expect("git init");
            }
            let done = init_with(dir.path(), create, Some(TEST_IDENTITY)).expect("init");
            assert!(
                !done.iter().any(|d| d.contains("no commits yet")),
                "{done:?}"
            );
            let root = dir.path();
            let head = git(root, &["rev-parse", "HEAD"]).expect("head");
            for b in ["dev", "qa"] {
                assert_eq!(
                    git(root, &["rev-parse", &format!("refs/heads/{b}")]).expect(b),
                    head
                );
            }
        }
    }

    #[test]
    fn init_cuts_branches_from_prod_not_the_checked_out_feature_branch() {
        let dir = repo();
        let root = dir.path();
        let main = git(root, &["rev-parse", "main"]).expect("main");
        git(root, &["checkout", "-q", "-b", "feat/x"]).expect("checkout");
        std::fs::write(root.join("x.txt"), "x").expect("write");
        git(root, &["add", "x.txt"]).expect("add");
        git(
            root,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "x",
            ],
        )
        .expect("commit");
        init_with(root, false, Some(TEST_IDENTITY)).expect("init");
        assert_eq!(git(root, &["rev-parse", "dev"]).expect("dev"), main);
        assert_eq!(git(root, &["rev-parse", "qa"]).expect("qa"), main);
    }

    #[test]
    fn a_second_init_changes_no_branch() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_with(dir.path(), true, Some(TEST_IDENTITY)).expect("init");
        let before = branches(dir.path());
        let done = init_with(dir.path(), true, Some(TEST_IDENTITY)).expect("again");
        assert_eq!(branches(dir.path()), before);
        assert!(
            !done.iter().any(|d| d.contains("created branch")),
            "{done:?}"
        );
    }

    #[test]
    fn init_and_ensure_branches_make_the_same_layout() {
        let a = tempfile::tempdir().expect("tempdir");
        let b = tempfile::tempdir().expect("tempdir");
        for d in [&a, &b] {
            git(d.path(), &["init", "-q", "-b", "main"]).expect("git init");
        }
        init_with(a.path(), false, Some(TEST_IDENTITY)).expect("init");
        // Same files init would leave committed, then the library path on a fresh repo.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            chillax_vcs::LocalGit::new(b.path(), "dev")
                .with_prod("main")
                .ensure_branches(&["qa"])
                .await
                .expect("ensure")
        });
        let names = |r: &Path| branches(r).into_iter().map(|(n, _)| n).collect::<Vec<_>>();
        assert_eq!(names(a.path()), names(b.path()));
        // Init commits a .gitignore, the library an empty tree: each is internally consistent.
        for d in [&a, &b] {
            let main = git(d.path(), &["rev-parse", "main"]).expect("main");
            for br in ["dev", "qa"] {
                assert_eq!(git(d.path(), &["rev-parse", br]).expect(br), main);
            }
        }
    }

    fn ensure(cwd: &Path, home: Option<&Path>) -> (Result<Project>, String) {
        let mut out = Vec::new();
        let r = ensure_project_with(cwd, Some(TEST_IDENTITY), home, &mut out);
        (r, String::from_utf8(out).expect("utf8"))
    }

    fn has_branches(root: &Path) {
        for b in ["main", "dev", "qa"] {
            assert!(
                git(root, &["rev-parse", "--verify", &format!("refs/heads/{b}")]).is_ok(),
                "{b}"
            );
        }
    }

    #[test]
    fn ensure_sets_up_a_plain_folder_with_one_notice() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let (r, out) = ensure(dir.path(), Some(home.path()));
        r.expect("ensure");
        assert!(dir.path().join(".git").exists());
        assert!(dir.path().join(".chillax/chillax.toml").is_file());
        has_branches(dir.path());
        assert_eq!(
            git(dir.path(), &["rev-parse", "--abbrev-ref", "HEAD"]).expect("head"),
            "main"
        );
        assert_eq!(out.lines().count(), 1, "{out}");
        assert!(out.contains("created a git repository"), "{out}");
        let root = dir.path().canonicalize().expect("canon");
        assert!(out.contains(&root.display().to_string()), "{out}");
    }

    #[test]
    fn ensure_sets_up_an_unborn_repo() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        git(dir.path(), &["init", "-q", "-b", "main"]).expect("git init");
        let (r, out) = ensure(dir.path(), Some(home.path()));
        r.expect("ensure");
        has_branches(dir.path());
        assert_eq!(out.lines().count(), 1, "{out}");
        assert!(out.starts_with("Set up Chillax in "), "{out}");
    }

    #[test]
    fn ensure_from_a_subfolder_sets_up_the_repository_root() {
        let repo = repo();
        let home = tempfile::tempdir().expect("home");
        let sub = repo.path().join("src/deep");
        std::fs::create_dir_all(&sub).expect("mkdir");
        let (r, out) = ensure(&sub, Some(home.path()));
        let project = r.expect("ensure");
        assert!(repo.path().join(".chillax/chillax.toml").is_file());
        assert!(!sub.join(".chillax").exists());
        assert_eq!(project.root, repo.path().canonicalize().expect("canon"));
        assert!(out.contains("repository root"), "{out}");
        assert_eq!(out.lines().count(), 1, "{out}");
    }

    #[test]
    fn ensure_on_an_initialised_project_is_silent_and_writes_nothing() {
        let repo = repo();
        let home = tempfile::tempdir().expect("home");
        init_with(repo.path(), false, Some(TEST_IDENTITY)).expect("init");
        let head = git(repo.path(), &["rev-parse", "HEAD"]).expect("head");
        let status = git(repo.path(), &["status", "--porcelain"]).expect("status");
        let (r, out) = ensure(repo.path(), Some(home.path()));
        r.expect("ensure");
        assert!(out.is_empty(), "{out}");
        assert_eq!(
            git(repo.path(), &["rev-parse", "HEAD"]).expect("head"),
            head
        );
        assert_eq!(
            git(repo.path(), &["status", "--porcelain"]).expect("status"),
            status
        );
    }

    #[test]
    fn a_second_ensure_is_silent_and_adds_no_commit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let (r1, out1) = ensure(dir.path(), Some(home.path()));
        r1.expect("first");
        let (r2, out2) = ensure(dir.path(), Some(home.path()));
        r2.expect("second");
        assert_eq!(out1.lines().count(), 1, "{out1}");
        assert!(out2.is_empty(), "{out2}");
        let log = git(dir.path(), &["log", "--oneline", "main"]).expect("log");
        assert_eq!(log.lines().count(), 1, "{log}");
    }

    #[test]
    fn ensure_refuses_the_home_directory_and_the_system_root() {
        let home = tempfile::tempdir().expect("home");
        let (r, out) = ensure(home.path(), Some(home.path()));
        let err = format!("{:#}", r.err().expect("refused"));
        assert!(err.contains("refusing"), "{err}");
        assert!(out.is_empty());
        assert!(!home.path().join(".chillax").exists());
        assert!(!home.path().join(".git").exists());

        // A plain folder that contains the home directory.
        let parent = tempfile::tempdir().expect("parent");
        let inner_home = parent.path().join("home");
        std::fs::create_dir(&inner_home).expect("mkdir");
        let (r, _) = ensure(parent.path(), Some(&inner_home));
        assert!(format!("{:#}", r.err().expect("refused")).contains("refusing"));
        assert!(!parent.path().join(".git").exists());

        let err = check_autoinit_allowed(Path::new("/"), false, None).expect_err("root");
        assert!(err.to_string().contains("refusing"));
    }

    #[test]
    fn ensure_refuses_a_repo_rooted_at_home_even_from_a_subfolder() {
        let home = repo();
        let sub = home.path().join("Downloads");
        std::fs::create_dir(&sub).expect("mkdir");
        let (r, out) = ensure(&sub, Some(home.path()));
        assert!(format!("{:#}", r.err().expect("refused")).contains("refusing"));
        assert!(out.is_empty());
        assert!(!home.path().join(".chillax").exists());
    }

    #[test]
    fn a_linked_worktree_resolves_to_the_main_project() {
        let repo = repo();
        let home = tempfile::tempdir().expect("home");
        init_with(repo.path(), false, Some(TEST_IDENTITY)).expect("init");
        let wt = tempfile::tempdir().expect("wt");
        let wt_path = wt.path().join("agent");
        git(
            repo.path(),
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "agent",
                wt_path.to_str().expect("utf8"),
            ],
        )
        .expect("worktree");
        let (r, out) = ensure(&wt_path, Some(home.path()));
        let project = r.expect("ensure");
        assert!(out.is_empty(), "{out}");
        assert_eq!(project.root, repo.path().canonicalize().expect("canon"));
        // The worktree holds the committed config, but no runtime state of its own.
        assert!(!wt_path.join(".chillax/state").exists());
    }

    fn status(root: &Path) -> String {
        git(root, &["status", "--porcelain"]).expect("status")
    }

    fn commit_count(root: &Path) -> usize {
        git(root, &["rev-list", "--count", "HEAD"])
            .expect("count")
            .parse()
            .expect("number")
    }

    fn user_commit(root: &Path, file: &str) {
        std::fs::write(root.join(file), "x\n").expect("write");
        git(root, &["add", file]).expect("add");
        git(
            root,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "user",
            ],
        )
        .expect("commit");
    }

    #[test]
    fn init_commits_its_files_so_prod_is_clean_and_branches_have_them() {
        let dir = repo();
        let root = dir.path();
        user_commit(root, "a.txt");
        init_with(root, false, Some(TEST_IDENTITY)).expect("init");
        assert_eq!(status(root), "");
        for branch in ["main", "dev", "qa"] {
            let files = git(root, &["ls-tree", "-r", "--name-only", branch]).expect("tree");
            assert!(files.contains(".chillax/chillax.toml"), "{branch}: {files}");
            assert!(files.contains(".gitignore"), "{branch}: {files}");
        }
    }

    #[test]
    fn init_leaves_staged_and_untracked_user_files_alone() {
        let dir = repo();
        let root = dir.path();
        std::fs::write(root.join("staged.txt"), "s\n").expect("write");
        git(root, &["add", "staged.txt"]).expect("add");
        std::fs::write(root.join("loose.txt"), "l\n").expect("write");
        init_with(root, false, Some(TEST_IDENTITY)).expect("init");
        let files = git(root, &["show", "--name-only", "--format=", "HEAD"]).expect("show");
        assert!(
            !files.contains("staged.txt") && !files.contains("loose.txt"),
            "{files}"
        );
        assert!(files.contains(".chillax/chillax.toml"), "{files}");
        let st = status(root);
        assert!(st.contains("A  staged.txt"), "{st}");
        assert!(st.contains("?? loose.txt"), "{st}");
        assert!(!st.contains(".chillax"), "{st}");
    }

    #[test]
    fn init_does_not_commit_a_gitignore_with_user_changes() {
        let dir = repo();
        let root = dir.path();
        std::fs::write(root.join(".gitignore"), "target/\n").expect("write");
        let done = init_with(root, false, Some(TEST_IDENTITY)).expect("init");
        assert!(
            done.iter()
                .any(|d| d.contains(".gitignore had uncommitted")),
            "{done:?}"
        );
        let files = git(root, &["show", "--name-only", "--format=", "HEAD"]).expect("show");
        assert!(!files.contains(".gitignore"), "{files}");
        assert_eq!(status(root), "?? .gitignore");
    }

    #[test]
    fn init_off_prod_leaves_files_and_says_so() {
        let dir = repo();
        let root = dir.path();
        git(root, &["switch", "-q", "-c", "feature"]).expect("switch");
        let before = commit_count(root);
        let done = init_with(root, false, Some(TEST_IDENTITY)).expect("init");
        assert_eq!(commit_count(root), before);
        assert!(
            done.iter().any(|d| d.contains("HEAD is not on main")),
            "{done:?}"
        );
    }

    #[test]
    fn init_on_an_empty_repo_makes_one_commit_with_everything() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        git(root, &["init", "-q", "-b", "main"]).expect("git init");
        init_with(root, false, Some(TEST_IDENTITY)).expect("init");
        assert_eq!(commit_count(root), 1);
        let files = git(root, &["ls-tree", "-r", "--name-only", "main"]).expect("tree");
        assert!(files.contains(".chillax/chillax.toml"), "{files}");
        assert!(files.contains(".gitignore"), "{files}");
        assert_eq!(status(root), "");
    }

    #[test]
    fn a_second_init_makes_no_commit() {
        let dir = repo();
        let root = dir.path();
        init_with(root, false, Some(TEST_IDENTITY)).expect("init");
        let before = commit_count(root);
        init_with(root, false, Some(TEST_IDENTITY)).expect("init again");
        assert_eq!(commit_count(root), before);
        assert_eq!(status(root), "");
    }
}
