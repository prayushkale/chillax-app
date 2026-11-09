//! The dev loop end to end: real git repositories, the real dispatcher and agent
//! runtime, and the scripted `fake-acp-agent` (no model, no credits). The test plays
//! the agents' MCP calls by sending the same commands the tools would.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chillax_agents::{AgentRuntime, McpEndpoint, RuntimeConfig, shared_backends};
use chillax_core::config::WorktreeMode;
use chillax_core::config::{AgentConfig, AppSpec, PermissionMode, ProdEnv, QaEnv};
use chillax_core::engine::State;
use chillax_core::{
    Actor, Command, CompanyHandle, CriterionResult, Engine, Hub, InboxItem, MemoryLog, NewBug,
    QaRoundStatus, QaVerdict, ReleaseStatus, ReviewVerdict, Role, RoleCatalog, SprintStatus,
    TicketKind, Workflow,
};
use chillax_dispatch::{AppsConfig, DispatchConfig, Dispatcher, LiveSettings};
use chillax_vcs::{GithubGit, LocalGit};

/// The scripted agent lives in another package, so find the binary Cargo built.
fn fake_agent() -> PathBuf {
    let exe = std::env::current_exe().expect("test binary path");
    let debug = exe.parent().and_then(Path::parent).expect("target dir");
    let agent = debug.join("fake-acp-agent");
    if !agent.exists() {
        let built = std::process::Command::new("cargo")
            .args(["build", "-p", "chillax-agents", "--bin", "fake-acp-agent"])
            .status()
            .expect("cargo runs");
        assert!(built.success(), "building the fake agent");
    }
    agent
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

struct Env {
    hub: Arc<Hub>,
    runtime: Arc<AgentRuntime>,
    repo: tempfile::TempDir,
    /// The bare remote and the directory of the fake `gh`, for GitHub-mode runs.
    github: Option<(tempfile::TempDir, tempfile::TempDir)>,
    dispatcher: Arc<Dispatcher>,
}

const FAKE_GH: &str = include_str!("../../chillax-vcs/testdata/fake-gh.sh");

fn role(id: &str, tag: &str, owns: &str, max: u32, auto: &str) -> Role {
    Role::parse(&format!(
        "---\nid: {id}\ntitle: {id}\ndefault_backend: fake\nmax_parallel_tasks: {max}\n\
         tags: [{tag}]\nowns_states: [{owns}]\npermissions:\n  auto_approve: [{auto}]\n---\n\
         You are {{{{name}}}}.\n"
    ))
    .expect("valid role")
}

impl Env {
    /// A repo with `dev`, a PM (Pam), reviewer (Lee) and the named developers.
    async fn new(test: Option<&str>, developers: &[&str]) -> Self {
        Self::build(test, developers, None, false, Vec::new()).await
    }

    /// Like [`Env::with_qa`], with a bare `origin` and a fake `gh`: work travels as pull
    /// requests (ADR 0009).
    async fn on_github(test: Option<&str>, developers: &[&str]) -> Self {
        Self::build(
            test,
            developers,
            Some((QaEnv::default(), ProdEnv::default())),
            true,
            Vec::new(),
        )
        .await
    }

    /// Like [`Env::new`], plus a QA engineer (Quinn) and the QA and prod commands.
    async fn with_qa(test: Option<&str>, developers: &[&str], qa: QaEnv, prod: ProdEnv) -> Self {
        Self::build(test, developers, Some((qa, prod)), false, Vec::new()).await
    }

    /// Like [`Env::with_qa`], also running the dev, QA and prod apps (ADR 0015).
    async fn with_apps(developers: &[&str], apps: Vec<AppSpec>) -> Self {
        Self::build(
            None,
            developers,
            Some((QaEnv::default(), ProdEnv::default())),
            false,
            apps,
        )
        .await
    }

    /// Like [`Env::new`] with the `shared` worktree mode (ADR 0025); every other
    /// constructor runs `per-ticket`, the default (ADR 0038).
    async fn shared(test: Option<&str>, developers: &[&str]) -> Self {
        Self::build_with(
            test,
            developers,
            None,
            false,
            Vec::new(),
            WorktreeMode::Shared,
        )
        .await
    }

    async fn build(
        test: Option<&str>,
        developers: &[&str],
        qa: Option<(QaEnv, ProdEnv)>,
        github: bool,
        apps: Vec<AppSpec>,
    ) -> Self {
        Self::build_with(test, developers, qa, github, apps, WorktreeMode::PerTicket).await
    }

    async fn build_with(
        test: Option<&str>,
        developers: &[&str],
        qa: Option<(QaEnv, ProdEnv)>,
        github: bool,
        apps: Vec<AppSpec>,
        mode: WorktreeMode,
    ) -> Self {
        let repo = tempfile::tempdir().expect("tempdir");
        git(repo.path(), &["init", "--quiet", "-b", "main"]);
        std::fs::write(repo.path().join("README.md"), "hello\n").expect("write");
        // What `chillax init` ignores, so Chillax's own files never dirty the checkout.
        std::fs::write(
            repo.path().join(".gitignore"),
            ".chillax/state/\n.chillax/worktrees/\n",
        )
        .expect("write");
        git(repo.path(), &["add", "-A"]);
        git(repo.path(), &["commit", "--quiet", "-m", "initial"]);
        git(repo.path(), &["branch", "dev"]);
        git(repo.path(), &["branch", "qa"]);
        let mut fakes = None;
        let mut vcs: Arc<dyn chillax_core::vcs::VcsHost> =
            Arc::new(LocalGit::new(repo.path(), "dev"));
        if github {
            let remote = tempfile::tempdir().expect("tempdir");
            let bin = tempfile::tempdir().expect("tempdir");
            git(remote.path(), &["init", "--quiet", "--bare", "-b", "main"]);
            let url = remote.path().to_string_lossy().to_string();
            git(repo.path(), &["remote", "add", "origin", &url]);
            git(
                repo.path(),
                &["push", "--quiet", "origin", "main", "dev", "qa"],
            );
            let script = bin.path().join("gh");
            std::fs::write(&script, FAKE_GH).expect("write gh");
            std::fs::write(bin.path().join("remote"), &url).expect("write remote");
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                    .expect("chmod");
            }
            vcs = Arc::new(GithubGit::new(repo.path(), "dev", "origin").with_gh(script));
            fakes = Some((remote, bin));
        }

        let roles = [
            role("pm", "manager", "backlog, ready", 1, "read"),
            role("dev", "developer", "in_progress", 1, "read, edit"),
            role("lead", "reviewer", "in_review", 2, "read"),
            role("qa", "qa", "in_qa", 2, "read, execute"),
            // A Tech Lead who also builds: hired by the triage tests only.
            role(
                "techlead",
                "reviewer, tech-lead",
                "in_review, in_progress",
                1,
                "read, edit",
            ),
        ];
        let engine = Engine::new("Acme", RoleCatalog::new(roles), Workflow::default()).with_team(
            chillax_core::config::TeamConfig {
                hiring_roles: vec!["pm".into()],
                ..Default::default()
            },
        );
        let hub = Arc::new(
            Hub::open(engine, Arc::new(MemoryLog::default()))
                .await
                .expect("hub"),
        );
        let runtime = AgentRuntime::new(
            Arc::clone(&hub),
            RuntimeConfig {
                company: "Acme".into(),
                cwd: repo.path().to_path_buf(),
                backends: shared_backends(BTreeMap::from([(
                    "fake".to_string(),
                    AgentConfig {
                        command: fake_agent().to_string_lossy().to_string(),
                        ..AgentConfig::default()
                    },
                )])),
                mcp: Some(McpEndpoint {
                    url: "http://127.0.0.1:1/mcp".into(),
                    issue_token: Arc::new(|employee| format!("token-{employee}")),
                }),
                max_concurrent: 8,
                permissions: PermissionMode::Policy,
                ..RuntimeConfig::default()
            },
        );
        runtime.start();
        let mut config = DispatchConfig::new(repo.path(), "dev");
        config.test = test.map(String::from);
        config.max_concurrent = 8;
        config.worktrees = mode;
        config.poll = Duration::from_millis(50);
        config.cooldown = Duration::from_millis(200);
        let with_qa = qa.is_some();
        if let Some((qa, prod)) = qa {
            config.qa = qa;
            config.prod = prod;
        }
        config.apps = apps;
        config.apps_config = AppsConfig {
            grace: Duration::from_millis(300),
            backoff_base: Duration::from_millis(50),
            backoff_cap: Duration::from_millis(200),
            max_attempts: 2,
            ..AppsConfig::default()
        };
        let dispatcher = Dispatcher::new(Arc::clone(&hub), Arc::clone(&runtime), vcs, config);
        dispatcher.start();

        let env = Self {
            hub,
            runtime,
            repo,
            github: fakes,
            dispatcher,
        };
        env.hire("pm", "Pam").await;
        env.hire("lead", "Lee").await;
        for name in developers {
            env.hire("dev", name).await;
        }
        if with_qa {
            env.hire("qa", "Quinn").await;
        }
        env
    }

    async fn hire(&self, role: &str, name: &str) {
        self.send(
            Actor::Stakeholder,
            Command::HireEmployee {
                role: role.into(),
                name: Some(name.into()),
                backend: None,
                model: None,
                effort: None,
                keep_defaults: false,
            },
        )
        .await;
    }

    async fn send(&self, actor: Actor, cmd: Command) {
        self.hub.send(actor, cmd).await.expect("command accepted");
    }

    /// Plan one sprint of tickets `(title, description, assignee)` and approve it.
    async fn sprint(&self, tickets: &[(&str, &str, &str)]) {
        self.sprint_blocked(tickets, &[]).await;
    }

    /// Like [`Env::sprint`], with `(ticket, blocked_by)` pairs set before approval.
    async fn sprint_blocked(&self, tickets: &[(&str, &str, &str)], blockers: &[(&str, &str)]) {
        let pam = || Actor::Employee("pam".into());
        let mut ids = Vec::new();
        for (n, (title, description, _)) in tickets.iter().enumerate() {
            self.send(
                pam(),
                Command::CreateTicket {
                    kind: TicketKind::Story,
                    title: (*title).into(),
                    description: (*description).into(),
                    acceptance_criteria: vec!["works".into()],
                    estimate: None,
                    epic: None,
                    external: None,
                },
            )
            .await;
            ids.push(format!("T-{}", n + 1));
        }
        for (id, blocker) in blockers {
            self.send(
                pam(),
                Command::SetBlockers {
                    id: (*id).into(),
                    blocked_by: vec![(*blocker).into()],
                },
            )
            .await;
        }
        self.send(
            pam(),
            Command::ProposeSprint {
                name: "S1".into(),
                goal: String::new(),
                tickets: ids.clone(),
            },
        )
        .await;
        self.send(
            Actor::Stakeholder,
            Command::ApproveSprint { id: "SP-1".into() },
        )
        .await;
        for (id, (_, _, assignee)) in ids.iter().zip(tickets) {
            if assignee.is_empty() {
                continue;
            }
            self.send(
                pam(),
                Command::AssignTicket {
                    id: id.clone(),
                    assignee: Some((*assignee).into()),
                },
            )
            .await;
        }
    }

    /// Dev builds and the reviewer merges `id`, which `dana` has just written a file for.
    async fn merge_ticket(&self, id: &str, developer: &str) {
        self.state_is(id, "in_progress").await;
        self.until(&format!("{developer} has a session"), |_| {
            self.runtime
                .ticket_sessions()
                .contains(&(developer.to_string(), id.to_string()))
        })
        .await;
        // The scripted agent writes the file the description names; wait for it.
        self.until("the work is on the branch", |s| {
            s.ticket(id).is_some_and(|t| t.worktree.is_some())
        })
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        self.request_review(id, developer).await;
        self.state_is(id, "in_review").await;
        self.review(id, ReviewVerdict::Approve, "ok").await;
        self.state_is(id, "in_qa").await;
    }

    /// Quinn passes `id`, proving its single criterion.
    async fn pass(&self, id: &str) {
        self.send(
            Actor::Employee("quinn".into()),
            Command::SubmitQaVerdict {
                id: id.into(),
                verdict: QaVerdict::Pass,
                criteria: vec![CriterionResult {
                    criterion: "works".into(),
                    passed: true,
                    evidence: "ran it".into(),
                }],
                notes: "all good".into(),
                bug: None,
            },
        )
        .await;
    }

    /// Wait until Quinn is testing `id`.
    async fn quinn_tests(&self, id: &str) {
        self.until(&format!("quinn is testing {id}"), |s| {
            s.open_round()
                .is_some_and(|r| r.status == QaRoundStatus::Testing)
                && self
                    .runtime
                    .ticket_sessions()
                    .contains(&("quinn".to_string(), id.to_string()))
        })
        .await;
    }

    fn worktree(&self, id: &str) -> PathBuf {
        self.repo.path().join(".chillax/worktrees").join(id)
    }

    async fn request_review(&self, id: &str, developer: &str) {
        self.send(
            Actor::Employee(developer.into()),
            Command::RequestReview {
                id: id.into(),
                summary: "done".into(),
            },
        )
        .await;
    }

    async fn review(&self, id: &str, verdict: ReviewVerdict, body: &str) {
        let reviewer = self
            .state()
            .await
            .ticket(id)
            .expect("ticket")
            .reviewer
            .clone();
        self.send(
            Actor::Employee(reviewer.expect("a reviewer")),
            Command::SubmitReview {
                id: id.into(),
                verdict,
                body: body.into(),
            },
        )
        .await;
    }

    async fn state(&self) -> State {
        self.hub.snapshot().await
    }

    async fn state_is(&self, id: &str, state: &str) {
        self.until(&format!("{id} reaches {state}"), |s| {
            s.ticket(id).is_some_and(|t| t.state == state)
        })
        .await;
    }

    async fn until(&self, what: &str, mut cond: impl FnMut(&State) -> bool) {
        let waited = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if cond(&self.state().await) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await;
        if waited.is_err() {
            let s = self.state().await;
            let tickets: Vec<_> = s
                .tickets
                .iter()
                .map(|t| {
                    (
                        &t.id,
                        &t.state,
                        &t.checks,
                        t.comments.last().map(|c| &c.body),
                    )
                })
                .collect();
            let chats: BTreeMap<_, _> = s
                .chats
                .iter()
                .map(|(who, lines)| {
                    let tail: Vec<_> = lines.iter().rev().take(4).map(|l| l.text.clone()).collect();
                    (who.clone(), tail)
                })
                .collect();
            let status: Vec<_> = s
                .employees
                .iter()
                .map(|e| (&e.employee.id, e.status))
                .collect();
            panic!(
                "timed out waiting until {what}: {tickets:?}\n{status:?}\n{chats:#?}\n{:?}",
                self.runtime.ticket_sessions()
            );
        }
    }

    async fn file_appears(&self, path: PathBuf) {
        let waited = tokio::time::timeout(Duration::from_secs(30), async {
            while !path.exists() {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await;
        assert!(waited.is_ok(), "{} never appeared", path.display());
    }
}

fn comments(s: &State, id: &str) -> Vec<String> {
    s.ticket(id)
        .expect("ticket")
        .comments
        .iter()
        .map(|c| c.body.clone())
        .collect()
}

#[tokio::test]
async fn an_approved_sprint_is_built_reviewed_and_merged_into_dev() {
    let env = Env::new(Some("test -f hello.txt"), &["Dana"]).await;
    env.sprint(&[("Say hello", "write file hello.txt", "dana")])
        .await;

    // Chillax starts the work: a worktree on its own branch, and a developer in it.
    env.state_is("T-1", "in_progress").await;
    let state = env.state().await;
    let ticket = state.ticket("T-1").expect("ticket");
    assert_eq!(ticket.branch.as_deref(), Some("feat/T-1-say-hello"));
    env.file_appears(env.worktree("T-1").join("hello.txt"))
        .await;
    assert_eq!(
        env.runtime.ticket_sessions(),
        [("dana".to_string(), "T-1".to_string())].into()
    );

    // The developer asks for a review: tests run, a reviewer is found and briefed.
    env.request_review("T-1", "dana").await;
    env.state_is("T-1", "in_review").await;
    env.until("the reviewer has a session", |_| {
        env.runtime
            .ticket_sessions()
            .contains(&("lee".to_string(), "T-1".to_string()))
    })
    .await;
    let state = env.state().await;
    let ticket = state.ticket("T-1").expect("ticket");
    assert_eq!(ticket.reviewer.as_deref(), Some("lee"));
    assert!(ticket.checks.contains("tests_pass"));

    // Approval merges the branch into dev and tidies up.
    env.review("T-1", ReviewVerdict::Approve, "lgtm").await;
    env.state_is("T-1", "in_qa").await;
    assert_eq!(
        git(env.repo.path(), &["show", "dev:hello.txt"]),
        "hello.txt"
    );
    assert_eq!(
        git(env.repo.path(), &["branch", "--show-current"]),
        "main",
        "the stakeholder's checkout is untouched"
    );
    assert!(!env.repo.path().join("hello.txt").exists());
    env.until("sessions and worktree are gone", |_| {
        env.runtime.ticket_sessions().is_empty() && !env.worktree("T-1").exists()
    })
    .await;
    let state = env.state().await;
    let history = comments(&state, "T-1");
    assert!(history.iter().any(|c| c == "Tests passed."), "{history:?}");
    assert!(
        history
            .iter()
            .any(|c| c.starts_with("Merged ") && c.ends_with("into dev.")),
        "{history:?}"
    );
    assert!(
        state
            .ticket("T-1")
            .expect("ticket")
            .checks
            .contains("merged_to_dev")
    );
}

#[tokio::test]
async fn a_developer_the_pm_hires_mid_run_gets_work_started() {
    let env = Env::new(None, &[]).await;
    let pam = || Actor::Employee("pam".into());
    env.send(
        pam(),
        Command::CreateTicket {
            kind: TicketKind::Story,
            title: "Say hello".into(),
            description: "write file hello.txt".into(),
            acceptance_criteria: vec!["works".into()],
            estimate: None,
            epic: None,
            external: None,
        },
    )
    .await;
    env.send(
        pam(),
        Command::ProposeSprint {
            name: "S1".into(),
            goal: String::new(),
            tickets: vec!["T-1".into()],
        },
    )
    .await;
    env.send(
        Actor::Stakeholder,
        Command::ApproveSprint { id: "SP-1".into() },
    )
    .await;
    env.send(
        pam(),
        Command::HireTeammate {
            role: "dev".into(),
            name: Some("Dee".into()),
            reason: "T-1 needs a developer".into(),
        },
    )
    .await;
    env.send(
        pam(),
        Command::AssignTicket {
            id: "T-1".into(),
            assignee: Some("dee".into()),
        },
    )
    .await;
    env.state_is("T-1", "in_progress").await;
    env.until("the new developer has a session", |_| {
        env.runtime
            .ticket_sessions()
            .contains(&("dee".to_string(), "T-1".to_string()))
    })
    .await;
}

#[tokio::test]
async fn failing_tests_keep_the_ticket_with_its_developer() {
    let env = Env::new(Some("echo nope >&2; exit 1"), &["Dana"]).await;
    env.sprint(&[("Say hello", "write file hello.txt", "dana")])
        .await;
    env.state_is("T-1", "in_progress").await;
    env.file_appears(env.worktree("T-1").join("hello.txt"))
        .await;

    env.request_review("T-1", "dana").await;
    env.until("the failure is on the ticket", |s| {
        comments(s, "T-1")
            .iter()
            .any(|c| c.starts_with("Tests failed."))
    })
    .await;
    let state = env.state().await;
    assert_eq!(state.ticket("T-1").expect("ticket").state, "in_progress");
    // The developer's session is told what failed.
    env.until("the developer is told", |s| {
        s.chats["dana"]
            .iter()
            .any(|l| l.text.contains("Fix it, then call request_review again"))
    })
    .await;
}

#[tokio::test]
async fn requested_changes_go_back_to_the_developer() {
    let env = Env::new(None, &["Dana"]).await;
    env.sprint(&[("Say hello", "write file hello.txt", "dana")])
        .await;
    env.state_is("T-1", "in_progress").await;
    env.file_appears(env.worktree("T-1").join("hello.txt"))
        .await;
    env.request_review("T-1", "dana").await;
    env.state_is("T-1", "in_review").await;

    env.review(
        "T-1",
        ReviewVerdict::RequestChanges,
        "handle the empty case",
    )
    .await;
    env.state_is("T-1", "in_progress").await;
    env.until("the reviewer's session is freed", |_| {
        !env.runtime
            .ticket_sessions()
            .contains(&("lee".to_string(), "T-1".to_string()))
    })
    .await;
    // The developer's old context ended with the hand-over; the rework starts a new
    // session, briefed with the feedback.
    env.until("the developer gets a fresh session", |_| {
        env.runtime
            .ticket_sessions()
            .contains(&("dana".to_string(), "T-1".to_string()))
    })
    .await;
    assert!(
        comments(&env.state().await, "T-1")
            .iter()
            .any(|c| c.contains("handle the empty case"))
    );
    // And the loop goes round again.
    env.request_review("T-1", "dana").await;
    env.state_is("T-1", "in_review").await;
}

#[tokio::test]
async fn open_branches_are_rebased_after_a_merge() {
    let env = Env::new(None, &["Dana", "Dan"]).await;
    env.sprint(&[
        ("First", "write file a.txt", "dana"),
        ("Second", "nothing to write yet", "dan"),
    ])
    .await;
    env.state_is("T-1", "in_progress").await;
    env.state_is("T-2", "in_progress").await;
    env.file_appears(env.worktree("T-1").join("a.txt")).await;
    // Both developers are idle before the merge, so the rebase can run.
    env.until("both sessions idle", |s| {
        ["dana", "dan"].iter().all(|d| {
            s.employee(d)
                .is_some_and(|e| e.status == chillax_core::EmployeeStatus::Idle)
        })
    })
    .await;

    env.request_review("T-1", "dana").await;
    env.state_is("T-1", "in_review").await;
    env.review("T-1", ReviewVerdict::Approve, "ok").await;
    env.state_is("T-1", "in_qa").await;

    env.file_appears(env.worktree("T-2").join("a.txt")).await;
    env.until("T-2 is no longer behind dev", |s| {
        s.ticket("T-2").is_some_and(|t| !t.needs_rebase)
    })
    .await;
}

/// T-1 (Dana) and T-2 (Dan) write `file` with different text, both are approved, and T-1
/// merges first; T-2's approval is then sent and Chillax tries to merge it.
async fn second_merge_after(env: &Env, file: &str, texts: [&str; 2]) {
    env.state_is("T-1", "in_progress").await;
    env.state_is("T-2", "in_progress").await;
    env.until("both developers have sessions", |_| {
        env.runtime.ticket_sessions().len() == 2
    })
    .await;
    for (id, text) in [("T-1", texts[0]), ("T-2", texts[1])] {
        std::fs::write(env.worktree(id).join(file), text).expect("write");
    }
    env.request_review("T-1", "dana").await;
    env.request_review("T-2", "dan").await;
    env.state_is("T-1", "in_review").await;
    env.state_is("T-2", "in_review").await;
    env.review("T-1", ReviewVerdict::Approve, "ok").await;
    env.state_is("T-1", "in_qa").await;
    env.review("T-2", ReviewVerdict::Approve, "ok").await;
}

/// Play the developer resolving a conflict with `dev` in `dir`: merge, keep both sides of
/// `file`, commit.
fn resolve(dir: &Path, file: &str, text: &str) {
    let merged = std::process::Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(["merge", "dev"])
        .current_dir(dir)
        .output()
        .expect("git runs");
    assert!(!merged.status.success(), "the merge conflicts");
    std::fs::write(dir.join(file), text).expect("write");
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "--quiet", "-m", "merge dev"]);
}

impl Env {
    /// Wait until `id`'s developer was handed conflict attempt `attempt`.
    async fn conflict_briefed(&self, id: &str, attempt: u32) {
        self.until(
            &format!("{id} conflict attempt {attempt} is briefed"),
            |s| {
                s.ticket(id)
                    .and_then(|t| t.conflict.as_ref())
                    .is_some_and(|c| c.attempts == attempt && c.briefed)
            },
        )
        .await;
    }

    /// The developer says they are done; the dispatcher may have got there first.
    async fn request_review_quietly(&self, id: &str, developer: &str) {
        let _ = self
            .hub
            .send(
                Actor::Employee(developer.into()),
                Command::RequestReview {
                    id: id.into(),
                    summary: "resolved".into(),
                },
            )
            .await;
    }
}

#[tokio::test]
async fn a_conflicting_merge_is_resolved_by_its_developer_and_merges_without_a_new_review() {
    let env = Env::new(None, &["Dana", "Dan"]).await;
    env.sprint(&[
        ("First", "nothing to write", "dana"),
        ("Second", "nothing to write", "dan"),
    ])
    .await;
    second_merge_after(&env, "same.txt", ["one\n", "two\n"]).await;

    // The approval stands: T-2 stays in review and Dan gets a resolve turn.
    env.conflict_briefed("T-2", 1).await;
    let state = env.state().await;
    let ticket = state.ticket("T-2").expect("ticket");
    assert_eq!(ticket.state, "in_review");
    assert!(ticket.checks.contains("review_approved"));
    assert_eq!(
        ticket.conflict.as_ref().map(|c| c.files.clone()),
        Some(vec!["same.txt".to_string()])
    );
    assert!(
        comments(&state, "T-2")
            .iter()
            .any(|c| c.contains("approval stands")),
    );
    assert!(
        env.runtime
            .ticket_sessions()
            .contains(&("dan".to_string(), "T-2".to_string()))
    );

    resolve(&env.worktree("T-2"), "same.txt", "one\ntwo\n");
    env.request_review_quietly("T-2", "dan").await;
    env.state_is("T-2", "in_qa").await;
    assert_eq!(git(env.repo.path(), &["show", "dev:same.txt"]), "one\ntwo");
    let reviews = comments(&env.state().await, "T-2")
        .iter()
        .filter(|c| c.ends_with("is reviewing."))
        .count();
    assert_eq!(reviews, 1, "no second review");
}

#[tokio::test]
async fn a_resolution_without_request_review_is_found_when_the_turn_ends() {
    let env = Env::new(None, &["Dana", "Dan"]).await;
    env.sprint(&[
        ("First", "nothing to write", "dana"),
        ("Second", "nothing to write", "dan"),
    ])
    .await;
    second_merge_after(&env, "same.txt", ["one\n", "two\n"]).await;
    env.conflict_briefed("T-2", 1).await;
    resolve(&env.worktree("T-2"), "same.txt", "one\ntwo\n");
    env.state_is("T-2", "in_qa").await;
    assert_eq!(git(env.repo.path(), &["show", "dev:same.txt"]), "one\ntwo");
}

#[tokio::test]
async fn tests_that_fail_once_dev_is_merged_in_send_the_ticket_back() {
    // Either change alone passes; together they fail.
    let env = Env::new(Some("test ! -f a.txt || test ! -f b.txt"), &["Dana", "Dan"]).await;
    env.sprint(&[
        ("First", "nothing to write", "dana"),
        ("Second", "nothing to write", "dan"),
    ])
    .await;
    env.state_is("T-1", "in_progress").await;
    env.state_is("T-2", "in_progress").await;
    env.until("both developers have sessions", |_| {
        env.runtime.ticket_sessions().len() == 2
    })
    .await;
    std::fs::write(env.worktree("T-1").join("a.txt"), "a\n").expect("write");
    std::fs::write(env.worktree("T-2").join("b.txt"), "b\n").expect("write");
    env.request_review("T-1", "dana").await;
    env.request_review("T-2", "dan").await;
    env.state_is("T-1", "in_review").await;
    env.state_is("T-2", "in_review").await;
    env.review("T-1", ReviewVerdict::Approve, "ok").await;
    env.state_is("T-1", "in_qa").await;
    env.review("T-2", ReviewVerdict::Approve, "ok").await;

    env.until("T-2 comes back with the failing tests", |s| {
        s.ticket("T-2").is_some_and(|t| t.state == "in_progress")
            && comments(s, "T-2")
                .iter()
                .any(|c| c.starts_with("Tests failed after merging dev"))
    })
    .await;
    let state = env.state().await;
    assert!(state.ticket("T-2").expect("ticket").checks.is_empty());
    let dev_files = git(env.repo.path(), &["ls-tree", "--name-only", "dev"]);
    assert!(!dev_files.contains("b.txt"), "{dev_files}");
}

#[tokio::test]
async fn a_merge_conflict_is_triaged_after_two_unresolved_attempts() {
    let env = Env::new(None, &["Dana", "Dan"]).await;
    env.hire("techlead", "Tess").await;
    env.sprint(&[
        ("First", "nothing to write", "dana"),
        ("Second", "nothing to write", "dan"),
    ])
    .await;
    second_merge_after(&env, "same.txt", ["one\n", "two\n"]).await;
    // Dan says he is done twice without merging dev.
    for attempt in 1..=2 {
        env.conflict_briefed("T-2", attempt).await;
        env.request_review_quietly("T-2", "dan").await;
    }

    let failure = env.failure().await;
    assert_eq!(failure.class, chillax_core::FailureClass::Merge);
    assert_eq!(failure.ticket.as_deref(), Some("T-2"));
    env.pam_is_briefed(&failure.fingerprint).await;
    env.until("the developer is told too", |s| {
        s.ticket("T-2")
            .is_some_and(|t| t.state == "in_progress" && t.conflict.is_none())
            && comments(s, "T-2")
                .iter()
                .any(|c| c.starts_with("Could not merge"))
    })
    .await;
    let bug = env.pam_files_bug(&failure.fingerprint).await;
    env.state_is(&bug, "in_progress").await;
}

#[tokio::test]
async fn blocked_tickets_wait_for_their_blockers_to_merge() {
    let env = Env::new(None, &["Dana", "Dan"]).await;
    env.sprint_blocked(
        &[
            ("First", "write file a.txt", "dana"),
            ("Second", "write file b.txt", "dan"),
        ],
        &[("T-2", "T-1")],
    )
    .await;
    env.state_is("T-1", "in_progress").await;
    env.file_appears(env.worktree("T-1").join("a.txt")).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        env.state().await.ticket("T-2").expect("ticket").state,
        "ready"
    );

    env.request_review("T-1", "dana").await;
    env.state_is("T-1", "in_review").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        env.state().await.ticket("T-2").expect("ticket").state,
        "ready",
        "in review is not merged"
    );

    env.review("T-1", ReviewVerdict::Approve, "ok").await;
    env.state_is("T-1", "in_qa").await;
    env.state_is("T-2", "in_progress").await;
}

