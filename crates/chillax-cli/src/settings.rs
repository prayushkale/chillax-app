//! `chillax settings` (the default agent, model and effort for new hires), `chillax fire`, and
//! the agent question `chillax hire` asks.

use std::io::IsTerminal;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chillax_agents::shared_backends;
use chillax_core::agents::{backend_title, known_presets};
use chillax_core::engine::State;
use chillax_core::{AgentDirectory, Command, CompanyHandle, HiringDefaults};

use crate::company::{Company, directory, live_url};
use crate::hire::{ask, paint};
use crate::project::Project;
use crate::remote::RemoteAgents;
use crate::send_offline_or_live;

/// The running server's agent directory, or a local one over `chillax.toml` (also when
/// the server predates `/api/v1/agents`).
pub async fn agents(project: &Project) -> Arc<dyn AgentDirectory> {
    if let Some(url) = live_url(project).await
        && serves_agents(&url).await
    {
        return Arc::new(RemoteAgents::new(&url));
    }
    directory(
        project,
        shared_backends(project.config.agents.backends.clone()),
    )
}

async fn serves_agents(url: &str) -> bool {
    let Ok(resp) = reqwest::get(format!("{url}/api/v1/agents")).await else {
        return false;
    };
    resp.status().is_success()
        && resp
            .json::<serde_json::Value>()
            .await
            .is_ok_and(|v| v.is_array())
}

/// Company state from the running server, or replayed from the event log.
pub async fn company_state(project: &Project) -> Result<State> {
    if let Some(url) = live_url(project).await {
        return reqwest::get(format!("{url}/api/v1/state"))
            .await?
            .json()
            .await
            .context("reading the running server's state");
    }
    Ok(Company::open(project).await?.snapshot().await)
}

/// `claude · sonnet · high`, or `claude · agent's default model`.
fn label(backend: Option<&str>, model: Option<&str>, effort: Option<&str>) -> String {
    let backend = backend.unwrap_or("each role's own agent");
    let mut label = match model {
        Some(model) => format!("{backend} · {model}"),
        None => format!("{backend} · agent's default model"),
    };
    if let Some(effort) = effort {
        label.push_str(&format!(" · {effort}"));
    }
    label
}

/// Make sure `backend` can be hired on, adding a known preset to the config if needed.
async fn ensure_backend(agents: &dyn AgentDirectory, backend: &str) -> Result<()> {
    let listed = agents.backends().await;
    match listed.iter().find(|b| b.name == backend) {
        Some(b) if b.configured => Ok(()),
        _ if known_presets().iter().any(|(name, ..)| *name == backend) => {
            agents.enable(backend).await?;
            println!("Added [agents.{backend}] to .chillax/chillax.toml.");
            Ok(())
        }
        _ => {
            let names: Vec<_> = listed.iter().map(|b| b.name.as_str()).collect();
            bail!(
                "no agent backend `{backend}` (available: {}); add it under [agents.{backend}] \
                 in .chillax/chillax.toml",
                names.join(", ")
            )
        }
    }
}

/// The agent, model and effort for a hire. Flags win; in a terminal without flags the
/// stakeholder confirms the default or types another `backend[/model[/effort]]`.
pub async fn choose_agent(
    project: &Project,
    backend: Option<String>,
    model: Option<String>,
    effort: Option<String>,
) -> Result<(Option<String>, Option<String>, Option<String>)> {
    choose_agent_from(project, backend, model, effort, None).await
}

/// Like [`choose_agent`]; `current` is the label shown as the default in the prompt
/// instead of the hiring defaults (used when changing an existing hire).
async fn choose_agent_from(
    project: &Project,
    backend: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    current: Option<String>,
) -> Result<(Option<String>, Option<String>, Option<String>)> {
    let agents = agents(project).await;
    if let Some(b) = &backend {
        ensure_backend(agents.as_ref(), b).await?;
    }
    if backend.is_some() || model.is_some() || effort.is_some() || !std::io::stdin().is_terminal() {
        return Ok((backend, model, effort));
    }
    let default_label = match current {
        Some(current) => current,
        None => {
            let defaults = company_state(project).await?.hiring;
            label(
                defaults.backend.as_deref(),
                defaults.model.as_deref(),
                defaults.effort.as_deref(),
            )
        }
    };
    loop {
        let answer = ask(&format!(
            "Agent [{default_label}] — Enter keeps it, or type `backend`, `backend/model` or \
             `backend/model/effort` (`?` lists): "
        ))
        .await?;
        let answer = answer.trim();
        if answer.is_empty() {
            return Ok((None, None, None));
        }
        if answer == "?" {
            print_agents(agents.as_ref(), true).await;
            continue;
        }
        let mut parts = answer.splitn(3, '/').map(str::trim);
        let b = parts.next().unwrap_or_default();
        let mut part = || parts.next().filter(|p| !p.is_empty()).map(str::to_string);
        let (m, e) = (part(), part());
        match ensure_backend(agents.as_ref(), b).await {
            Ok(()) => return Ok((Some(b.to_string()), m, e)),
            Err(err) => eprintln!("{}", paint("31", &format!("✗ {err:#}"))),
        }
    }
}

