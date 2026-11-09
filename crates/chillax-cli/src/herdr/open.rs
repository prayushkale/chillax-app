//! `chillax herdr ...`: open employees and projects in herdr panes (ADR 0018, 6b). The
//! plugin in `integrations/herdr` is a thin wrapper over these commands.

use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, bail};
use chillax_core::config::{AppConfig, HerdrMode};
use chillax_core::engine::State;
use chillax_core::{InstanceInfo, InstanceSource};
use clap::{Args, Subcommand, ValueEnum};

use super::{CliHerdr, Direction, Herdr, HerdrEnv, NewPane, SplitTarget, doctor, shell_quote};
use crate::hire::ask;
use crate::instance::Registry;
use crate::project::Project;

/// What to say when a herdr verb runs outside herdr.
pub const NOT_IN_HERDR: &str = "not inside a herdr pane; run chillax inside herdr, or use \
`chillax herdr workspace` from a herdr pane";

#[derive(Subcommand)]
pub enum HerdrCmd {
    /// Open an employee in a new herdr pane (default: a shell in a split to the right).
    Open(OpenArgs),
    /// Show whether Chillax is inside herdr, and which herdr version.
    Status,
    /// Create (or focus) a herdr workspace for this project, running `chillax` in it.
    Workspace,
    /// Run `chillax` in the current herdr pane (used by the herdr plugin).
    Start,
    /// Open this project's web UI in the browser (used by the herdr plugin).
    Dashboard,
    /// Create or focus the herdr workspace of a running project (default: ask).
    Switch {
        /// Project name or root (see `chillax projects`).
        project: Option<String>,
    },
    /// Open a pane that asks which employee or project, then does it (used by the plugin).
    Pick {
        #[arg(value_enum)]
        what: Pick,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Pick {
    Employee,
    Project,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Dir {
    Right,
    Down,
}

impl From<Dir> for Direction {
    fn from(d: Dir) -> Self {
        match d {
            Dir::Right => Direction::Right,
            Dir::Down => Direction::Down,
        }
    }
}

#[derive(Args)]
pub struct OpenArgs {
    /// Employee id or name (default: ask).
    pub employee: Option<String>,
    /// A shell in the employee's worktree (the default); runs nothing.
    #[arg(long, conflicts_with_all = ["transcript", "app"])]
    pub shell: bool,
    /// Follow what the employee is doing (`chillax transcript <employee> --follow`).
    #[arg(long, conflicts_with = "app")]
    pub transcript: bool,
    /// Run the dev app (`[env.dev] run`) in the employee's worktree.
    #[arg(long)]
    pub app: bool,
    /// Open in a new tab instead of a split.
    #[arg(long, conflicts_with = "split")]
    pub tab: bool,
    /// Split the current pane in this direction (default: right).
    #[arg(long, value_enum)]
    pub split: Option<Dir>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Shell,
    Transcript,
    App,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    Tab,
    Split(Direction),
}

impl OpenArgs {
    pub fn mode(&self) -> Mode {
        if self.app {
            Mode::App
        } else if self.transcript {
            Mode::Transcript
        } else {
            Mode::Shell
        }
    }

    pub fn place(&self) -> Place {
        if self.tab {
            Place::Tab
        } else {
            Place::Split(self.split.map_or(Direction::Right, Direction::from))
        }
    }
}

/// What to open: the directory, and the command to type once the shell is up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub cwd: PathBuf,
    pub command: Option<String>,
}

/// Where an employee's work is: the worktree of their first live ticket that has one
/// (and still exists), else the project root.
pub fn employee_cwd(
    state: &State,
    employee: &str,
    root: &Path,
    exists: impl Fn(&Path) -> bool,
) -> PathBuf {
    state
        .assignments(employee)
        .into_iter()
        .filter(|t| t.dropped.is_none() && t.worktree.is_some())
        .map(|t| t.worktree_path(root))
        .find(|p| exists(p))
        .unwrap_or_else(|| root.to_path_buf())
}

/// The pane command for `mode`. The dev app command comes from the committed config and
/// is meant to run in a shell, like `chillax`'s own supervisor runs it.
pub fn plan(mode: Mode, employee_id: &str, cwd: PathBuf, app: &AppConfig) -> Result<Plan> {
    Ok(match mode {
        Mode::Shell => Plan { cwd, command: None },
        Mode::Transcript => Plan {
            cwd,
            command: Some(format!(
                "chillax transcript {} --follow",
                shell_quote(employee_id)
            )),
        },
        Mode::App => {
            let Some(run) = app.run.as_deref().map(str::trim).filter(|r| !r.is_empty()) else {
                bail!("no dev app configured: set `run` under [env.dev] in .chillax/chillax.toml");
            };
            let run = match app.port {
                Some(port) => run.replace("{port}", &port.to_string()),
                None => run.to_string(),
            };
            let cwd = match app.run_cwd.as_deref() {
                Some(sub) if !sub.is_empty() => cwd.join(sub),
                _ => cwd,
            };
            Plan {
                cwd,
                command: Some(run),
            }
        }
    })
}

/// Open `plan` in a new pane or tab and type its command. Nothing is spawned through a
/// shell here; the command is typed into the pane's own shell by herdr.
pub async fn open_plan(herdr: &Herdr, plan: &Plan, label: &str, place: Place) -> Result<()> {
    let spec = NewPane {
        cwd: Some(plan.cwd.clone()),
        env: Vec::new(),
        focus: Some(true),
    };
    let created = match place {
        Place::Tab => herdr.tab_create(Some(label), &spec).await,
        Place::Split(dir) => {
            herdr
                .pane_split(&SplitTarget::Current, dir, None, &spec)
                .await
        }
    }?;
    if let Some(command) = &plan.command {
        herdr.pane_run(&created.pane, command).await?;
    }
    Ok(())
}

/// A workspace for a project: focus the one labelled with its name, else create it and
/// start `chillax` there.
pub async fn project_workspace(herdr: &Herdr, name: &str, root: &Path) -> Result<()> {
    if let Some(id) = herdr.find_workspace(name).await {
        herdr.workspace_focus(&id).await?;
        return Ok(());
    }
    let spec = NewPane {
        cwd: Some(root.to_path_buf()),
        env: Vec::new(),
        focus: Some(true),
    };
    let created = herdr.workspace_create(Some(name), &spec).await?;
    herdr.pane_run(&created.pane, "chillax").await?;
    Ok(())
}

/// The running instance whose project contains `cwd` (the deepest root wins).
pub fn instance_for_cwd<'a>(list: &'a [InstanceInfo], cwd: &Path) -> Option<&'a InstanceInfo> {
    list.iter()
        .filter(|i| cwd.starts_with(&i.root))
        .max_by_key(|i| i.root.len())
}

