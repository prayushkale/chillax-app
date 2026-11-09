//! `chillax`: the binary that wires the company together.

mod company;
mod config_write;
mod hardware;
mod herdr;
mod hire;
mod instance;
mod project;
mod release;
mod reload;
mod remote;
mod settings;
mod transcript;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chillax_core::{Actor, Command, CompanyHandle};
use chillax_tui::App;
use clap::{Parser, Subcommand};

use crate::company::{Company, live_url, server_url};
use crate::project::{Project, git};

#[derive(Parser)]
#[command(
    name = "chillax",
    version,
    about = "You are the stakeholder. Chillax runs the company."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create .chillax/, gitignore entries, and dev/qa branches.
    Init {
        /// Also create the git repository (and a first commit) when there is none.
        #[arg(long)]
        git: bool,
    },
    /// Check git, config, roles and every configured agent backend (ACP handshake).
    Doctor {
        /// Seconds to wait for each agent's `initialize` handshake.
        #[arg(long, default_value_t = 90)]
        timeout: u64,
    },
    /// Run the web server and engine without the TUI.
    Serve {
        /// Required for now; reserved for a future mode that also opens the TUI.
        #[arg(long)]
        headless: bool,
    },
    /// List the Chillax instances running on this machine, one per project.
    Projects {
        /// Print JSON (`current` and `instances`, as `/api/v1/instances`).
        #[arg(long)]
        json: bool,
    },
    /// Hire an employee into a role, or from a catalog persona (reviewed first).
    Hire {
        /// Role id (e.g. project-manager) or catalog persona (see `chillax roles --search`).
        #[arg(required_unless_present_any = ["from", "like"], conflicts_with_all = ["from", "like"])]
        role: Option<String>,
        /// Hire another employee just like this one (id or name): same role, backend,
        /// model and effort. --name may still pick the name.
        #[arg(long, conflicts_with_all = ["from", "title", "backend", "model", "effort", "base", "role_id", "yes"])]
        like: Option<String>,
        /// Create a new role from this existing one (any role; works with the catalog
        /// off); pair with --role-id and optionally --title.
        #[arg(long, requires = "role_id")]
        from: Option<String>,
        /// With --from: title of the new role (default: the id, capitalised).
        #[arg(long, requires = "from")]
        title: Option<String>,
        #[arg(long)]
        name: Option<String>,
        /// Agent backend from [agents.*] in chillax.toml.
        #[arg(long)]
        backend: Option<String>,
        #[arg(long)]
        model: Option<String>,
        /// Reasoning effort (thought level) for the agent, e.g. high (see `settings --models`).
        /// Existing roles only: persona drafts don't carry one.
        #[arg(long)]
        effort: Option<String>,
        /// Personas only: the Chillax role whose tools and permissions it gets
        /// (default: suggested from [catalog.base_roles] and your past picks).
        #[arg(long)]
        base: Option<String>,
        /// Personas only: id for the adopted role (default: the persona's slug).
        #[arg(long)]
        role_id: Option<String>,
        /// Personas only: adopt without the interactive review.
        #[arg(long)]
        yes: bool,
        /// Personas and --from only: have the agent tailor the persona to this project's
        /// plan files (plan.md, AGENTS.md, …) before the review.
        #[arg(long, conflicts_with = "like")]
        tailor: bool,
    },
    /// List employees and what they're doing.
    Team,
    /// Fire (let go of) an employee: their session ends and their tickets return to
    /// the backlog.
    #[command(alias = "let-go")]
    Fire {
        /// Employee id or name (see `chillax team`).
        employee: String,
        /// Don't ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Change the agent (backend, model, effort) of an existing hire. Their sessions
    /// end and restart on the new agent; refused while they are replying unless
    /// `--when-idle` queues the change for the end of the turn.
    SetAgent {
        /// Employee id or name (see `chillax team`).
        employee: String,
        /// Agent backend (default: keep the current one).
        #[arg(long)]
        backend: Option<String>,
        /// Model id (default: keep it on the same backend, else the agent's default).
        #[arg(long)]
        model: Option<String>,
        /// Effort level (default: keep it for the same model, else the agent's default).
        #[arg(long)]
        effort: Option<String>,
        /// If they are replying, switch when the reply ends instead of refusing.
        #[arg(long)]
        when_idle: bool,
    },
    /// Show what an employee is saying and doing, as plain text.
    Transcript {
        /// Employee id or name (see `chillax team`).
        employee: String,
        /// Keep going as new events arrive.
        #[arg(long)]
        follow: bool,
        /// Start after this event sequence number.
        #[arg(long, default_value_t = 0)]
        since: u64,
    },
    /// Open employees and projects in herdr panes (run inside herdr).
    Herdr {
        #[command(subcommand)]
        command: herdr::HerdrCmd,
    },
    /// Stop the dev loop: no new work starts and ticket sessions are closed.
    Pause,
    /// Let the dev loop run again; unfinished tickets get their agents back.
    Resume,
    /// Promote `dev` to `qa` now instead of waiting for the sprint to finish.
    Promote,
    /// Open the release gate on what QA signed off, then approve it (or reject it).
    Release {
        /// Version to tag, like v0.2.0 (default: the suggestion).
        #[arg(long)]
        version: Option<String>,
        /// Read the release notes from this file (default: the generated notes).
        #[arg(long)]
        notes_file: Option<std::path::PathBuf>,
        /// Open the gate even though tickets failed QA; they ship as known issues.
        #[arg(long)]
        force: bool,
        /// Don't ask for confirmation.
        #[arg(long)]
        yes: bool,
        /// Reject the release at the gate, with this reason.
        #[arg(long, conflicts_with_all = ["version", "notes_file"])]
        reject: Option<String>,
    },
    /// Tell Chillax what you need; the project manager files it as work items and
    /// assigns each to the best-fit employee.
    Request {
        /// What you need, in your own words ("-" reads it from stdin).
        text: String,
        /// Start now: the tickets join the active sprint (or open one) without waiting
        /// for a sprint to be approved.
        #[arg(long)]
        now: bool,
    },
    /// Show where the code is: dev, qa, prod and the release candidate.
    Env {
        /// Run the failed environment step (or deploy) again.
        #[arg(long)]
        retry: bool,
        /// Start or restart the app of an environment (dev, qa or prod).
        #[arg(long, value_name = "ENV", conflicts_with = "retry")]
        restart: Option<String>,
    },
    /// Show or change the default coding agent, model and effort for new hires.
    Settings {
        /// Default agent backend, e.g. claude, codex, gemini. Installed presets are
        /// added to chillax.toml.
        #[arg(long)]
        backend: Option<String>,
        /// Default model on that backend (see --models).
        #[arg(long, conflicts_with = "default_model")]
        model: Option<String>,
        /// Use the agent's own default model.
        #[arg(long)]
        default_model: bool,
        /// Default effort (thought level) on that backend and model (see --models).
        #[arg(long, conflicts_with = "default_effort")]
        effort: Option<String>,
        /// Use the agent's own default effort.
        #[arg(long)]
        default_effort: bool,
        /// List each agent's models and effort levels (starts the agents; sends no prompts).
        #[arg(long)]
        models: bool,
    },
    /// List available roles and featured catalog personas.
    Roles {
        /// Search the persona catalog.
        #[arg(long)]
        search: Option<String>,
        /// Fetch the latest catalog first.
        #[arg(long)]
        refresh: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cwd = std::env::current_dir()?;
    match cli.command {
        None => run_tui(&cwd).await,
        Some(Cmd::Init { git }) => {
            let lines = if git {
                project::init_with_git(&cwd)?
            } else {
                project::init(&cwd)?
            };
            for line in lines {
                println!("  {line}");
            }
            println!("Ready. Next: `chillax doctor`, then `chillax`.");
            Ok(())
        }
        Some(Cmd::Projects { json }) => {
            let list = chillax_core::InstanceSource::list(&instance::Registry::from_env()).await;
            let current = Project::discover(&cwd)
                .ok()
                .map(|p| instance::canonical(&p.root));
            if json {
                let body = serde_json::json!({"current": current, "instances": list});
                println!("{}", serde_json::to_string_pretty(&body)?);
            } else {
                print!("{}", instance::render_projects(&list, current.as_deref()));
            }
            Ok(())
        }
        Some(Cmd::Doctor { timeout }) => doctor(&cwd, Duration::from_secs(timeout)).await,
        Some(Cmd::Serve { headless }) => {
            if !headless {
                bail!("use `chillax serve --headless` (or plain `chillax` for the TUI)");
            }
            let project = project::ensure_project(&cwd)?;
            init_logging(&project, true)?;
            let company = Arc::new(Company::start(&project).await?);
            company::end_apps_on_abort(company.clone(), || {});
            println!("Chillax is running at {} (Ctrl+C to stop)", company.url);
            tokio::signal::ctrl_c().await?;
            company.shutdown().await;
            tracing::info!("chillax stopped cleanly (ctrl-c)");
            Ok(())
        }
        Some(Cmd::Hire {
            role,
            like,
            from,
            title,
            name,
            backend,
            model,
            effort,
            base,
            role_id,
            yes,
            tailor,
        }) => {
            let project = project::ensure_project(&cwd)?;
            if let Some(like) = like {
                return settings::duplicate(&project, &like, name).await;
            }
            let role = role.unwrap_or_default();
            if project.roles.get(&role).is_none() || from.is_some() {
                return hire::hire_persona(
                    &project,
                    hire::HireArgs {
                        persona: role,
                        from,
                        title,
                        name,
                        base,
                        backend,
                        model,
                        role_id,
                        yes,
                        tailor,
                    },
                )
                .await;
            }
            if tailor {
                bail!("--tailor works on catalog personas and --from copies, not on `{role}`");
            }
            let (backend, model, effort) =
                settings::choose_agent(&project, backend, model, effort).await?;
            let cmd = Command::HireEmployee {
                role,
                name,
                backend,
                model,
                effort,
                keep_defaults: false,
            };
            let events = send_offline_or_live(&project, cmd).await?;
            for event in events {
                println!("{}", event["summary"].as_str().unwrap_or_default());
            }
            Ok(())
        }
        Some(Cmd::Team) => {
            let project = project::ensure_project(&cwd)?;
            let hub = Company::open(&project).await?;
            let state = hub.snapshot().await;
            if state.employees.is_empty() {
                println!("Nobody yet. Try `chillax hire fullstack-developer --name Dev`.");
            }
            for e in &state.employees {
                let tickets: Vec<_> = state
                    .assignments(&e.employee.id)
                    .iter()
                    .map(|t| t.id.clone())
                    .collect();
                let agent = [
                    Some(&e.employee.backend),
                    e.employee.model.as_ref(),
                    e.employee.effort.as_ref(),
                ]
                .into_iter()
                .flatten()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join("·");
                println!(
                    "{:<12} {:<22} {:<16} {:<15} {}",
                    e.employee.id,
                    e.employee.role_title,
                    agent,
                    e.status.label(),
                    tickets.join(", ")
                );
            }
            Ok(())
        }
        Some(Cmd::Fire { employee, yes }) => {
            let project = project::ensure_project(&cwd)?;
            settings::fire(&project, &employee, yes).await
        }
        Some(Cmd::SetAgent {
            employee,
            backend,
            model,
            effort,
            when_idle,
        }) => {
            let project = project::ensure_project(&cwd)?;
            settings::set_agent(&project, &employee, backend, model, effort, when_idle).await
        }
        Some(Cmd::Transcript {
            employee,
            follow,
            since,
        }) => transcript::run(&cwd, &employee, follow, since).await,
        Some(Cmd::Herdr { command }) => herdr::run(command, &cwd).await,
        Some(Cmd::Pause) => {
            let project = project::ensure_project(&cwd)?;
            let events = send_offline_or_live(&project, Command::PauseDispatch).await?;
            println!(
                "{}",
                if events.is_empty() {
                    "already paused"
                } else {
                    "paused"
                }
            );
            Ok(())
        }
        Some(Cmd::Resume) => {
            let project = project::ensure_project(&cwd)?;
            let events = send_offline_or_live(&project, Command::ResumeDispatch).await?;
            println!(
                "{}",
                if events.is_empty() {
                    "not paused"
                } else {
                    "resumed"
                }
            );
            Ok(())
        }
        Some(Cmd::Request { text, now }) => {
            release::request(&project::ensure_project(&cwd)?, &text, now).await
        }
        Some(Cmd::Promote) => release::promote(&project::ensure_project(&cwd)?).await,
        Some(Cmd::Release {
            version,
            notes_file,
            force,
            yes,
            reject,
        }) => {
            release::release(
                &project::ensure_project(&cwd)?,
                release::ReleaseArgs {
                    version,
                    notes_file,
                    force,
                    yes,
                    reject,
                },
            )
            .await
        }
        Some(Cmd::Env { retry, restart }) => {
            let project = project::ensure_project(&cwd)?;
            if let Some(env) = restart {
                release::restart_app(&project, &env).await
            } else if retry {
                release::retry(&project).await
            } else {
                release::env(&project).await
            }
        }
        Some(Cmd::Settings {
            backend,
            model,
            default_model,
            effort,
            default_effort,
            models,
        }) => {
            let project = project::ensure_project(&cwd)?;
            settings::settings(
                &project,
                settings::SettingsArgs {
                    backend,
                    model,
                    default_model,
                    effort,
                    default_effort,
                    models,
                },
            )
            .await
        }
        Some(Cmd::Roles { search, refresh }) => {
            let project = project::ensure_project(&cwd)?;
            if search.is_none() {
                println!("Your roles");
                for role in project.roles.iter() {
                    println!("{:<40} {:<22} {}", role.id, role.title, role.description);
                }
            }
            if project.config.catalog.enabled {
                hire::list_personas(&project, search.as_deref(), refresh).await?;
            } else if search.is_some() || refresh {
                bail!("the persona catalog is disabled ([catalog] enabled = false)");
            }
            Ok(())
        }
    }
}

async fn run_tui(cwd: &Path) -> Result<()> {
    let project = project::ensure_project(cwd)?;
    init_logging(&project, false)?;
    let company = Arc::new(Company::start(&project).await?);
    let handle: Arc<dyn CompanyHandle> = company.hub.clone();
    let app = App::new(handle.snapshot().await, handle.roles())
        .with_web_url(&company.url)
        .with_instances(Arc::new(instance::Registry::from_env()))
        .with_project_root(instance::canonical(&project.root))
        .with_attachments(company.attachments.clone());
    // OSC 8 links are hidden by terminals that know them; where one prints them instead,
    // CHILLAX_PLAIN_LINKS=1 (or TERM=dumb) draws the address as plain text.
    let app = if std::env::var_os("CHILLAX_PLAIN_LINKS").is_some_and(|v| v != "0")
        || std::env::var("TERM").is_ok_and(|t| t == "dumb")
    {
        app.with_plain_links()
    } else {
        app
    };

    // init_terminal installs a panic hook that restores the terminal and mouse capture.
    let mut terminal = chillax_tui::init_terminal().context("terminal UI failed")?;
    company::end_apps_on_abort(company.clone(), chillax_tui::restore_terminal);
    let result = app
        .run(
            &mut terminal,
            handle,
            company.recruiter.clone(),
            Some(company.agents.clone()),
        )
        .await;
    chillax_tui::restore_terminal();
    company.shutdown().await;
    match &result {
        Ok(()) => tracing::info!("chillax stopped cleanly"),
        Err(err) => tracing::error!(%err, "terminal UI exited with an error"),
    }
    result.context("terminal UI failed")?;
    println!("Bye. Your company's history is in .chillax/state/chillax.db.");
    Ok(())
}

/// Send a stakeholder command through the running server if there is one (so its
/// state stays authoritative), otherwise straight into the event store.
pub(crate) async fn send_offline_or_live(
    project: &Project,
    cmd: Command,
) -> Result<Vec<serde_json::Value>> {
    send_to(project, cmd, live_url(project).await).await
}

/// [`send_offline_or_live`] with the live server (verified to be this project's) given.
async fn send_to(
    project: &Project,
    cmd: Command,
    live: Option<String>,
) -> Result<Vec<serde_json::Value>> {
    let summarise = |events: Vec<chillax_core::Event>| {
        events
            .into_iter()
            .map(|e| serde_json::json!({"summary": e.kind.summary()}))
            .collect()
    };
    if let Some(url) = live {
        let resp = reqwest::Client::new()
            .post(format!("{url}/api/v1/commands"))
            .json(&cmd)
            .send()
            .await?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await?;
        if !status.is_success() {
            bail!("{}", body["error"].as_str().unwrap_or("command rejected"));
        }
        let events: Vec<chillax_core::Event> = serde_json::from_value(body["events"].clone())?;
        return Ok(summarise(events));
    }
    let hub = Company::open(project).await?;
    Ok(summarise(hub.send(Actor::Stakeholder, cmd).await?))
}

async fn doctor(cwd: &Path, timeout: Duration) -> Result<()> {
    let mut problems = 0;
    let mut check = |ok: bool, what: String| {
        println!("{} {what}", if ok { "✓" } else { "✗" });
        if !ok {
            problems += 1;
        }
    };

    match git(cwd, &["--version"]) {
        Ok(v) => check(true, v),
        Err(e) => check(false, format!("git: {e}")),
    }
    let project = match project::ensure_project(cwd) {
        Ok(p) => {
            check(
                true,
                format!(
                    "project `{}` at {}",
                    p.config.project.name,
                    p.root.display()
                ),
            );
            p
        }
        Err(e) => {
            check(false, format!("{e:#}"));
            bail!("fix the problems above and run `chillax doctor` again");
        }
    };
    check(
        true,
        format!(
            "{} roles, workflow with {} states",
            project.roles.iter().count(),
            project.workflow.states.len()
        ),
    );
    let env = &project.config.env;
    for branch in [env.dev.base_branch.as_str(), env.qa.branch.as_str()] {
        let ok = git(
            &project.root,
            &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
        )
        .is_ok();
        check(
            ok,
            format!(
                "branch {branch}{}",
                if ok {
                    ""
                } else {
                    " missing (run `chillax init` to recreate it)"
                }
            ),
        );
    }
    match &project.config.env.dev.test {
        Some(test) => check(
            true,
            format!("dev tests: `{test}` runs before every review"),
        ),
        None => println!(
            "! no `test` in [env.dev]: tests count as skipped, so every review request goes \
             straight to a reviewer"
        ),
    }
    if env.qa.up.is_none() || env.qa.test.is_none() {
        let missing: Vec<&str> = [("up", &env.qa.up), ("test", &env.qa.test)]
            .into_iter()
            .filter(|(_, c)| c.is_none())
            .map(|(name, _)| name)
            .collect();
        println!(
            "! no `{}` in [env.qa]: QA agents test without a running environment and Chillax \
             skips that step",
            missing.join("`/`")
        );
    }
    for problem in project.config.app_problems() {
        check(false, format!("apps: {problem}"));
    }
    let mut env_files: Vec<String> = Vec::new();
    for file in chillax_core::config::APP_ENVS
        .iter()
        .flat_map(|stage| env.files_for(stage))
    {
        if project.root.join(&file).is_file() && !env_files.contains(&file) {
            env_files.push(file);
        }
    }
    if !env_files.is_empty() {
        check(
            true,
            format!(
                "env files: {} reach every worktree, agent and command",
                env_files.join(", ")
            ),
        );
    }
    for file in &env_files {
        if git(&project.root, &["ls-files", "--error-unmatch", file]).is_ok() {
            println!("! {file} is committed to git: keep secrets out of the repository");
        }
    }
    for app in env.apps() {
        let (program, busy) = (
            app.command
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_string(),
            std::net::TcpStream::connect_timeout(
                &std::net::SocketAddr::from(([127, 0, 0, 1], app.port)),
                Duration::from_millis(200),
            )
            .is_ok(),
        );
        check(
            !busy,
            format!(
                "{} app: `{}` on port {} ({}){}",
                app.env,
                app.command,
                app.port,
                app.url,
                if busy {
                    " - something already listens on that port"
                } else {
                    ""
                }
            ),
        );
        if !program.is_empty() && !program.contains('/') && !program.contains('=') {
            let found = std::process::Command::new("sh")
                .args(["-c", &format!("command -v {program}")])
                .stdout(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if !found {
                println!(
                    "! `{program}` is not on PATH: the {} app may not start",
                    app.env
                );
            }
        }
    }
    let prod = env.prod.branch.as_str();
    if git(
        &project.root,
        &["rev-parse", "--verify", &format!("refs/heads/{prod}")],
    )
    .is_err()
    {
        println!(
            "! branch {prod} does not exist: the first release creates it from {}",
            env.qa.branch
        );
    }
    let vcs = &project.config.vcs;
    let remote = project::remote_url(&project.root, &vcs.remote);
    match vcs.kind {
        chillax_core::config::VcsKind::Github => match &remote {
            None => check(
                false,
                format!(
                    "vcs: github needs a git remote named `{}` (git remote add {} <url>); \
                     until then Chillax works locally",
                    vcs.remote, vcs.remote
                ),
            ),
            Some(url) => {
                check(
                    project::is_github_url(url),
                    format!("vcs: github, remote `{}` is {url}", vcs.remote),
                );
                match project::gh_authenticated() {
                    Ok(true) => check(true, "gh: signed in".into()),
                    Ok(false) => check(false, "gh: not signed in (run `gh auth login`)".into()),
                    Err(e) => check(false, format!("gh: {e:#}")),
                }
            }
        },
        chillax_core::config::VcsKind::Local => {
            check(
                true,
                "vcs: local (branches and merges stay on this machine)".into(),
            );
            if let Some(url) = remote.as_deref().filter(|u| project::is_github_url(u)) {
                let gh = match project::gh_authenticated() {
                    Ok(true) => "gh is signed in",
                    Ok(false) => "run `gh auth login` first",
                    Err(_) => "install the GitHub CLI (gh) first",
                };
                println!(
                    "! `{}` is on GitHub ({url}): to open a pull request per ticket, set \
                     `kind = \"github\"` under [vcs] in .chillax/chillax.toml ({gh})",
                    vcs.remote
                );
            }
        }
    }
    match company::check_tracker_kind(&project.config.tracker) {
        Ok(()) => check(true, "tracker: local".into()),
        Err(err) => check(false, format!("tracker: {err}")),
    }
    let url = server_url(&project);
    let server = &project.config.server;
    if let Some(live) = live_url(&project).await {
        check(true, format!("chillax is running at {live}"));
    } else {
        match instance::identity(&url, &project.root).await {
            instance::Identity::Other(who) => {
                let holder = instance::describe_holder(&instance::Identity::Other(who));
                // Without an explicit port Chillax moves on to the next free one.
                check(
                    !server.port_explicit,
                    format!(
                        "port {} is taken by {holder}{}",
                        server.port,
                        if server.port_explicit {
                            "; pick another [server] port"
                        } else {
                            "; the next free port will be used"
                        }
                    ),
                );
            }
            _ => {
                let free = tokio::net::TcpListener::bind((server.bind.as_str(), server.port))
                    .await
                    .is_ok();
                check(
                    free || !server.port_explicit,
                    format!(
                        "port {} {}",
                        server.port,
                        if free {
                            "is free"
                        } else {
                            "is in use by another program; the next free port will be used"
                        }
                    ),
                );
            }
        }
    }
    if !instance::Registry::from_env().is_enabled() {
        check(
            false,
            "no home directory: other chillax commands cannot find a running instance on a \
             fallback port"
                .into(),
        );
    }

    for line in herdr::doctor(project.config.herdr.enabled, &std::env::vars().collect()).await {
        println!("{line}");
    }

    if project.config.agents.is_empty() {
        check(
            false,
            "no [agents.*] backends configured in chillax.toml".into(),
        );
    }
    let agents = settings::agents(&project).await;
    for (name, agent) in &project.config.agents.backends {
        if agent.args.iter().any(|a| a.contains("claude-agent-acp")) {
            println!(
                "! `{name}` runs Claude Code's adapter under node, which may trigger a macOS \
                 keychain prompt for Claude Code's sign-in"
            );
        }
        println!(
            "… starting `{name}` ({} {}) for an ACP handshake",
            agent.command,
            agent.args.join(" ")
        );
        match chillax_agents::handshake(agent, timeout).await {
            Ok(info) => {
                check(
                    true,
                    format!(
                        "agent `{name}`: {} {} (protocol {}, MCP over HTTP: {}, session/load: {})",
                        info.name,
                        info.version,
                        info.protocol_version,
                        yes_no(info.mcp_http),
                        yes_no(info.load_session)
                    ),
                );
                // Opening a session (no prompt) also catches a signed-out agent CLI.
                eprintln!("… asking `{name}` for its models (starts it; no prompt, no cost)");
                match agents.models(name).await {
                    Ok(models) => check(
                        true,
                        format!(
                            "agent `{name}` models: {}",
                            if models.is_empty() {
                                "(no list)".to_string()
                            } else {
                                models
                                    .iter()
                                    .map(|m| m.id.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            }
                        ),
                    ),
                    Err(e) => check(false, format!("agent `{name}`: {e}")),
                }
            }
            Err(e) => {
                let mut message = format!("agent `{name}`: {e}");
                // An interrupted first `npx` download leaves truncated files behind.
                let text = e.to_string();
                if text.contains("_npx") && text.contains("SyntaxError")
                    || text.contains("Unexpected end of input")
                {
                    message.push_str(
                        "\n  hint: the npx cache looks corrupt; delete ~/.npm/_npx and retry",
                    );
                } else if matches!(e, chillax_agents::HandshakeError::Timeout(_))
                    && agent.command == "npx"
                {
                    message.push_str("\n  hint: the first run downloads the adapter; retry with a longer --timeout");
                }
                check(false, message)
            }
        }
    }
    if problems == 0 {
        println!("All good. Run `chillax`.");
        Ok(())
    } else {
        bail!("{problems} problem(s) found")
    }
}

fn yes_no(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

/// Log to `.chillax/state/chillax.log`. Never to stdout while the TUI owns the terminal.
fn init_logging(project: &Project, also_stderr: bool) -> Result<()> {
    use tracing_subscriber::EnvFilter;
    std::fs::create_dir_all(project.state_dir())?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(project.state_dir().join("chillax.log"))?;
    let filter = EnvFilter::try_from_env("CHILLAX_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false);
    if also_stderr {
        builder.with_writer(std::io::stderr).init();
    } else {
        builder.with_writer(Mutex::new(file)).init();
    }
    install_panic_logger();
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        pid = std::process::id(),
        "chillax starting"
    );
    Ok(())
}

/// Send every panic, on any thread or task, to the log with its location and a
/// backtrace. Installed before the TUI wraps the hook, so the terminal is restored
/// first and the crash is still traceable in `chillax.log` afterwards.
fn install_panic_logger() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map_or_else(|| "unknown".to_string(), |l| l.to_string());
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "non-string panic payload".into());
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!(%location, %message, "panic\n{backtrace}");
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("chillax").chain(args.iter().copied()))
    }

    #[test]
    fn herdr_open_and_transcript_parse() {
        let Some(Cmd::Herdr {
            command: herdr::HerdrCmd::Open(args),
        }) = parse(&["herdr", "open", "pat", "--transcript", "--split", "down"])
            .expect("parses")
            .command
        else {
            panic!("not herdr open");
        };
        assert_eq!(args.employee.as_deref(), Some("pat"));
        assert!(args.transcript && !args.app);
        assert!(parse(&["herdr", "open", "pat", "--shell", "--app"]).is_err());
        assert!(parse(&["herdr", "open", "pat", "--tab", "--split", "right"]).is_err());
        assert!(matches!(
            parse(&["transcript", "pat", "--follow", "--since", "9"])
                .expect("parses")
                .command,
            Some(Cmd::Transcript {
                follow: true,
                since: 9,
                ..
            })
        ));
    }

    #[test]
    fn projects_takes_an_optional_json_flag() {
        assert!(matches!(
            parse(&["projects"]).expect("parses").command,
            Some(Cmd::Projects { json: false })
        ));
        assert!(matches!(
            parse(&["projects", "--json"]).expect("parses").command,
            Some(Cmd::Projects { json: true })
        ));
    }

    /// Project B's `hire` must never be posted to project A's server on B's port.
    #[tokio::test]
    async fn commands_are_not_posted_to_another_projects_server() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let posts = Arc::new(AtomicUsize::new(0));
        let counter = posts.clone();
        let app = axum::Router::new()
            .route(
                "/api/v1/health",
                axum::routing::get(|| async {
                    axum::Json(serde_json::json!({
                        "status": "ok",
                        "project": {"name": "A", "root": "/somewhere/else"}
                    }))
                }),
            )
            .route(
                "/api/v1/commands",
                axum::routing::post(move || {
                    counter.fetch_add(1, Ordering::SeqCst);
                    async { axum::Json(serde_json::json!({"events": []})) }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move { axum::serve(listener, app).await });

        let dir = tempfile::tempdir().expect("tempdir");
        let dot = dir.path().join(".chillax");
        std::fs::create_dir_all(dot.join("state")).expect("state dir");
        std::fs::write(
            dot.join("chillax.toml"),
            format!("[project]\nname = \"b\"\n[server]\nport = {port}\n"),
        )
        .expect("config");
        let project = Project::load(dir.path()).expect("loads");
        let cmd = Command::SubmitRequest {
            text: "x".into(),
            start_now: false,
        };
        // No registry entry exists for this fresh directory, so only the configured
        // port is looked at, and it answers as a different project.
        send_offline_or_live(&project, cmd).await.expect("offline");
        assert_eq!(posts.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn env_restart_names_an_environment() {
        let Some(Cmd::Env { retry, restart }) =
            parse(&["env", "--restart", "qa"]).expect("env").command
        else {
            panic!("not an env command");
        };
        assert!(!retry);
        assert_eq!(restart.as_deref(), Some("qa"));
        assert!(parse(&["env", "--retry", "--restart", "qa"]).is_err());
    }

    #[test]
    fn qa_and_release_commands_parse() {
        assert!(matches!(
            parse(&["promote"]).expect("promote").command,
            Some(Cmd::Promote)
        ));
        assert!(matches!(
            parse(&["env", "--retry"]).expect("env").command,
            Some(Cmd::Env {
                retry: true,
                restart: None
            })
        ));
        let Some(Cmd::Release {
            version,
            notes_file,
            force,
            yes,
            reject,
        }) = parse(&[
            "release",
            "--version",
            "v1.0.0",
            "--notes-file",
            "n.md",
            "--force",
            "--yes",
        ])
        .expect("release")
        .command
        else {
            panic!("not a release command");
        };
        assert_eq!(version.as_deref(), Some("v1.0.0"));
        assert_eq!(notes_file.as_deref(), Some(Path::new("n.md")));
        assert!(force && yes && reject.is_none());
    }

    #[test]
    fn request_takes_text_and_now() {
        let Some(Cmd::Request { text, now }) = parse(&["request", "Add dark mode", "--now"])
            .expect("request")
            .command
        else {
            panic!("not a request");
        };
        assert_eq!(text, "Add dark mode");
        assert!(now);
        assert!(parse(&["request"]).is_err());
    }

    #[test]
    fn set_agent_takes_an_employee_and_agent_flags() {
        let Some(Cmd::SetAgent {
            employee,
            backend,
            model,
            effort,
            when_idle,
        }) = parse(&[
            "set-agent",
            "pat",
            "--backend",
            "codex",
            "--model",
            "gpt-5",
            "--effort",
            "high",
            "--when-idle",
        ])
        .expect("set-agent")
        .command
        else {
            panic!("not set-agent");
        };
        assert_eq!(employee, "pat");
        assert!(when_idle);
        assert_eq!(
            (backend.as_deref(), model.as_deref(), effort.as_deref()),
            (Some("codex"), Some("gpt-5"), Some("high"))
        );
        assert!(parse(&["set-agent"]).is_err());
    }

    #[test]
    fn rejecting_cannot_also_set_a_version() {
        assert!(parse(&["release", "--reject", "not yet"]).is_ok());
        assert!(parse(&["release", "--reject", "no", "--version", "v1.0.0"]).is_err());
    }
}