async fn print_agents(agents: &dyn AgentDirectory, with_models: bool) {
    for b in agents.backends().await {
        let note = if !b.configured {
            " (installed; picking it adds it to chillax.toml)".to_string()
        } else if !b.available {
            format!(" (`{}` not found on PATH)", b.command)
        } else {
            String::new()
        };
        println!("  {:<10} {}{note}", b.name, b.title);
        if with_models && b.configured && b.available {
            eprintln!(
                "… asking {} for its models (starts it; no prompt, no cost)",
                b.title
            );
            match agents.models(&b.name).await {
                Ok(models) if models.is_empty() => println!("             (no model list)"),
                Ok(models) => {
                    for m in models {
                        println!("             {:<12} {}", m.id, m.name);
                    }
                }
                Err(err) => println!("             {}", paint("31", &format!("✗ {err}"))),
            }
            // Probed with the agent's default model: other models may differ.
            if let Ok(efforts) = agents.efforts(&b.name).await
                && !efforts.is_empty()
            {
                let ids: Vec<_> = efforts.iter().map(|e| e.id.as_str()).collect();
                println!("             effort: {}", ids.join(", "));
            }
        }
    }
}

pub struct SettingsArgs {
    pub backend: Option<String>,
    pub model: Option<String>,
    pub default_model: bool,
    pub effort: Option<String>,
    pub default_effort: bool,
    pub models: bool,
}

pub async fn settings(project: &Project, args: SettingsArgs) -> Result<()> {
    let agents = agents(project).await;
    let current = company_state(project).await?.hiring;
    let changing = args.backend.is_some()
        || args.model.is_some()
        || args.default_model
        || args.effort.is_some()
        || args.default_effort;
    if changing {
        if let Some(b) = &args.backend {
            ensure_backend(agents.as_ref(), b).await?;
        }
        let backend = args.backend.clone().or(current.backend.clone());
        if backend.is_none() {
            bail!("pick a backend too, e.g. `chillax settings --backend claude --model sonnet`");
        }
        // A model only makes sense on the backend it was picked for.
        let model = if args.default_model {
            None
        } else if args.model.is_some() {
            args.model.clone()
        } else if backend == current.backend {
            current.model.clone()
        } else {
            None
        };
        // An effort only makes sense for the backend and model it was picked for.
        let effort = if args.default_effort {
            None
        } else if args.effort.is_some() {
            args.effort.clone()
        } else if backend == current.backend && model == current.model {
            current.effort.clone()
        } else {
            None
        };
        let cmd = Command::SetHiringDefaults {
            backend,
            model,
            effort,
        };
        for summary in send_offline_or_live(project, cmd).await? {
            println!("{}", summary["summary"].as_str().unwrap_or_default());
        }
    }
    let HiringDefaults {
        backend,
        model,
        effort,
    } = company_state(project).await?.hiring;
    println!("Hiring defaults (new hires and persona revisions):");
    println!(
        "  agent: {}",
        backend.as_deref().map_or_else(
            || "each role's own (claude unless the role says otherwise)".to_string(),
            |b| format!("{} ({b})", backend_title(b))
        )
    );
    println!(
        "  model: {}",
        model.as_deref().unwrap_or("the agent's default")
    );
    println!(
        "  effort: {}",
        effort.as_deref().unwrap_or("the agent's default")
    );
    println!("\nAgents:");
    print_agents(agents.as_ref(), args.models).await;
    println!(
        "\nChange with `chillax settings --backend <agent> [--model <id> | --default-model] [--effort <level> | --default-effort]`{}.",
        if args.models {
            ""
        } else {
            "; list models and efforts with --models"
        }
    );
    Ok(())
}