/// A fresh client for this one command, so a user-run verb never inherits a disabled
/// background reporter.
fn connect(project: &str) -> Result<Herdr> {
    let env: BTreeMap<String, String> = std::env::vars().collect();
    let Some(found) = HerdrEnv::detect(&env) else {
        bail!(NOT_IN_HERDR);
    };
    Ok(Herdr::new(
        Arc::new(CliHerdr::new(found.bin)),
        found.pane,
        project,
    ))
}

fn project_name(project: &Result<Project>) -> String {
    project
        .as_ref()
        .map_or_else(|_| "chillax".into(), |p| p.config.project.name.clone())
}

pub async fn run(cmd: HerdrCmd, cwd: &Path) -> Result<()> {
    match cmd {
        HerdrCmd::Status => {
            let env: BTreeMap<String, String> = std::env::vars().collect();
            if HerdrEnv::detect(&env).is_none() {
                println!("herdr: {NOT_IN_HERDR}");
            }
            for line in doctor(HerdrMode::On, &env).await {
                println!("{line}");
            }
            Ok(())
        }
        HerdrCmd::Open(args) => {
            let project = crate::project::ensure_project(cwd);
            let herdr = connect(&project_name(&project))?;
            let project = project?;
            let state = crate::settings::company_state(&project).await?;
            let who = match &args.employee {
                Some(who) => who.clone(),
                None => choose_employee(&state).await?,
            };
            let employee = crate::settings::find_employee(&state, &who)?;
            let id = employee.employee.id.clone();
            let dir = employee_cwd(&state, &id, &project.root, |p| p.is_dir());
            let plan = plan(args.mode(), &id, dir, &project.config.env.dev.app)?;
            open_plan(&herdr, &plan, &employee.employee.name, args.place()).await
        }
        HerdrCmd::Workspace => {
            let project = crate::project::ensure_project(cwd)?;
            let herdr = connect(&project.config.project.name)?;
            project_workspace(&herdr, &project.config.project.name, &project.root).await
        }
        HerdrCmd::Start => {
            let herdr = connect("chillax")?;
            herdr.pane_run(herdr.pane(), "chillax").await?;
            Ok(())
        }
        HerdrCmd::Dashboard => {
            let herdr = connect("chillax")?;
            let here = active_cwd(&herdr, cwd).await;
            let list = Registry::from_env().list().await;
            let here = std::fs::canonicalize(&here).unwrap_or(here);
            let Some(found) = instance_for_cwd(&list, &here) else {
                bail!(
                    "no running Chillax for {}; start it with `chillax`",
                    here.display()
                );
            };
            open_url(&found.url)
        }
        HerdrCmd::Switch { project } => {
            let herdr = connect("chillax")?;
            let list = Registry::from_env().list().await;
            let wanted = match project {
                Some(p) => p,
                None => choose_project(&list).await?,
            };
            let Some(found) = list.iter().find(|i| i.name == wanted || i.root == wanted) else {
                bail!("no running project `{wanted}` (see `chillax projects`)");
            };
            project_workspace(&herdr, &found.name, Path::new(&found.root)).await
        }
        HerdrCmd::Pick { what } => {
            let herdr = connect("chillax")?;
            let sub = match what {
                Pick::Employee => "open",
                Pick::Project => "switch",
            };
            let spec = NewPane {
                cwd: Some(active_cwd(&herdr, cwd).await),
                env: Vec::new(),
                focus: Some(true),
            };
            let created = herdr
                .pane_split(&SplitTarget::Current, Direction::Down, None, &spec)
                .await?;
            herdr
                .pane_run(&created.pane, &format!("chillax herdr {sub}"))
                .await?;
            Ok(())
        }
    }
}