#[tokio::test]
async fn pausing_stops_the_agents_and_resuming_brings_them_back() {
    let env = Env::new(None, &["Dana"]).await;
    env.sprint(&[("Say hello", "write file hello.txt", "dana")])
        .await;
    env.state_is("T-1", "in_progress").await;
    env.until("the developer has a session", |_| {
        !env.runtime.ticket_sessions().is_empty()
    })
    .await;

    env.send(Actor::Stakeholder, Command::PauseDispatch).await;
    env.until("sessions are closed", |_| {
        env.runtime.ticket_sessions().is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        env.runtime.ticket_sessions().is_empty(),
        "nothing restarts while paused"
    );

    env.send(Actor::Stakeholder, Command::ResumeDispatch).await;
    env.until("the developer is back", |_| {
        env.runtime
            .ticket_sessions()
            .contains(&("dana".to_string(), "T-1".to_string()))
    })
    .await;
}

#[tokio::test]
async fn the_planner_is_asked_to_assign_loose_tickets() {
    let env = Env::new(None, &["Dana"]).await;
    env.send(
        Actor::Employee("pam".into()),
        Command::CreateTicket {
            kind: TicketKind::Story,
            title: "Loose".into(),
            description: String::new(),
            acceptance_criteria: vec![],
            estimate: None,
            epic: None,
            external: None,
        },
    )
    .await;
    env.send(
        Actor::Employee("pam".into()),
        Command::ProposeSprint {
            name: "S1".into(),
            goal: String::new(),
            tickets: vec!["T-1".into()],
        },
    )
    .await;
    env.send(
        Actor::Stakeholder,
        Command::ApproveSprint { id: "SP-1".into() },
    )
    .await;
    env.until("Pam is nudged", |s| {
        s.chats["pam"].iter().any(|l| {
            l.text.contains("[Chillax] Sprint SP-1 was approved") && l.text.contains("T-1")
        })
    })
    .await;
}

fn marker(dir: &Path, name: &str) -> PathBuf {
    dir.join(name)
}

#[tokio::test]
async fn a_merged_story_is_tested_released_tagged_and_deployed() {
    let marks = tempfile::tempdir().expect("tempdir");
    let m = marks.path().display();
    let qa = QaEnv {
        up: Some(format!("touch {m}/up")),
        test: Some("test -f hello.txt".into()),
        down: Some(format!("touch {m}/down")),
        ..QaEnv::default()
    };
    let prod = ProdEnv {
        deploy: Some(format!("touch {m}/deploy")),
        ..ProdEnv::default()
    };
    let env = Env::with_qa(None, &["Dana"], qa, prod).await;
    env.sprint(&[("Say hello", "write file hello.txt", "dana")])
        .await;
    env.merge_ticket("T-1", "dana").await;

    // The sprint is complete, so Chillax promotes dev to qa on its own, brings the
    // environment up, checks it, and starts the tester.
    env.quinn_tests("T-1").await;
    assert!(marker(marks.path(), "up").exists());
    assert_eq!(
        git(env.repo.path(), &["rev-parse", "qa"]),
        git(env.repo.path(), &["rev-parse", "dev"]),
        "qa is dev"
    );

    // A pass signs the ticket off; with every ticket done, the round closes (and the
    // environment goes down) and a release waits at the gate.
    env.pass("T-1").await;
    env.state_is("T-1", "done").await;
    env.until("the release waits at the gate", |s| {
        s.releases
            .last()
            .is_some_and(|r| r.status == ReleaseStatus::Gated)
    })
    .await;
    let state = env.state().await;
    assert_eq!(
        state.qa.as_ref().map(|r| r.status),
        Some(QaRoundStatus::Closed)
    );
    assert!(marker(marks.path(), "down").exists(), "down ran");
    assert!(
        state
            .inbox
            .iter()
            .any(|i| matches!(i, InboxItem::ReleaseGate { release, .. } if release == "R-1"))
    );
    env.until("the tester's session is closed", |_| {
        env.runtime.ticket_sessions().is_empty()
    })
    .await;
    let release = state.release("R-1").expect("release").clone();
    assert_eq!(release.version, "v0.1.0");
    assert!(release.notes.contains("Say hello"), "{}", release.notes);

    // The stakeholder approves: qa is merged into main, tagged, merged back into dev,
    // and deployed.
    env.send(
        Actor::Stakeholder,
        Command::ApproveRelease {
            id: "R-1".into(),
            version: "v0.1.0".into(),
            notes: release.notes.clone(),
        },
    )
    .await;
    env.until("the release is deployed", |s| {
        s.release("R-1")
            .is_some_and(|r| r.status == ReleaseStatus::Deployed)
    })
    .await;
    let repo = env.repo.path();
    assert_eq!(git(repo, &["cat-file", "-t", "v0.1.0"]), "tag");
    assert!(git(repo, &["tag", "-n9", "-l", "v0.1.0"]).contains("Say hello"));
    assert_eq!(git(repo, &["show", "main:hello.txt"]), "hello.txt");
    let changelog = git(repo, &["show", "main:CHANGELOG.md"]);
    assert!(changelog.starts_with("# Changelog"), "{changelog}");
    assert!(changelog.contains("## v0.1.0 — "), "{changelog}");
    assert!(
        git(repo, &["show", "dev:CHANGELOG.md"]).contains("## v0.1.0"),
        "main went back into dev"
    );
    assert_eq!(
        git(repo, &["rev-list", "-n1", "v0.1.0"]),
        git(repo, &["rev-parse", "main"]),
        "the tag is the release merge"
    );
    assert!(marker(marks.path(), "deploy").exists(), "deploy ran");
    let state = env.state().await;
    assert_eq!(
        state.sprint("SP-1").expect("sprint").status,
        SprintStatus::Released
    );
    assert!(
        state
            .ticket("T-1")
            .expect("ticket")
            .checks
            .contains("released")
    );
    // The stakeholder's own checkout of main took the release in place, cleanly.
    assert_eq!(git(repo, &["status", "--porcelain"]), "");
    assert!(repo.join("CHANGELOG.md").exists());

    // After the deploy, qa followed main and nothing was left to report (ADR 0036).
    let mut converged = false;
    for _ in 0..100 {
        if git(repo, &["rev-parse", "qa"]) == git(repo, &["rev-parse", "main"]) {
            converged = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(converged, "qa should follow main after the deploy");
    assert!(
        !env.state().await.inbox.iter().any(|i| matches!(
            i,
            InboxItem::Alert { title, .. } if title.contains("not fully synced")
        )),
        "a clean convergence raises no alert"
    );
    assert_eq!(git(repo, &["branch", "--show-current"]), "main");
}

#[tokio::test]
async fn qa_tests_a_merged_story_while_the_next_one_is_still_in_development() {
    let env = Env::with_qa(None, &["Dana"], QaEnv::default(), ProdEnv::default()).await;
    env.sprint(&[
        ("Say hello", "write file hello.txt", "dana"),
        ("Say bye", "write file bye.txt", "dana"),
    ])
    .await;
    env.merge_ticket("T-1", "dana").await;

    // Dana moves on to T-2 and has not asked for a review; QA already has T-1.
    env.quinn_tests("T-1").await;
    let state = env.state().await;
    let t2 = state.ticket("T-2").expect("second story");
    assert!(
        matches!(t2.state.as_str(), "ready" | "in_progress"),
        "T-2 is {}",
        t2.state
    );
    env.pass("T-1").await;
    env.state_is("T-1", "done").await;
    env.until("round 1 closes", |s| {
        s.qa.as_ref()
            .is_some_and(|r| r.number == 1 && r.status == QaRoundStatus::Closed)
    })
    .await;
    // The sprint is not finished, so there is no release yet.
    assert!(env.state().await.releases.is_empty());

    // T-2 arrives later and gets its own round; finishing the sprint opens the gate.
    env.merge_ticket("T-2", "dana").await;
    env.until("round 2 tests T-2", |s| {
        s.open_round()
            .is_some_and(|r| r.number == 2 && r.status == QaRoundStatus::Testing)
    })
    .await;
    env.pass("T-2").await;
    env.until("the release waits at the gate", |s| {
        s.releases
            .last()
            .is_some_and(|r| r.status == ReleaseStatus::Gated)
    })
    .await;
    let release = env.state().await.release("R-1").expect("release").clone();
    assert_eq!(release.tickets, ["T-1", "T-2"]);
}

#[tokio::test]
async fn a_failed_story_gets_a_bug_that_is_fixed_before_the_retest_and_release() {
    let env = Env::with_qa(None, &["Dana"], QaEnv::default(), ProdEnv::default()).await;
    env.sprint(&[("Say hello", "write file hello.txt", "dana")])
        .await;
    env.merge_ticket("T-1", "dana").await;
    env.quinn_tests("T-1").await;

    // QA fails the story and files a bug whose description tells the scripted developer
    // what to write.
    env.send(
        Actor::Employee("quinn".into()),
        Command::SubmitQaVerdict {
            id: "T-1".into(),
            verdict: QaVerdict::Fail,
            criteria: vec![CriterionResult {
                criterion: "works".into(),
                passed: false,
                evidence: "the greeting is empty".into(),
            }],
            notes: "greeting missing".into(),
            bug: Some(NewBug {
                title: "Greeting is empty".into(),
                description: "write file fix.txt".into(),
            }),
        },
    )
    .await;

    // The round closes with no release: a failure holds it back.
    env.until("round 1 closes", |s| {
        s.qa.as_ref()
            .is_some_and(|r| r.number == 1 && r.status == QaRoundStatus::Closed)
    })
    .await;
    assert!(env.state().await.releases.is_empty());
    let state = env.state().await;
    let story = state.ticket("T-1").expect("story");
    assert_eq!(story.state, "in_qa");
    assert!(story.checks.contains("qa_failed"));

    // The bug goes to the story's developer through the normal loop.
    let bug = state.ticket("T-2").expect("bug");
    assert_eq!(bug.kind, TicketKind::Bug);
    assert_eq!(bug.found_in.as_deref(), Some("T-1"));
    assert_eq!(bug.assignee.as_deref(), Some("dana"));
    env.merge_ticket("T-2", "dana").await;
    assert_eq!(git(env.repo.path(), &["show", "dev:fix.txt"]), "fix.txt");

    // With the bug merged the sprint is complete again: round 2 retests both.
    env.until("round 2 is testing both", |s| {
        s.qa.as_ref().is_some_and(|r| {
            r.number == 2 && r.status == QaRoundStatus::Testing && r.tickets.len() == 2
        })
    })
    .await;
    assert!(
        !env.state()
            .await
            .ticket("T-1")
            .expect("story")
            .checks
            .contains("qa_failed"),
        "the retest starts clean"
    );
    env.until("quinn has both tickets", |_| {
        env.runtime.ticket_sessions().len() == 2
    })
    .await;
    env.pass("T-1").await;
    env.pass("T-2").await;
    env.until("the release waits at the gate", |s| {
        s.releases
            .last()
            .is_some_and(|r| r.status == ReleaseStatus::Gated)
    })
    .await;
    let release = env.state().await.release("R-1").expect("release").clone();
    assert_eq!(release.tickets, ["T-1", "T-2"]);
    assert!(release.notes.contains("### Fixes"), "{}", release.notes);
    assert!(
        release.notes.contains("(found in T-1)"),
        "{}",
        release.notes
    );
}

#[tokio::test]
async fn a_qa_environment_that_fails_to_come_up_waits_for_a_retry() {
    let marks = tempfile::tempdir().expect("tempdir");
    let ready = marks.path().join("ready");
    let qa = QaEnv {
        up: Some(format!("test -f {}", ready.display())),
        ..QaEnv::default()
    };
    let env = Env::with_qa(None, &["Dana"], qa, ProdEnv::default()).await;
    env.sprint(&[("Say hello", "write file hello.txt", "dana")])
        .await;
    env.merge_ticket("T-1", "dana").await;

    // `up` fails: the stakeholder is told, no bug is filed and no tester starts.
    env.until("the round is stuck on the environment", |s| {
        s.qa.as_ref()
            .is_some_and(|r| r.status == QaRoundStatus::EnvFailed)
    })
    .await;
    let state = env.state().await;
    assert!(state.inbox.iter().any(|i| matches!(
        i,
        InboxItem::EnvAlert { env, step: chillax_core::EnvStep::Up, .. } if env == "qa"
    )));
    assert_eq!(state.tickets.len(), 1, "no bug for an environment failure");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        env.runtime.ticket_sessions().is_empty(),
        "nothing retries on its own"
    );

    // Fixed by hand, then retried.
    std::fs::write(&ready, "").expect("write");
    env.send(
        Actor::Stakeholder,
        Command::RetryEnv {
            env: "qa".into(),
            step: chillax_core::EnvStep::Up,
        },
    )
    .await;
    env.quinn_tests("T-1").await;
    assert!(
        !env.state()
            .await
            .inbox
            .iter()
            .any(|i| matches!(i, InboxItem::EnvAlert { .. })),
        "the retried alert is gone"
    );
}

#[tokio::test]
async fn a_dirty_main_checkout_does_not_block_the_release() {
    let env = Env::with_qa(None, &["Dana"], QaEnv::default(), ProdEnv::default()).await;
    env.sprint(&[("Say hello", "write file hello.txt", "dana")])
        .await;
    env.merge_ticket("T-1", "dana").await;
    env.quinn_tests("T-1").await;
    env.pass("T-1").await;
    env.until("the release waits at the gate", |s| {
        s.releases
            .last()
            .is_some_and(|r| r.status == ReleaseStatus::Gated)
    })
    .await;

    // main is the stakeholder's own checkout, and they have uncommitted work in it.
    let scratch = env.repo.path().join("scratch.txt");
    std::fs::write(&scratch, "work in progress\n").expect("write");
    let notes = env
        .state()
        .await
        .release("R-1")
        .expect("release")
        .notes
        .clone();
    env.send(
        Actor::Stakeholder,
        Command::ApproveRelease {
            id: "R-1".into(),
            version: "v0.1.0".into(),
            notes,
        },
    )
    .await;
    // Their unrelated work does not stop the release, and is left as it was.
    env.until("the release is deployed", |s| {
        s.release("R-1")
            .is_some_and(|r| r.status == ReleaseStatus::Deployed)
    })
    .await;
    assert_eq!(
        std::fs::read_to_string(&scratch).expect("their work"),
        "work in progress\n"
    );
    assert_eq!(git(env.repo.path(), &["cat-file", "-t", "v0.1.0"]), "tag");
}

#[tokio::test]
async fn on_github_work_goes_through_pull_requests_and_a_dirty_checkout_never_blocks_the_release() {
    let env = Env::on_github(None, &["Dana"]).await;
    env.sprint(&[("Say hello", "write file hello.txt", "dana")])
        .await;
    // The scripted agent must have written its file, or the branch has nothing to propose.
    env.state_is("T-1", "in_progress").await;
    env.file_appears(env.worktree("T-1").join("hello.txt"))
        .await;
    env.merge_ticket("T-1", "dana").await;
    let (remote, bin) = env.github.as_ref().expect("github mode");
    let (remote, bin) = (remote.path(), bin.path());
    let gh_log = || std::fs::read_to_string(bin.join("gh.log")).unwrap_or_default();

    // The branch was pushed, a pull request opened into dev, the verdict commented on
    // it and the pull request merged on the remote.
    let state = env.state().await;
    let ticket = state.ticket("T-1").expect("ticket");
    assert_eq!(ticket.pr_number, Some(1));
    assert_eq!(
        ticket.pr_url.as_deref(),
        Some("https://github.com/o/r/pull/1")
    );
    assert!(gh_log().contains("pr create --base dev --head feat/T-1-say-hello"));
    assert!(gh_log().contains("pr merge 1 --merge"));
    let comments_log = std::fs::read_to_string(bin.join("comments.log")).expect("comments");
    assert!(comments_log.contains("approved"), "{comments_log}");
    assert_eq!(git(remote, &["show", "dev:hello.txt"]), "hello.txt");
    // The local dev followed the remote one.
    env.until("local dev caught up", |_| {
        git(env.repo.path(), &["rev-parse", "dev"]) == git(remote, &["rev-parse", "dev"])
    })
    .await;

    // Promotion pushes qa.
    env.quinn_tests("T-1").await;
    assert_eq!(
        git(remote, &["rev-parse", "qa"]),
        git(remote, &["rev-parse", "dev"])
    );
    env.pass("T-1").await;
    env.until("the release waits at the gate", |s| {
        s.releases
            .last()
            .is_some_and(|r| r.status == ReleaseStatus::Gated)
    })
    .await;

    // The stakeholder has uncommitted work on main in the repository itself.
    let scratch = env.repo.path().join("scratch.txt");
    std::fs::write(&scratch, "work in progress\n").expect("write");
    let release = env.state().await.release("R-1").expect("release").clone();
    env.send(
        Actor::Stakeholder,
        Command::ApproveRelease {
            id: "R-1".into(),
            version: "v0.1.0".into(),
            notes: release.notes.clone(),
        },
    )
    .await;
    env.until("the release is deployed", |s| {
        s.release("R-1")
            .is_some_and(|r| r.status == ReleaseStatus::Deployed)
    })
    .await;
    assert!(scratch.exists(), "their work was not touched");
    assert!(gh_log().contains("pr create --base main --head qa"));
    assert!(gh_log().contains("release create v0.1.0"));
    assert!(git(remote, &["tag", "-l"]).contains("v0.1.0"));
    assert_eq!(
        git(remote, &["rev-list", "-n1", "v0.1.0"]),
        git(remote, &["rev-parse", "main"]),
        "the tag is the merged release"
    );
    assert!(git(remote, &["show", "main:CHANGELOG.md"]).contains("## v0.1.0"));
    assert!(
        git(remote, &["show", "dev:CHANGELOG.md"]).contains("## v0.1.0"),
        "main went back into dev and dev was pushed"
    );
    // The local main stayed behind, and the stakeholder is told so.
    let state = env.state().await;
    assert!(
        state
            .inbox
            .iter()
            .any(|i| matches!(i, InboxItem::Alert { title, detail, .. }
        if title == "Local main is behind the remote" && detail.contains("origin/main")))
    );
}

// ---- failure triage (plan.md section 7.4, ADR 0010) ----

impl Env {
    /// The failure the dispatcher has recorded, once there is one.
    async fn failure(&self) -> chillax_core::Failure {
        self.until("a failure is recorded", |s| !s.failures.is_empty())
            .await;
        self.state().await.failures[0].clone()
    }

    /// Wait until Pam has been briefed about the failure.
    async fn pam_is_briefed(&self, fingerprint: &str) {
        self.until("Pam is briefed", |s| {
            s.chats["pam"]
                .iter()
                .any(|l| l.text.contains("Failure triage") && l.text.contains(fingerprint))
        })
        .await;
    }

    /// Play Pam's `create_story` call: a bug for the Tech Lead, linked to the failure.
    async fn pam_files_bug(&self, fingerprint: &str) -> String {
        let pam = || Actor::Employee("pam".into());
        self.send(
            pam(),
            Command::CreateTicket {
                kind: TicketKind::Bug,
                title: "Fix the failure".into(),
                description: "nothing to write".into(),
                acceptance_criteria: vec!["works".into()],
                estimate: None,
                epic: None,
                external: None,
            },
        )
        .await;
        let bug = self
            .state()
            .await
            .tickets
            .last()
            .expect("ticket")
            .id
            .clone();
        self.send(
            pam(),
            Command::AssignTicket {
                id: bug.clone(),
                assignee: Some("tess".into()),
            },
        )
        .await;
        self.send(
            pam(),
            Command::LinkTriageBug {
                fingerprint: fingerprint.into(),
                bug: bug.clone(),
            },
        )
        .await;
        bug
    }

    fn briefings(state: &State) -> usize {
        state.chats["pam"]
            .iter()
            .filter(|l| l.text.contains("Failure triage"))
            .count()
    }
}

#[tokio::test]
async fn tests_failing_twice_become_a_bug_the_tech_lead_builds() {
    let env = Env::new(Some("echo nope >&2; exit 1"), &["Dana"]).await;
    env.hire("techlead", "Tess").await;
    env.sprint(&[("Say hello", "write file hello.txt", "dana")])
        .await;
    env.file_appears(env.worktree("T-1").join("hello.txt"))
        .await;

    // The first failure stays with the developer.
    env.request_review("T-1", "dana").await;
    env.until("the first failure is on the ticket", |s| {
        comments(s, "T-1")
            .iter()
            .any(|c| c.starts_with("Tests failed."))
    })
    .await;
    assert!(env.state().await.failures.is_empty());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(Env::briefings(&env.state().await), 0);

    // The second is triaged: Pam is briefed with the output, the context and the owner.
    env.until("the developer is told", |s| {
        s.chats["dana"]
            .iter()
            .any(|l| l.text.contains("Fix it, then call request_review"))
    })
    .await;
    env.request_review("T-1", "dana").await;
    let failure = env.failure().await;
    env.pam_is_briefed(&failure.fingerprint).await;
    let state = env.state().await;
    let brief = state.chats["pam"]
        .iter()
        .find(|l| l.text.contains("Failure triage"))
        .expect("briefing");
    assert!(brief.text.contains("nope"), "{}", brief.text);
    assert!(
        brief.text.contains("T-1") && brief.text.contains("`tess`"),
        "{}",
        brief.text
    );

    // Pam files the bug; it joins the sprint and Tess builds it in a worktree.
    let bug = env.pam_files_bug(&failure.fingerprint).await;
    env.state_is(&bug, "in_progress").await;
    env.until("Tess has a session on the bug", |_| {
        env.runtime
            .ticket_sessions()
            .contains(&("tess".to_string(), bug.clone()))
    })
    .await;
    let state = env.state().await;
    assert_eq!(
        state.ticket(&bug).expect("bug").sprint.as_deref(),
        Some("SP-1")
    );
    assert!(
        state.inbox.iter().any(|i| matches!(
            i,
            InboxItem::Alert { title, .. } if title.contains(&format!("Bug {bug} filed"))
        )),
        "the stakeholder gets a note"
    );

    // The same failure again does not brief Pam or file a second bug.
    env.request_review("T-1", "dana").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let state = env.state().await;
    assert_eq!(Env::briefings(&state), 1);
    assert_eq!(state.tickets.len(), 2);
    assert!(state.failures[0].occurrences >= 2);
}

#[tokio::test]
async fn a_failed_qa_environment_is_briefed_filed_and_still_waits_for_a_retry() {
    let marks = tempfile::tempdir().expect("tempdir");
    let ready = marks.path().join("ready");
    let qa = QaEnv {
        up: Some(format!("echo no-db >&2; test -f {}", ready.display())),
        ..QaEnv::default()
    };
    let env = Env::with_qa(None, &["Dana"], qa, ProdEnv::default()).await;
    env.hire("techlead", "Tess").await;
    env.sprint(&[("Say hello", "write file hello.txt", "dana")])
        .await;
    env.merge_ticket("T-1", "dana").await;

    let failure = env.failure().await;
    assert_eq!(failure.subject, "qa up");
    env.pam_is_briefed(&failure.fingerprint).await;
    let bug = env.pam_files_bug(&failure.fingerprint).await;
    env.state_is(&bug, "in_progress").await;
    let state = env.state().await;
    assert!(
        state
            .inbox
            .iter()
            .any(|i| matches!(i, InboxItem::EnvAlert { .. })),
        "the original alert stays so the stakeholder can still retry"
    );
    assert!(state.inbox.iter().any(|i| matches!(
        i,
        InboxItem::Alert { title, .. } if title.contains(&format!("Bug {bug} filed"))
    )));

    std::fs::write(&ready, "").expect("write");
    env.send(
        Actor::Stakeholder,
        Command::RetryEnv {
            env: "qa".into(),
            step: chillax_core::EnvStep::Up,
        },
    )
    .await;
    env.quinn_tests("T-1").await;
}

#[tokio::test]
async fn dropping_a_ticket_in_progress_ends_its_session_and_frees_the_worktree() {
    let env = Env::new(None, &["Dana"]).await;
    env.sprint(&[("Say hello", "write file hello.txt", "dana")])
        .await;
    env.state_is("T-1", "in_progress").await;
    env.file_appears(env.worktree("T-1").join("hello.txt"))
        .await;
    env.until("the developer has a session", |_| {
        !env.runtime.ticket_sessions().is_empty()
    })
    .await;

    env.send(
        Actor::Stakeholder,
        Command::DropTicket {
            id: "T-1".into(),
            reason: "no longer wanted".into(),
        },
    )
    .await;
    env.until("the session and worktree are gone", |_| {
        env.runtime.ticket_sessions().is_empty() && !env.worktree("T-1").exists()
    })
    .await;
    // The work is kept on its branch, and nothing restarts.
    assert!(git(env.repo.path(), &["branch", "--list", "feat/T-1-*"]).contains("feat/T-1"));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(env.runtime.ticket_sessions().is_empty());
    let state = env.state().await;
    let ticket = state.ticket("T-1").expect("ticket");
    assert_eq!(ticket.dropped.as_deref(), Some("no longer wanted"));
    assert_eq!(ticket.assignee, None);
}

#[tokio::test]
async fn asking_for_the_next_sprint_briefs_the_planner_with_the_backlog() {
    let env = Env::new(None, &["Dana"]).await;
    env.send(
        Actor::Employee("pam".into()),
        Command::CreateTicket {
            kind: TicketKind::Story,
            title: "Checkout page".into(),
            description: String::new(),
            acceptance_criteria: vec![],
            estimate: Some(3),
            epic: None,
            external: None,
        },
    )
    .await;
    env.send(
        Actor::Stakeholder,
        Command::RequestSprintPlanning {
            note: Some("payments first".into()),
        },
    )
    .await;
    env.until("Pam is briefed", |s| {
        s.chats["pam"].iter().any(|l| {
            l.text.contains("plan the next sprint")
                && l.text.contains("T-1 (story, 3 pt): Checkout page")
                && l.text.contains("payments first")
        })
    })
    .await;
    env.until("the request is marked briefed", |s| {
        s.planning.as_ref().is_some_and(|p| p.briefed)
    })
    .await;
}

/// What the PM's `create_story` with `assignee` and `request_id` does.
async fn file_for_request(env: &Env, request: &str, title: &str, description: &str, to: &str) {
    let pam = || Actor::Employee("pam".into());
    let id = format!("T-{}", env.state().await.tickets.len() + 1);
    env.send(
        pam(),
        Command::CreateTicket {
            kind: TicketKind::Story,
            title: title.into(),
            description: description.into(),
            acceptance_criteria: vec!["works".into()],
            estimate: None,
            epic: None,
            external: None,
        },
    )
    .await;
    env.send(
        pam(),
        Command::AssignTicket {
            id: id.clone(),
            assignee: Some(to.into()),
        },
    )
    .await;
    env.send(
        pam(),
        Command::LinkRequestTicket {
            request: request.into(),
            ticket: id,
        },
    )
    .await;
}

#[tokio::test]
async fn a_request_that_starts_now_is_briefed_filed_assigned_and_built() {
    let env = Env::new(None, &["Dana"]).await;
    env.send(
        Actor::Stakeholder,
        Command::SubmitRequest {
            text: "Say hello on the home page".into(),
            start_now: true,
        },
    )
    .await;
    env.until("Pam is briefed with the request and the roster", |s| {
        s.chats["pam"].iter().any(|l| {
            l.text.contains("Request R-1")
                && l.text.contains("Say hello on the home page")
                && l.text.contains("Dana (id `dana`)")
                && l.text.contains("started now")
        })
    })
    .await;
    env.until("the request is marked briefed", |s| {
        s.request("R-1")
            .is_some_and(|r| r.status == chillax_core::RequestStatus::Briefed)
    })
    .await;

    file_for_request(&env, "R-1", "Say hello", "write file hello.txt", "dana").await;
    // start_now: the request approved a sprint, so Dana starts without anyone else.
    env.state_is("T-1", "in_progress").await;
    env.file_appears(env.worktree("T-1").join("hello.txt"))
        .await;

    env.send(
        Actor::Employee("pam".into()),
        Command::CompleteRequest {
            id: "R-1".into(),
            summary: "T-1 to Dana".into(),
        },
    )
    .await;
    let state = env.state().await;
    assert_eq!(
        state.request("R-1").map(|r| r.status),
        Some(chillax_core::RequestStatus::Triaged)
    );
    assert!(state.inbox.iter().any(|i| matches!(i,
        InboxItem::Alert { title, detail, .. }
            if title == "Request R-1 filed" && detail.contains("Dana"))));
    // No second briefing for a finished request.
    let briefings = |s: &State| {
        s.chats["pam"]
            .iter()
            .filter(|l| l.text.contains("Request R-1"))
            .count()
    };
    let before = briefings(&state);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(briefings(&env.state().await), before);
}

#[tokio::test]
async fn a_request_without_start_now_leaves_tickets_in_the_backlog() {
    let env = Env::new(None, &["Dana"]).await;
    env.send(
        Actor::Stakeholder,
        Command::SubmitRequest {
            text: "Say hello".into(),
            start_now: false,
        },
    )
    .await;
    env.until("Pam is briefed", |s| {
        s.chats["pam"]
            .iter()
            .any(|l| l.text.contains("wait in the backlog"))
    })
    .await;
    file_for_request(&env, "R-1", "Say hello", "write file hello.txt", "dana").await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let state = env.state().await;
    let t = state.ticket("T-1").expect("ticket");
    assert_eq!(
        (t.state.as_str(), t.assignee.as_deref()),
        ("backlog", Some("dana"))
    );
    assert!(state.sprints.is_empty());
}

fn free_ports() -> [u16; 3] {
    let listeners: Vec<_> = (0..3)
        .map(|_| std::net::TcpListener::bind("127.0.0.1:0").expect("bind"))
        .collect();
    let ports: Vec<u16> = listeners
        .iter()
        .map(|l| l.local_addr().expect("addr").port())
        .collect();
    [ports[0], ports[1], ports[2]]
}

fn app_spec(env: &str, port: u16, marks: &Path) -> AppSpec {
    let m = marks.display();
    AppSpec {
        env: env.into(),
        command: format!("echo $$ >> {m}/{env}.pids; ls > {m}/{env}.ls; sleep 30"),
        port,
        url: format!("http://localhost:{port}"),
        cwd: None,
    }
}

fn lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect()
}

fn alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[tokio::test]
async fn the_apps_follow_dev_qa_and_the_release_tag_and_obey_the_stakeholder() {
    let marks = tempfile::tempdir().expect("tempdir");
    let [dev_port, qa_port, prod_port] = free_ports();
    let m = marks.path();
    let env = Env::with_apps(
        &["Dana"],
        vec![
            app_spec("dev", dev_port, m),
            app_spec("qa", qa_port, m),
            app_spec("prod", prod_port, m),
        ],
    )
    .await;

    // dev and qa start at once from their integration worktrees; prod waits for a tag.
    env.until("dev and qa run, prod waits", |s| {
        s.app("dev").is_some_and(|a| a.is_running())
            && s.app("qa").is_some_and(|a| a.is_running())
            && s.app("prod")
                .is_some_and(|a| a.reason.as_deref() == Some("no release yet"))
    })
    .await;
    let s = env.state().await;
    assert_eq!(
        s.app("dev").expect("dev").url,
        format!("http://localhost:{dev_port}")
    );
    assert_eq!(
        s.app("dev").expect("dev").commit.as_deref(),
        Some(git(env.repo.path(), &["rev-parse", "dev"]).as_str())
    );
    assert!(!s.app("prod").expect("prod").is_running());
    env.file_appears(m.join("dev.ls")).await;
    assert!(!lines(&m.join("dev.ls")).contains(&"hello.txt".to_string()));
    let dev_first = lines(&m.join("dev.pids"));
    assert_eq!(dev_first.len(), 1);

    // A merge to dev restarts the dev app on the new code.
    env.sprint(&[("Say hello", "write file hello.txt", "dana")])
        .await;
    env.merge_ticket("T-1", "dana").await;
    env.until("dev restarted on the merge", |s| {
        let dev = git(env.repo.path(), &["rev-parse", "dev"]);
        s.app("dev")
            .is_some_and(|a| a.is_running() && a.commit.as_deref() == Some(dev.as_str()))
    })
    .await;
    env.until("dev serves hello.txt", |_| {
        lines(&m.join("dev.ls")).contains(&"hello.txt".to_string())
    })
    .await;
    let dev_pids = lines(&m.join("dev.pids"));
    assert!(dev_pids.len() >= 2, "{dev_pids:?}");
    env.until("the old dev process is gone", |_| !alive(&dev_pids[0]))
        .await;

    // Promotion restarts qa on the promoted code.
    env.until("qa serves hello.txt", |_| {
        lines(&m.join("qa.ls")).contains(&"hello.txt".to_string())
    })
    .await;
    assert!(env.state().await.app("qa").expect("qa").restarts >= 1);

    // Stop and start by hand: a stopped app stays stopped.
    env.send(Actor::Stakeholder, Command::StopApp { env: "dev".into() })
        .await;
    env.until("dev is stopped", |s| {
        s.app("dev").is_some_and(|a| {
            a.status == chillax_core::AppStatus::Stopped
                && a.reason.as_deref() == Some("stopped by you")
        })
    })
    .await;
    let last = lines(&m.join("dev.pids")).pop().expect("a pid");
    env.until("the dev process ends", |_| !alive(&last)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(env.dispatcher.apps().live().get("dev"), None);
    env.send(Actor::Stakeholder, Command::StartApp { env: "dev".into() })
        .await;
    env.until("dev runs again", |s| {
        s.app("dev").is_some_and(|a| a.is_running())
    })
    .await;
    env.send(
        Actor::Stakeholder,
        Command::RestartApp { env: "dev".into() },
    )
    .await;
    let before = lines(&m.join("dev.pids")).len();
    env.until("dev restarted by hand", |s| {
        lines(&m.join("dev.pids")).len() > before && s.app("dev").is_some_and(|a| a.is_running())
    })
    .await;

    // The release starts prod from the tag, detached in the main worktree.
    env.pass("T-1").await;
    env.until("the release waits at the gate", |s| {
        s.releases
            .last()
            .is_some_and(|r| r.status == ReleaseStatus::Gated)
    })
    .await;
    let release = env.state().await.release("R-1").expect("release").clone();
    env.send(
        Actor::Stakeholder,
        Command::ApproveRelease {
            id: "R-1".into(),
            version: "v0.1.0".into(),
            notes: release.notes.clone(),
        },
    )
    .await;
    env.until("prod runs the tag", |s| {
        s.app("prod").is_some_and(|a| a.is_running())
    })
    .await;
    let tag = git(env.repo.path(), &["rev-list", "-n1", "v0.1.0"]);
    assert_eq!(
        env.state()
            .await
            .app("prod")
            .expect("prod")
            .commit
            .as_deref(),
        Some(tag.as_str())
    );
    env.until("prod serves the release", |_| {
        let ls = lines(&m.join("prod.ls"));
        ls.contains(&"hello.txt".to_string()) && ls.contains(&"CHANGELOG.md".to_string())
    })
    .await;
    assert_eq!(
        git(
            &env.repo.path().join(".chillax/worktrees/_main"),
            &["rev-parse", "HEAD"]
        ),
        tag
    );
    // Prod shares the main worktree; there is no separate `_prod` (ADR 0025).
    assert!(!env.repo.path().join(".chillax/worktrees/_prod").exists());

    // Quitting ends every app.
    let all: Vec<String> = ["dev", "qa", "prod"]
        .iter()
        .filter_map(|e| lines(&m.join(format!("{e}.pids"))).pop())
        .collect();
    assert_eq!(all.len(), 3);
    env.dispatcher.apps().shutdown().await;
    for pid in &all {
        env.until("an app process to end", |_| !alive(pid)).await;
    }
}

#[tokio::test]
async fn an_app_that_keeps_crashing_raises_one_alert_until_it_is_restarted() {
    let marks = tempfile::tempdir().expect("tempdir");
    let [dev_port, _, _] = free_ports();
    let mut crashing = app_spec("dev", dev_port, marks.path());
    crashing.command = "echo kaboom >&2; exit 7".into();
    let env = Env::with_apps(&["Dana"], vec![crashing]).await;
    env.until("the crash alert", |s| {
        s.inbox.iter().any(|i| {
            matches!(i, InboxItem::Alert { title, detail, .. }
                if title == "The dev app keeps crashing" && detail.contains("kaboom"))
        })
    })
    .await;
    let s = env.state().await;
    assert_eq!(
        s.app("dev").expect("dev").status,
        chillax_core::AppStatus::Crashed
    );
    assert!(env.dispatcher.apps().live().is_empty());
    // Asking for a restart tries again (and crashes again, with one alert in the end).
    env.send(
        Actor::Stakeholder,
        Command::RestartApp { env: "dev".into() },
    )
    .await;
    env.until("it was tried again and the alert is back", |s| {
        s.app("dev").is_some_and(|a| {
            a.restarts >= 3 && a.status == chillax_core::AppStatus::Crashed
        }) && s.inbox.iter().any(
            |i| matches!(i, InboxItem::Alert { title, .. } if title == "The dev app keeps crashing"),
        )
    })
    .await;
    let alerts = env
        .state()
        .await
        .inbox
        .iter()
        .filter(|i| matches!(i, InboxItem::Alert { title, .. } if title == "The dev app keeps crashing"))
        .count();
    assert_eq!(alerts, 1);
}

#[tokio::test]
async fn consecutive_tickets_for_a_developer_get_distinct_sessions() {
    let env = Env::new(None, &["Dana"]).await;
    let mut rx = env.hub.subscribe();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    tokio::spawn(async move {
        while let Ok(event) = rx.recv().await {
            sink.lock().expect("lock").push(event.kind);
        }
    });
    env.sprint(&[
        ("First", "write file a.txt", "dana"),
        ("Second", "write file b.txt", "dana"),
    ])
    .await;
    env.merge_ticket("T-1", "dana").await;
    env.state_is("T-2", "in_progress").await;
    env.until("dana moves on to T-2 alone", |_| {
        env.runtime
            .ticket_sessions()
            .contains(&("dana".to_string(), "T-2".to_string()))
            && !env
                .runtime
                .ticket_sessions()
                .contains(&("dana".to_string(), "T-1".to_string()))
    })
    .await;
    let started = seen
        .lock()
        .expect("lock")
        .iter()
        .filter(|k| {
            matches!(k, chillax_core::EventKind::AgentStarted { employee, .. } if employee == "dana")
        })
        .count();
    assert_eq!(started, 2, "one session per ticket, never one carried over");
    assert_eq!(
        env.runtime.ticket_sessions(),
        [("dana".to_string(), "T-2".to_string())].into()
    );
}

#[tokio::test]
async fn a_session_left_open_on_another_ticket_is_closed_when_the_employee_starts_a_new_one() {
    let env = Env::new(None, &["Dana", "Dan"]).await;
    env.sprint(&[
        ("First", "write file a.txt", "dana"),
        ("Second", "write file b.txt", "dan"),
        // Left unassigned until Dana is busy: handing it to her at once races the start
        // of T-1 (see the cap note in the report).
        ("Third", "write file c.txt", ""),
    ])
    .await;
    env.state_is("T-1", "in_progress").await;
    env.until("dana is on T-1 and dan on T-2", |_| {
        let open = env.runtime.ticket_sessions();
        open.contains(&("dana".to_string(), "T-1".to_string()))
            && open.contains(&("dan".to_string(), "T-2".to_string()))
    })
    .await;
    assert!(
        !env.runtime
            .ticket_sessions()
            .contains(&("dana".to_string(), "T-3".to_string())),
        "dana has one ticket at a time"
    );

    env.send(
        Actor::Stakeholder,
        Command::AssignTicket {
            id: "T-3".into(),
            assignee: Some("dana".into()),
        },
    )
    .await;

    // T-1 moves to Dan: Dana's session no longer waits on her but stays open until she
    // starts something else. Dan is busy, so T-1 simply waits.
    env.send(
        Actor::Stakeholder,
        Command::AssignTicket {
            id: "T-1".into(),
            assignee: Some("dan".into()),
        },
    )
    .await;
    env.until("dana starts T-3 and her T-1 session is gone", |_| {
        let open = env.runtime.ticket_sessions();
        open.contains(&("dana".to_string(), "T-3".to_string()))
            && !open.contains(&("dana".to_string(), "T-1".to_string()))
    })
    .await;
}

fn dev_tree(env: &Env) -> PathBuf {
    env.repo.path().join(".chillax/worktrees/_dev")
}

/// The directories under `.chillax/worktrees`, sorted.
fn worktree_dirs(env: &Env) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(env.repo.path().join(".chillax/worktrees"))
        .map(|dir| {
            dir.flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

#[tokio::test]
async fn in_shared_mode_tickets_take_turns_on_one_dev_worktree() {
    let env = Env::shared(None, &["Dana", "Dave"]).await;
    env.sprint(&[
        ("First", "write file a.txt", "dana"),
        ("Second", "write file b.txt", "dave"),
    ])
    .await;

    // The first ticket holds the dev worktree on its own branch; the second waits.
    env.state_is("T-1", "in_progress").await;
    env.file_appears(dev_tree(&env).join("a.txt")).await;
    assert_eq!(
        git(&dev_tree(&env), &["branch", "--show-current"]),
        "feat/T-1-first"
    );
    let state = env.state().await;
    assert_eq!(
        state.ticket("T-1").expect("ticket").worktree.as_deref(),
        Some(".chillax/worktrees/_dev")
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        env.state().await.ticket("T-2").expect("ticket").state,
        "ready"
    );
    assert!(!dev_tree(&env).join("b.txt").exists());

    env.request_review("T-1", "dana").await;
    env.state_is("T-1", "in_review").await;
    env.review("T-1", ReviewVerdict::Approve, "ok").await;
    env.state_is("T-1", "in_qa").await;
    assert_eq!(git(env.repo.path(), &["show", "dev:a.txt"]), "a.txt");

    // Now the second ticket gets the worktree, cut from the dev that has the first.
    env.state_is("T-2", "in_progress").await;
    env.file_appears(dev_tree(&env).join("b.txt")).await;
    assert_eq!(
        git(&dev_tree(&env), &["branch", "--show-current"]),
        "feat/T-2-second"
    );
    assert!(dev_tree(&env).join("a.txt").exists());
    env.request_review("T-2", "dave").await;
    env.state_is("T-2", "in_review").await;
    env.review("T-2", ReviewVerdict::Approve, "ok").await;
    env.state_is("T-2", "in_qa").await;

    // Nothing but the environment worktrees ever existed, and `_dev` is back on `dev`.
    env.until("the dev worktree is parked on dev", |_| {
        env.runtime.ticket_sessions().is_empty()
            && std::fs::read_to_string(dev_tree(&env).join(".git")).is_ok()
            && git(&dev_tree(&env), &["branch", "--show-current"]) == "dev"
    })
    .await;
    assert!(git(&dev_tree(&env), &["status", "--porcelain"]).is_empty());
    assert_eq!(git(env.repo.path(), &["show", "dev:b.txt"]), "b.txt");
    assert!(
        worktree_dirs(&env)
            .iter()
            .all(|d| ["_dev", "_qa", "_main"].contains(&d.as_str())),
        "{:?}",
        worktree_dirs(&env)
    );
}

#[tokio::test]
async fn dropping_the_ticket_in_the_shared_worktree_keeps_its_work_and_returns_to_dev() {
    let env = Env::shared(None, &["Dana"]).await;
    env.sprint(&[("Say hello", "write file hello.txt", "dana")])
        .await;
    env.state_is("T-1", "in_progress").await;
    env.file_appears(dev_tree(&env).join("hello.txt")).await;
    env.until("the developer has a session", |_| {
        !env.runtime.ticket_sessions().is_empty()
    })
    .await;

    env.send(
        Actor::Stakeholder,
        Command::DropTicket {
            id: "T-1".into(),
            reason: "no longer wanted".into(),
        },
    )
    .await;
    env.until("the dev worktree is back on dev", |_| {
        env.runtime.ticket_sessions().is_empty()
            && git(&dev_tree(&env), &["branch", "--show-current"]) == "dev"
    })
    .await;
    // The work sits on the ticket branch; the worktree stays, clean, without it.
    assert_eq!(
        git(env.repo.path(), &["show", "feat/T-1-say-hello:hello.txt"]),
        "hello.txt"
    );
    assert!(git(&dev_tree(&env), &["status", "--porcelain"]).is_empty());
    assert!(!dev_tree(&env).join("hello.txt").exists());
}

#[tokio::test]
async fn finished_work_empties_the_employees_transcript_but_only_then() {
    use chillax_core::EventKind;
    let env = Env::new(Some("test -f hello.txt"), &["Dana"]).await;
    let mut rx = env.hub.subscribe();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    tokio::spawn(async move {
        while let Ok(event) = rx.recv().await {
            sink.lock().expect("lock").push(event.kind);
        }
    });
    let cleared = || -> Vec<(String, String)> {
        seen.lock()
            .expect("lock")
            .iter()
            .filter_map(|k| match k {
                EventKind::EmployeeContextCleared { employee, ticket } => {
                    Some((employee.clone(), ticket.clone()))
                }
                _ => None,
            })
            .collect()
    };
    env.sprint(&[("Say hello", "write file hello.txt", "dana")])
        .await;
    env.state_is("T-1", "in_progress").await;
    env.file_appears(env.worktree("T-1").join("hello.txt"))
        .await;
    // Mid-work: nothing is cleared and the developer's session stays.
    assert!(cleared().is_empty());

    // Tests pass: the developer's work is handed over, so their transcript is emptied.
    env.request_review("T-1", "dana").await;
    env.until("dana's context is cleared", |s| {
        cleared().contains(&("dana".to_string(), "T-1".to_string()))
            && s.transcript("dana").is_empty()
    })
    .await;
    // Only ticket workers are ever cleared: never the planner or a chat session.
    assert!(
        cleared().iter().all(|(e, _)| e == "dana" || e == "lee"),
        "{:?}",
        cleared()
    );
}

#[tokio::test]
async fn a_reload_changes_the_worktree_mode_and_the_qa_steps_for_new_work() {
    use chillax_core::EnvStep;
    let env = Env::shared(None, &["ann"]).await;
    assert_eq!(env.dispatcher.worktree_mode(), WorktreeMode::Shared);
    let mut live = LiveSettings::from_config(&{
        let mut c = DispatchConfig::new(env.repo.path(), "dev");
        c.worktrees = WorktreeMode::PerTicket;
        c.qa.up = Some("echo migrated".into());
        c
    });
    live.qa.timeout_secs = 30;
    env.dispatcher.reload(live);
    assert_eq!(env.dispatcher.worktree_mode(), WorktreeMode::PerTicket);
    let run = env
        .dispatcher
        .environment()
        .run("qa", EnvStep::Up, env.repo.path())
        .await
        .expect("runs");
    assert!(!run.skipped && run.ok, "{}", run.output_tail);
}

#[tokio::test]
async fn a_conflict_in_the_shared_worktree_is_resolved_there_and_merged() {
    let env = Env::shared(None, &["Dana"]).await;
    env.sprint(&[("First", "nothing to write", "dana")]).await;
    env.state_is("T-1", "in_progress").await;
    env.until("the developer has a session", |_| {
        !env.runtime.ticket_sessions().is_empty()
    })
    .await;
    std::fs::write(dev_tree(&env).join("same.txt"), "one\n").expect("write");
    env.request_review("T-1", "dana").await;
    env.state_is("T-1", "in_review").await;
    // Someone else lands a conflicting change on dev meanwhile.
    let other = tempfile::tempdir().expect("tempdir");
    let other = other.path().join("dev");
    let dir = other.to_string_lossy().to_string();
    git(
        env.repo.path(),
        &["worktree", "add", "--quiet", &dir, "dev"],
    );
    std::fs::write(other.join("same.txt"), "other\n").expect("write");
    git(&other, &["add", "-A"]);
    git(&other, &["commit", "--quiet", "-m", "other"]);
    git(env.repo.path(), &["worktree", "remove", "--force", &dir]);

    env.review("T-1", ReviewVerdict::Approve, "ok").await;
    env.conflict_briefed("T-1", 1).await;
    assert_eq!(
        git(&dev_tree(&env), &["branch", "--show-current"]),
        "feat/T-1-first",
        "the dev worktree stays on the ticket for the resolution"
    );
    resolve(&dev_tree(&env), "same.txt", "one\nother\n");
    env.request_review_quietly("T-1", "dana").await;
    env.state_is("T-1", "in_qa").await;
    assert_eq!(
        git(env.repo.path(), &["show", "dev:same.txt"]),
        "one\nother"
    );
}

#[tokio::test]
async fn on_github_a_branch_synced_with_dev_is_pushed_before_its_pull_request_merges() {
    let env = Env::on_github(None, &["Dana", "Dan"]).await;
    env.sprint(&[
        ("First", "nothing to write", "dana"),
        ("Second", "nothing to write", "dan"),
    ])
    .await;
    env.state_is("T-1", "in_progress").await;
    env.state_is("T-2", "in_progress").await;
    env.until("both developers have sessions", |_| {
        env.runtime.ticket_sessions().len() == 2
    })
    .await;
    std::fs::write(env.worktree("T-1").join("a.txt"), "a\n").expect("write");
    std::fs::write(env.worktree("T-2").join("b.txt"), "b\n").expect("write");
    // One at a time: the fake `gh` numbers pull requests without a lock.
    env.request_review("T-1", "dana").await;
    env.state_is("T-1", "in_review").await;
    env.request_review("T-2", "dan").await;
    env.state_is("T-2", "in_review").await;
    env.review("T-1", ReviewVerdict::Approve, "ok").await;
    env.state_is("T-1", "in_qa").await;
    env.review("T-2", ReviewVerdict::Approve, "ok").await;
    env.state_is("T-2", "in_qa").await;

    let (remote, _) = env.github.as_ref().expect("github mode");
    let remote = remote.path();
    let branch = env
        .state()
        .await
        .ticket("T-2")
        .and_then(|t| t.branch.clone())
        .expect("branch");
    assert_eq!(
        git(remote, &["show", &format!("{branch}:a.txt")]),
        "a",
        "the pushed branch has dev merged in"
    );
    assert!(
        comments(&env.state().await, "T-2")
            .iter()
            .any(|c| c.starts_with("Merged dev into the branch"))
    );
    assert_eq!(git(remote, &["show", "dev:b.txt"]), "b");
}