/// Fire (let go of) an employee by id or name.
pub async fn fire(project: &Project, who: &str, yes: bool) -> Result<()> {
    let state = company_state(project).await?;
    let employee = find_employee(&state, who)?;
    let e = &employee.employee;
    if !yes {
        if !std::io::stdin().is_terminal() {
            bail!("pass --yes to fire {} without a terminal", e.name);
        }
        let tickets = state.assignments(&e.id).len();
        let answer = ask(&format!(
            "Fire {} ({})? Their session ends{}. [y/N] ",
            e.name,
            e.role_title,
            if tickets == 0 {
                String::new()
            } else {
                format!(" and {tickets} ticket(s) go back to the backlog")
            }
        ))
        .await?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            println!("{} stays.", e.name);
            return Ok(());
        }
    }
    let cmd = Command::LetGo {
        employee: e.id.clone(),
    };
    for summary in send_offline_or_live(project, cmd).await? {
        println!("{}", summary["summary"].as_str().unwrap_or_default());
    }
    Ok(())
}

/// Hire another employee just like `who` (id first, then case-insensitive name). The
/// agent is copied, so the hiring defaults stay as they are; `name` overrides the
/// engine's pick.
pub async fn duplicate(project: &Project, who: &str, name: Option<String>) -> Result<()> {
    let state = company_state(project).await?;
    let wanted = who.trim().to_lowercase();
    let source = match state.employees.iter().find(|e| e.employee.id == wanted) {
        Some(e) => e,
        None => {
            let by_name: Vec<_> = state
                .employees
                .iter()
                .filter(|e| e.employee.name.to_lowercase() == wanted)
                .collect();
            match by_name.as_slice() {
                [one] => one,
                [] => find_employee(&state, who)?,
                many => bail!(
                    "`{who}` matches {} employees ({}); use an id",
                    many.len(),
                    many.iter()
                        .map(|e| e.employee.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        }
    };
    let mut cmd = source.employee.duplicate_command();
    if let Command::HireEmployee { name: n, .. } = &mut cmd {
        *n = name;
    }
    for event in send_offline_or_live(project, cmd).await? {
        println!("{}", event["summary"].as_str().unwrap_or_default());
    }
    Ok(())
}

/// Find an employee by id or name, listing the team when nobody matches.
pub(crate) fn find_employee<'a>(
    state: &'a State,
    who: &str,
) -> Result<&'a chillax_core::engine::EmployeeView> {
    let wanted = who.trim().to_lowercase();
    let Some(employee) = state
        .employees
        .iter()
        .find(|e| e.employee.id == wanted || e.employee.name.to_lowercase() == wanted)
    else {
        let team: Vec<_> = state
            .employees
            .iter()
            .map(|e| e.employee.id.as_str())
            .collect();
        bail!(
            "nobody called `{who}` on the team{}",
            if team.is_empty() {
                String::new()
            } else {
                format!(" (team: {})", team.join(", "))
            }
        );
    };
    Ok(employee)
}

/// Change the agent of an existing hire. Without flags, a terminal prompts with the
/// current agent as the default. A model or effort not given stays as it is while the
/// backend stays the same; a new backend starts from its own defaults.
pub async fn set_agent(
    project: &Project,
    who: &str,
    backend: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    when_idle: bool,
) -> Result<()> {
    let state = company_state(project).await?;
    let e = find_employee(&state, who)?.employee.clone();
    let flags = backend.is_some() || model.is_some() || effort.is_some();
    if !flags && !std::io::stdin().is_terminal() {
        bail!(
            "pass --backend, --model or --effort to change {}'s agent",
            e.name
        );
    }
    let current = label(Some(&e.backend), e.model.as_deref(), e.effort.as_deref());
    let (backend, model, effort) =
        choose_agent_from(project, backend, model, effort, Some(current)).await?;
    if backend.is_none() && model.is_none() && effort.is_none() {
        println!("{} stays on the same agent.", e.name);
        return Ok(());
    }
    let same_backend = backend.as_deref().is_none_or(|b| b == e.backend);
    let model = model.or_else(|| e.model.clone().filter(|_| same_backend));
    let effort = effort.or_else(|| {
        e.effort
            .clone()
            .filter(|_| same_backend && model == e.model)
    });
    let cmd = Command::SetEmployeeAgent {
        employee: e.id,
        when_idle,
        backend,
        model,
        effort,
    };
    let events = send_offline_or_live(project, cmd).await?;
    if events.is_empty() {
        println!("{} already runs on that agent.", e.name);
    }
    for summary in events {
        println!("{}", summary["summary"].as_str().unwrap_or_default());
    }
    Ok(())
}