/// The directory of the pane the user is looking at: what herdr key commands export,
/// else what `pane get` reports, else this process's own directory.
async fn active_cwd(herdr: &Herdr, fallback: &Path) -> PathBuf {
    if let Some(dir) = std::env::var_os("HERDR_ACTIVE_PANE_CWD").filter(|v| !v.is_empty()) {
        return dir.into();
    }
    herdr
        .pane_cwd(herdr.pane())
        .await
        .unwrap_or_else(|| fallback.to_path_buf())
}

async fn choose_employee(state: &State) -> Result<String> {
    let names: Vec<String> = state
        .employees
        .iter()
        .map(|e| format!("{} ({})", e.employee.name, e.employee.role_title))
        .collect();
    let ids: Vec<String> = state
        .employees
        .iter()
        .map(|e| e.employee.id.clone())
        .collect();
    choose("employee", &names, &ids).await
}

async fn choose_project(list: &[InstanceInfo]) -> Result<String> {
    let names: Vec<String> = list
        .iter()
        .map(|i| format!("{}  {}", i.name, i.root))
        .collect();
    let values: Vec<String> = list.iter().map(|i| i.name.clone()).collect();
    choose("project", &names, &values).await
}

/// Number the choices and read one; needs a terminal.
async fn choose(what: &str, labels: &[String], values: &[String]) -> Result<String> {
    if labels.is_empty() {
        bail!("no {what} to choose from");
    }
    if !std::io::stdin().is_terminal() {
        bail!("name the {what} on the command line (no terminal to ask on)");
    }
    for (i, label) in labels.iter().enumerate() {
        println!("{:>2}. {label}", i + 1);
    }
    let answer = ask(&format!("Which {what}? [1-{}] ", labels.len())).await?;
    answer
        .trim()
        .parse::<usize>()
        .ok()
        .and_then(|n| values.get(n.checked_sub(1)?))
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("not a choice: {}", answer.trim()))
}

/// Open a URL with the platform opener; on failure say where to go instead.
fn open_url(url: &str) -> Result<()> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    match std::process::Command::new(opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(_) => Ok(()),
        Err(error) => bail!("could not run {opener} ({error}); open {url} yourself"),
    }
}
