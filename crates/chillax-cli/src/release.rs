//! `chillax promote | release | env`: drive QA and the release gate from
//! the shell. Commands go through the running server when there is one, otherwise
//! straight into the event store, where they wait for the dispatcher.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use chillax_core::engine::State;
use chillax_core::{Command, Config, QaRound, QaRoundStatus, QaVerdict, Release, ReleaseStatus};

use crate::company::live_url;
use crate::hire::ask;
use crate::project::Project;
use crate::send_offline_or_live;
use crate::settings::company_state;

#[derive(Debug, Default)]
pub struct ReleaseArgs {
    pub version: Option<String>,
    pub notes_file: Option<PathBuf>,
    pub force: bool,
    pub yes: bool,
    pub reject: Option<String>,
}

/// The release waiting at the `release` gate.
pub fn gated(state: &State) -> Option<&Release> {
    state
        .releases
        .iter()
        .find(|r| r.status == ReleaseStatus::Gated)
}

/// The command that approves `release`, with the stakeholder's version and notes when
/// given and the proposed ones otherwise.
pub fn approval(
    release: &Release,
    version: Option<&str>,
    notes_file: Option<&Path>,
) -> Result<Command> {
    let notes = match notes_file {
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("reading release notes from {}", path.display()))?,
        None => release.notes.clone(),
    };
    Ok(Command::ApproveRelease {
        id: release.id.clone(),
        version: version.unwrap_or(&release.version).trim().to_string(),
        notes,
    })
}

/// What `chillax env` prints: where the code is on its way from dev to prod.
pub fn env_report(state: &State, config: &Config) -> Vec<String> {
    let env = &config.env;
    let open = state
        .tickets
        .iter()
        .filter(|t| t.branch.is_some() && t.state != "done" && !t.checks.contains("merged_to_dev"))
        .count();
    let mut dev = format!(
        "dev      {:<8} open branches: {open} · waiting for QA: {}",
        env.dev.base_branch,
        state.waiting_for_qa().len()
    );
    if state.qa_requested {
        dev.push_str(" · promotion requested");
    }
    let qa = match &state.qa {
        None => format!("qa       {:<8} no round yet", env.qa.branch),
        Some(round) => format!(
            "qa       {:<8} round {} {} @{} · {}",
            env.qa.branch,
            round.number,
            round_status(round),
            round.commit.chars().take(7).collect::<String>(),
            verdicts(round)
        ),
    };
    let last = state.releases.iter().rev().find(|r| {
        matches!(
            r.status,
            ReleaseStatus::Tagged | ReleaseStatus::Deployed | ReleaseStatus::DeployFailed
        )
    });
    let prod = match last {
        None => format!("prod     {:<8} nothing released yet", env.prod.branch),
        Some(r) => format!(
            "prod     {:<8} {} {}",
            env.prod.branch,
            r.tag.as_deref().unwrap_or(&r.version),
            release_status(r)
        ),
    };
    let mut lines = vec![dev, qa, prod];
    lines.extend(app_lines(state, config));
    if let Some(r) = state.open_release() {
        let mut line = format!(
            "release  {:<8} {} ({} tickets",
            r.version,
            release_status(r),
            r.tickets.len()
        );
        if !r.known_issues.is_empty() {
            line.push_str(&format!(", known issues: {}", r.known_issues.len()));
        }
        line.push(')');
        lines.push(line);
    }
    for item in &state.inbox {
        if let chillax_core::InboxItem::EnvAlert { env, step, id, .. } = item {
            lines.push(format!(
                "! {id} {env} {step:?} failed: `chillax env --retry`"
            ));
        }
    }
    lines
}

/// One line per app Chillax runs (ADR 0015): status, address, commit and why it is down.
fn app_lines(state: &State, config: &Config) -> Vec<String> {
    if state.apps.is_empty() {
        // Chillax has not run since the apps were configured.
        return config
            .env
            .apps()
            .iter()
            .map(|a| format!("app {:<5} not started yet  {}", a.env, a.url))
            .collect();
    }
    state
        .apps
        .iter()
        .map(|a| {
            let mut line = format!("app {:<5} {:<8} {}", a.env, a.status.label(), a.url);
            if let Some(commit) = &a.commit
                && a.status != chillax_core::AppStatus::Stopped
            {
                line.push_str(&format!(" @{}", chillax_core::apps::short(commit)));
            }
            if let Some(reason) = &a.reason {
                line.push_str(&format!(" ({reason})"));
            }
            if !a.wanted {
                line.push_str(" · `chillax env --restart ");
                line.push_str(&a.env);
                line.push_str("` starts it");
            }
            line
        })
        .collect()
}

