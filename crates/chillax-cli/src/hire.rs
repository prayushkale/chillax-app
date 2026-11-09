//! The persona catalog from the command line: `chillax roles --search` and
//! `chillax hire <persona>` with review, agent revisions and confirmation.

use std::io::{IsTerminal, Write};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chillax_core::{
    Actor, Command, CompanyHandle, DiffOp, Hub, PersonaSummary, Recruiter, Role, RoleDraft,
    Severity,
};

use crate::company::{Company, live_url, recruiter};
use crate::project::Project;
use crate::remote::RemoteRecruiter;

/// The recruiter to use: the running server's, or a local one over the event log.
pub struct Catalog {
    pub recruiter: Arc<dyn Recruiter>,
    /// Set when running locally; commands go straight to this hub.
    pub hub: Option<Arc<Hub>>,
}

impl Catalog {
    pub async fn open(project: &Project) -> Result<Self> {
        if let Some(url) = live_url(project).await {
            return Ok(Self {
                recruiter: Arc::new(RemoteRecruiter::new(&url)),
                hub: None,
            });
        }
        let hub = Company::open(project).await?;
        let backends = chillax_agents::shared_backends(project.config.agents.backends.clone());
        let local = recruiter(project, hub.clone(), backends).await;
        if project.config.catalog.enabled && local.status().await.commit.is_none() {
            eprintln!("Fetching the {} catalog…", project.config.catalog.name);
            local.refresh().await?;
        }
        Ok(Self {
            recruiter: local,
            hub: Some(hub),
        })
    }

    async fn send(&self, project: &Project, cmd: Command) -> Result<Vec<String>> {
        match &self.hub {
            Some(hub) => Ok(hub
                .send(Actor::Stakeholder, cmd)
                .await?
                .into_iter()
                .map(|e| e.kind.summary())
                .collect()),
            None => Ok(crate::send_offline_or_live(project, cmd)
                .await?
                .into_iter()
                .map(|e| e["summary"].as_str().unwrap_or_default().to_string())
                .collect()),
        }
    }
}

/// `chillax roles` catalog section: featured personas, or search results.
pub async fn list_personas(project: &Project, query: Option<&str>, refresh: bool) -> Result<()> {
    let catalog = Catalog::open(project).await?;
    if refresh {
        let status = catalog.recruiter.refresh().await?;
        eprintln!(
            "Refreshed {} @ {} ({} personas)",
            status.name,
            short(status.commit.as_deref()),
            status.personas
        );
    }
    let (heading, personas) = match query {
        Some(q) => (
            format!("Personas matching \"{q}\""),
            catalog.recruiter.search(q, 50).await?,
        ),
        None => {
            let status = catalog.recruiter.status().await;
            (
                format!(
                    "Featured from {} @ {} ({} personas; search with --search)",
                    status.name,
                    short(status.commit.as_deref()),
                    status.personas
                ),
                catalog.recruiter.featured(10).await?,
            )
        }
    };
    println!("\n{heading}");
    if personas.is_empty() {
        println!("  (none)");
    }
    for p in &personas {
        println!("{}", persona_row(p));
    }
    println!("\nHire one with `chillax hire <persona>` (add `--base <role>` to pick its policy).");
    Ok(())
}

fn persona_row(p: &PersonaSummary) -> String {
    let mut extra = String::new();
    if let Some(role) = &p.adopted_role {
        extra.push_str(&format!(" [adopted as {role}]"));
    }
    if p.hires > 0 {
        extra.push_str(&format!(" [hired {}×]", p.hires));
    }
    format!(
        "{:<40} {:<18} {}{extra}",
        p.slug,
        p.division,
        clip(&p.description, 70)
    )
}

pub struct HireArgs {
    /// Catalog persona slug; empty with `from`.
    pub persona: String,
    /// Copy this existing role into a new one (`--from`), needs `role_id`.
    pub from: Option<String>,
    pub title: Option<String>,
    pub name: Option<String>,
    pub base: Option<String>,
    pub backend: Option<String>,
    pub model: Option<String>,
    pub role_id: Option<String>,
    pub yes: bool,
    /// Tailor the persona to the project's plan files before the review.
    pub tailor: bool,
}