fn verdicts(round: &QaRound) -> String {
    if round.tickets.is_empty() {
        return "no tickets".into();
    }
    round
        .tickets
        .iter()
        .map(|id| {
            let word = match round.verdicts.get(id) {
                Some(QaVerdict::Pass) => "pass",
                Some(QaVerdict::Fail) => "fail",
                Some(QaVerdict::Env) => "env",
                None => "pending",
            };
            format!("{id} {word}")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn round_status(round: &QaRound) -> &'static str {
    match round.status {
        QaRoundStatus::Starting => "starting",
        QaRoundStatus::Testing => "testing",
        QaRoundStatus::EnvFailed => "env failed",
        QaRoundStatus::Closed => "closed",
    }
}

fn release_status(r: &Release) -> &'static str {
    match r.status {
        ReleaseStatus::Gated => "awaiting approval",
        ReleaseStatus::Approved => "merging and tagging",
        ReleaseStatus::Tagged => "tagged",
        ReleaseStatus::Deployed => "deployed",
        ReleaseStatus::DeployFailed => "deploy failed",
        ReleaseStatus::Rejected => "rejected",
    }
}

/// Without a running Chillax nothing carries a command out; say so.
async fn offline_note(project: &Project) {
    if live_url(project).await.is_none() {
        println!(
            "Chillax is not running: start `chillax` (or `chillax serve --headless`) and the \
             dispatcher carries this out."
        );
    }
}

async fn send_and_print(project: &Project, cmd: Command) -> Result<()> {
    for event in send_offline_or_live(project, cmd).await? {
        println!("{}", event["summary"].as_str().unwrap_or_default());
    }
    Ok(())
}

/// `chillax promote`: promote `dev` to `qa` now.
pub async fn promote(project: &Project) -> Result<()> {
    send_and_print(project, Command::PromoteToQa).await?;
    offline_note(project).await;
    Ok(())
}

/// `chillax request "…" [--now]`: describe a need; the project manager files and assigns it.
pub async fn request(project: &Project, text: &str, start_now: bool) -> Result<()> {
    let text = if text == "-" {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
        buf
    } else {
        text.to_string()
    };
    send_and_print(project, Command::SubmitRequest { text, start_now }).await?;
    if live_url(project).await.is_some() {
        println!("The project manager will file it and assign the tickets; watch the Inbox.");
    } else {
        offline_note(project).await;
    }
    Ok(())
}

/// `chillax env`: status of dev, qa and prod.
pub async fn env(project: &Project) -> Result<()> {
    let state = company_state(project).await?;
    for line in env_report(&state, &project.config) {
        println!("{line}");
    }
    Ok(())
}

/// `chillax env --restart <env>`: start or restart the app of an environment.
pub async fn restart_app(project: &Project, env: &str) -> Result<()> {
    let state = company_state(project).await?;
    let command = match state.app(env) {
        Some(app) if app.can_start() => Command::StartApp { env: env.into() },
        _ => Command::RestartApp { env: env.into() },
    };
    send_and_print(project, command).await.map_err(|e| {
        if state.apps.is_empty() {
            anyhow!(
                "{e}\n(apps are known once Chillax has run with `run` and `port` set; start \
                 `chillax` or `chillax serve --headless` first)"
            )
        } else {
            e
        }
    })?;
    offline_note(project).await;
    Ok(())
}

/// `chillax env --retry`: run the step that failed again.
pub async fn retry(project: &Project) -> Result<()> {
    let state = company_state(project).await?;
    let alert = state.inbox.iter().find_map(|item| match item {
        chillax_core::InboxItem::EnvAlert { env, step, .. } => Some((env.clone(), *step)),
        _ => None,
    });
    let (env, step) = match (alert, state.releases.last()) {
        (Some(failed), _) => failed,
        (None, Some(r)) if r.status == ReleaseStatus::DeployFailed => {
            ("prod".to_string(), chillax_core::EnvStep::Deploy)
        }
        _ => bail!("nothing to retry: no environment step has failed"),
    };
    send_and_print(project, Command::RetryEnv { env, step }).await?;
    offline_note(project).await;
    Ok(())
}

/// `chillax release`: open the gate if needed, then approve (or reject) the candidate.
pub async fn release(project: &Project, args: ReleaseArgs) -> Result<()> {
    let mut state = company_state(project).await?;
    if gated(&state).is_none() {
        send_offline_or_live(project, Command::OpenReleaseGate { force: args.force })
            .await
            .map_err(|e| {
                if args.force || e.to_string().contains("nothing is ready") {
                    e
                } else {
                    anyhow!("{e}\n(`--force` ships what passed and lists the rest as known issues)")
                }
            })?;
        state = company_state(project).await?;
    }
    let Some(release) = gated(&state) else {
        bail!("no release is waiting for approval; see `chillax env`");
    };

    if let Some(reason) = args.reject {
        let cmd = Command::RejectRelease {
            id: release.id.clone(),
            reason,
        };
        return send_and_print(project, cmd).await;
    }

    let cmd = approval(release, args.version.as_deref(), args.notes_file.as_deref())?;
    let Command::ApproveRelease { version, notes, .. } = &cmd else {
        unreachable!("approval builds ApproveRelease");
    };
    println!("Release {version} ({} tickets)", release.tickets.len());
    if !release.known_issues.is_empty() {
        println!("Known issues: {}", release.known_issues.join(", "));
    }
    println!("\n{}\n", notes.trim_end());
    if !args.yes {
        if !std::io::stdin().is_terminal() {
            bail!("pass --yes to approve without a terminal");
        }
        let answer = ask(&format!("Approve {version} and tag it? [Y/n] ")).await?;
        if matches!(answer.trim(), "n" | "N" | "no") {
            println!("Not approved; the release stays at the gate.");
            return Ok(());
        }
    }
    send_and_print(project, cmd).await?;
    offline_note(project).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chillax_core::{EnvStep, InboxItem, Ticket, TicketKind};

    fn release(status: ReleaseStatus) -> Release {
        Release {
            id: "REL-1".into(),
            version: "v0.2.0".into(),
            sprint: None,
            round: 1,
            tickets: vec!["T-1".into(), "T-2".into()],
            known_issues: vec!["T-3".into()],
            notes: "## Stories\n- T-1".into(),
            status,
            tag: None,
            commit: None,
        }
    }

    fn ticket(id: &str, state: &str) -> Ticket {
        Ticket {
            id: id.into(),
            kind: TicketKind::Story,
            title: id.into(),
            description: String::new(),
            acceptance_criteria: vec![],
            estimate: None,
            epic: None,
            state: state.into(),
            assignee: None,
            sprint: None,
            comments: vec![],
            external: None,
            blocked_by: vec![],
            branch: None,
            worktree: None,
            reviewer: None,
            checks: Default::default(),
            needs_rebase: false,
            review_requested: false,
            found_in: None,
            pr_number: None,
            pr_url: None,
            dropped: None,
            urgent: false,
            conflict: None,
        }
    }

    #[test]
    fn only_a_gated_release_can_be_approved() {
        let mut state = State {
            releases: vec![release(ReleaseStatus::Tagged)],
            ..State::default()
        };
        assert!(gated(&state).is_none());
        state.releases.push(release(ReleaseStatus::Gated));
        assert_eq!(gated(&state).map(|r| r.id.as_str()), Some("REL-1"));
    }

    #[test]
    fn approval_defaults_to_the_proposal_and_takes_overrides() {
        let r = release(ReleaseStatus::Gated);
        assert_eq!(
            approval(&r, None, None).expect("default"),
            Command::ApproveRelease {
                id: "REL-1".into(),
                version: "v0.2.0".into(),
                notes: "## Stories\n- T-1".into(),
            }
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("notes.md");
        std::fs::write(&file, "My notes\n").expect("write");
        assert_eq!(
            approval(&r, Some(" v1.0.0 "), Some(&file)).expect("override"),
            Command::ApproveRelease {
                id: "REL-1".into(),
                version: "v1.0.0".into(),
                notes: "My notes\n".into(),
            }
        );
        let err = approval(&r, None, Some(&dir.path().join("missing.md"))).expect_err("no file");
        assert!(format!("{err:#}").contains("missing.md"));
    }

    #[test]
    fn env_report_before_anything_happened() {
        let report = env_report(&State::default(), &Config::default());
        assert_eq!(report.len(), 3, "{report:?}");
        assert!(report[0].starts_with("dev") && report[0].contains("open branches: 0"));
        assert!(report[1].contains("no round yet"));
        assert!(report[2].contains("nothing released yet"));
    }

    #[test]
    fn env_report_shows_the_round_the_candidate_and_failures() {
        let mut state = State::default();
        let mut dev = ticket("T-1", "in_progress");
        dev.branch = Some("feat/t-1".into());
        state.tickets = vec![dev, ticket("T-2", "in_qa")];
        state.qa = Some(QaRound {
            number: 3,
            sprint: None,
            commit: "0123456789".into(),
            status: QaRoundStatus::EnvFailed,
            tickets: vec!["T-2".into(), "T-4".into()],
            verdicts: [("T-2".to_string(), QaVerdict::Pass)].into(),
            env_retries: Default::default(),
            up_done: false,
        });
        state.releases = vec![release(ReleaseStatus::Gated)];
        state.inbox = vec![InboxItem::EnvAlert {
            id: "IN-9".into(),
            env: "qa".into(),
            step: EnvStep::Up,
            output_tail: String::new(),
        }];
        let text = env_report(&state, &Config::default()).join("\n");
        assert!(
            text.contains("open branches: 1 · waiting for QA: 1"),
            "{text}"
        );
        assert!(text.contains("round 3 env failed @0123456"), "{text}");
        assert!(text.contains("T-2 pass, T-4 pending"), "{text}");
        assert!(
            text.contains("awaiting approval (2 tickets, known issues: 1)"),
            "{text}"
        );
        assert!(text.contains("! IN-9 qa Up failed"), "{text}");
    }

    #[test]
    fn env_report_lists_the_apps() {
        use chillax_core::{AppInfo, AppStatus, RunningApp};
        let mut config = Config::default();
        config.env.dev.app.run = Some("npm run dev".into());
        config.env.dev.app.port = Some(3000);
        let before = env_report(&State::default(), &config);
        assert_eq!(before.len(), 4, "{before:?}");
        assert!(before[3].contains("dev   not started yet  http://localhost:3000"));

        let mut state = State::default();
        let info = |env: &str, port: u16| AppInfo {
            env: env.into(),
            port,
            url: format!("http://localhost:{port}"),
        };
        let mut dev = RunningApp::new(&info("dev", 3000));
        dev.status = AppStatus::Running;
        dev.commit = Some("0123456789".into());
        let mut prod = RunningApp::new(&info("prod", 3002));
        prod.reason = Some("no release yet".into());
        let mut qa = RunningApp::new(&info("qa", 3001));
        qa.wanted = false;
        state.apps = vec![dev, qa, prod];
        let report = env_report(&state, &config);
        assert_eq!(report.len(), 6, "{report:?}");
        assert_eq!(
            report[3],
            "app dev   running  http://localhost:3000 @0123456"
        );
        assert!(report[4].contains("chillax env --restart qa"), "{report:?}");
        assert!(
            report[5].contains("stopped  http://localhost:3002 (no release yet)"),
            "{report:?}"
        );
    }

    #[test]
    fn env_report_names_the_last_tag_and_a_failed_deploy() {
        let mut state = State::default();
        let mut r = release(ReleaseStatus::DeployFailed);
        r.tag = Some("v0.2.0".into());
        state.releases = vec![r];
        let report = env_report(&state, &Config::default());
        assert!(report[2].contains("v0.2.0 deploy failed"), "{report:?}");
        assert_eq!(report.len(), 3, "a failed deploy is not a candidate");
    }
}