/// Review a catalog persona, optionally have an agent revise it, adopt it, and hire.
pub async fn hire_persona(project: &Project, args: HireArgs) -> Result<()> {
    let interactive = std::io::stdin().is_terminal();
    if !interactive && !args.yes {
        bail!(
            "hiring a catalog persona needs a review; run this in a terminal, or pass --yes \
             to accept the persona as is (its findings are still printed)"
        );
    }
    let catalog = Catalog::open(project).await?;
    let r = &catalog.recruiter;
    let from_role = args.from.clone();
    if from_role.is_some() && args.base.is_some() {
        bail!("--base cannot be combined with --from (the copy keeps the source role's base)");
    }
    let personas = if from_role.is_some() {
        Vec::new()
    } else {
        r.personas().await?
    };
    let persona = personas.iter().find(|p| p.slug == args.persona);
    if persona.is_none() && from_role.is_none() {
        let close = r
            .search(&args.persona.replace('-', " "), 5)
            .await
            .unwrap_or_default();
        let hint = if close.is_empty() {
            "see `chillax roles --search <words>`".to_string()
        } else {
            format!(
                "did you mean: {}?",
                close
                    .iter()
                    .map(|p| p.slug.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        bail!("no role or persona `{}`; {hint}", args.persona);
    }

    let bases: Vec<_> = project
        .roles
        .iter()
        .filter(|role| role.source.is_none())
        .cloned()
        .collect();
    // Revisions run on the chosen agent too, so ask before drafting.
    // Persona drafts carry no effort, so none is asked for.
    let (backend, model, _) =
        crate::settings::choose_agent(project, args.backend, args.model, None).await?;
    // Without --base the draft starts on the suggested base; it can be changed below.
    let mut draft = match (&from_role, persona) {
        (Some(from), _) => {
            let Some(new_id) = args.role_id.as_deref() else {
                bail!("--from needs --role-id <id> for the new role");
            };
            let title = args.title.clone().unwrap_or_else(|| title_from_id(new_id));
            r.draft_from_role(from, new_id, &title, backend, model)
                .await?
        }
        (None, Some(persona)) => {
            r.draft(&persona.slug, args.base.as_deref(), backend, model)
                .await?
        }
        (None, None) => unreachable!("checked above"),
    };
    if args.tailor {
        eprintln!("{}", tailoring_note(&draft));
        draft = r.tailor(&draft.id).await?;
    }
    print_review(&draft);
    if args.tailor {
        print_diff(&draft);
    }
    if interactive {
        loop {
            let line = ask(&format!(
                "\nSuggest a change for the {} agent to make (Enter to continue, `u` to undo, \
                 `t` to tailor to the project, `b` to change the base role): ",
                draft.backend
            ))
            .await?;
            let line = line.trim();
            if line.is_empty() {
                break;
            }
            let result = if line == "u" {
                r.undo(&draft.id).await
            } else if line == "t" {
                eprintln!("{}", tailoring_note(&draft));
                r.tailor(&draft.id).await
            } else if line == "b" || line.starts_with("b ") {
                let base = match line[1..].trim() {
                    "" => pick_base(&bases, &draft.base_role).await?,
                    id => id.to_string(),
                };
                r.set_base(&draft.id, &base).await.inspect(|d| {
                    println!("Base role is now {}.", base_label(&bases, &d.base_role));
                })
            } else {
                eprintln!(
                    "Revising (no tools, no repo access; this uses your agent and can take a minute)…"
                );
                r.revise(&draft.id, line).await
            };
            match result {
                Ok(next) => {
                    draft = next;
                    print_diff(&draft);
                }
                Err(err) => eprintln!("{}", paint("31", &format!("✗ {err}"))),
            }
        }
    }

    let role_id = args.role_id.unwrap_or_else(|| draft.role_id.clone());
    let overwrite = persona.is_some_and(|p| p.adopted_role.as_deref() == Some(role_id.as_str()));
    let name = args.name.unwrap_or_else(|| draft.persona.name.clone());
    if interactive && !args.yes {
        if overwrite {
            println!("This replaces the role you adopted earlier as `{role_id}`.");
        }
        let answer = ask(&format!(
            "\nUse this prompt for {name} as role `{role_id}` (saved to .chillax/roles/{role_id}.md)? [y/N] "
        ))
        .await?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            r.discard(&draft.id).await;
            println!("Not hired.");
            return Ok(());
        }
    }
    let role = r.adopt(&draft.id, Some(role_id), overwrite).await?;
    println!("Adopted role `{}` ({}).", role.id, role.title);
    let summaries = catalog
        .send(
            project,
            Command::HireEmployee {
                role: role.id,
                name: Some(name),
                backend: Some(draft.backend.clone()),
                model: draft.model.clone(),
                effort: None,
                keep_defaults: false,
            },
        )
        .await?;
    for summary in summaries {
        println!("{summary}");
    }
    Ok(())
}

/// What tailoring reads and what it costs, said before the agent runs.
fn tailoring_note(draft: &RoleDraft) -> String {
    format!(
        "Tailoring to the project from {} (no tools; this uses your agent and can take a \
         minute)…",
        if draft.project_files.is_empty() {
            "its plan files".to_string()
        } else {
            draft.project_files.join(", ")
        }
    )
}

/// "senior-dev" -> "Senior Dev", for a new role given only an id.
fn title_from_id(id: &str) -> String {
    id.split('-')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            c.next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Ask which trusted role a persona should inherit its policy from.
async fn pick_base(bases: &[Role], current: &str) -> Result<String> {
    println!(
        "The base role gives the persona its tools, permissions and workflow states \
         (the persona itself only adds text):"
    );
    for (i, role) in bases.iter().enumerate() {
        println!(
            "  {}{}. {:<24} tools: {}",
            if role.id == current { "*" } else { " " },
            i + 1,
            role.title,
            if role.mcp_tools.is_empty() {
                "none".into()
            } else {
                role.mcp_tools.join(", ")
            }
        );
    }
    let pick = ask("Base role [number]: ").await?;
    let n: usize = pick
        .trim()
        .parse()
        .context("enter one of the numbers above")?;
    bases
        .get(n.wrapping_sub(1))
        .map(|role| role.id.clone())
        .context("enter one of the numbers above")
}

fn base_label(bases: &[Role], id: &str) -> String {
    bases
        .iter()
        .find(|r| r.id == id)
        .map_or_else(|| id.to_string(), |r| format!("{} ({id})", r.title))
}

fn print_review(draft: &RoleDraft) {
    let suggested = if draft.persona.suggested_base.as_deref() == Some(draft.base_role.as_str()) {
        ", suggested"
    } else {
        ""
    };
    println!(
        "\n{} — {} (base role: {}{suggested}, agent: {}{})",
        paint("1", &draft.persona.name),
        draft.persona.path,
        draft.base_role,
        draft.backend,
        draft
            .model
            .as_deref()
            .map(|m| format!(" · {m}"))
            .unwrap_or_default()
    );
    if !draft.project_files.is_empty() {
        println!(
            "The prompt points the hire at: {}",
            draft.project_files.join(", ")
        );
    }
    print_findings(draft);
    println!("\n{}", paint("2", "──── persona prompt ────"));
    for (i, line) in draft.persona_text.lines().enumerate() {
        let flag = draft
            .findings
            .iter()
            .filter(|f| f.line == i + 1)
            .map(|f| f.severity)
            .max();
        let text = format!("{:>4} {line}", i + 1);
        match flag {
            Some(Severity::Warn) => println!("{}", paint("33", &format!("⚠{text}"))),
            Some(Severity::Info) => println!("·{text}"),
            None => println!(" {text}"),
        }
    }
    println!("{}", paint("2", "──── end ────"));
}

fn print_findings(draft: &RoleDraft) {
    let warns: Vec<_> = draft
        .findings
        .iter()
        .filter(|f| f.severity == Severity::Warn)
        .collect();
    let notes = draft.findings.len() - warns.len();
    if warns.is_empty() {
        println!(
            "{}",
            paint(
                "32",
                &format!("✓ No suspicious instructions found ({notes} notes). Read it anyway.")
            )
        );
    } else {
        println!(
            "{}",
            paint(
                "33",
                &format!("⚠ {} warning(s), {notes} notes:", warns.len())
            )
        );
        for f in warns {
            println!("  line {}: {} — {}", f.line, f.message, f.excerpt);
        }
    }
}

fn print_diff(draft: &RoleDraft) {
    println!("\n{}", paint("2", "──── changes from the original ────"));
    for line in &draft.diff {
        match line.op {
            DiffOp::Same => {}
            DiffOp::Added => println!("{}", paint("32", &format!("+ {}", line.text))),
            DiffOp::Removed => println!("{}", paint("31", &format!("- {}", line.text))),
        }
    }
    if let Some(last) = draft.revisions.last()
        && !last.summary.is_empty()
    {
        println!(
            "{}\n{}",
            paint("2", "What the agent changed:"),
            last.summary
        );
    }
    print_findings(draft);
}

/// Read one line from stdin without blocking the async runtime.
pub(crate) async fn ask(prompt: &str) -> Result<String> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).map(|_| line)
    })
    .await?
    .context("reading from the terminal")
}

/// ANSI styling, only when stdout is a terminal.
pub(crate) fn paint(code: &str, text: &str) -> String {
    if std::io::stdout().is_terminal() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

fn short(commit: Option<&str>) -> &str {
    commit.map_or("—", |c| &c[..c.len().min(7)])
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() > max {
        let cut: String = text.chars().take(max - 1).collect();
        format!("{cut}…")
    } else {
        text.to_string()
    }
}
