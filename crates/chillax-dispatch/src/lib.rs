//! The dev loop. It watches the company, asks the pure
//! [`next_actions`] what should happen next, and does it: creates a worktree and branch
//! per ticket, opens developer and reviewer sessions, runs the project's tests, merges
//! approved branches into `dev` and rebases the branches still being built. From M4 it
//! also promotes `dev` to `qa`, runs the QA environment, opens QA sessions, closes the
//! round, and makes the release: merge to the prod branch, tag, back-merge, deploy.
//! Every result is recorded as a command, so the event log tells the whole story.
//!
//! Agents never touch git beyond committing in their own worktree (AGENTS.md rule 4, 8).

mod apps;
mod apps_detect;
mod briefing;
mod cleanup;
mod environment;
mod problems;
mod release;
mod shell;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use chillax_agents::{AgentRuntime, EnvFiles, SessionKey, TicketSession};
use chillax_core::agents::backend_title;
use chillax_core::apps::AppTarget;
use chillax_core::config::{AppSpec, ProdEnv, QaEnv, WorktreeMode};
use chillax_core::dispatch::{
    Action, CheckinCtx, Context, KEY_RELEASE, SessionKind, checkin_wanted, next_actions,
    session_is_waiting,
};
use chillax_core::engine::{Engine, slug};
use chillax_core::env::Environment;
use chillax_core::limits::{self, FailureKind, Jitter};
use chillax_core::model::TicketKind;
use chillax_core::problems::ProblemKind;
use chillax_core::stall::{BusySession, StallCtx};
use chillax_core::vcs::{FastForward, MergeOutcome, RebaseResult, SyncResult, VcsHost};
use chillax_core::workflow::{IN_PROGRESS, IN_REVIEW};
use chillax_core::{
    Actor, Command, CompanyHandle, EnvStep, Event, EventKind, Hub, InboxItem, QaVerdict,
    RebaseOutcome, ReleaseStatus, ReviewVerdict, SyncOutcome, Ticket, TicketId,
};
use tokio::sync::{Notify, broadcast};
use tokio::task::JoinHandle;

pub use apps::{AppSupervisor, AppsConfig, kill_all_processes};
pub use apps_detect::{AppDetector, AppProbe, Listener, LsofProbe};
pub use environment::ShellEnvironment;
pub use shell::{ShellResult, run_shell, run_shell_with};

/// The most diff a reviewer's briefing carries; the rest is theirs to read in the tree.
/// How long a session whose agent just submitted its outcome may keep its turn running
/// before it is cancelled. Cancelling at once aborts the tool call that carried the
/// outcome ("MCP request aborted") before its response reaches the agent.
const TURN_GRACE: Duration = Duration::from_secs(10);
const DIFF_BYTES: usize = 60_000;

/// The dev worktree, relative to the repository root (`chillax-vcs` makes it). In shared
/// mode every ticket records it as its worktree.
const QA_SCRATCH: &str = ".chillax/state/qa";
const DEV_WORKTREE: &str = ".chillax/worktrees/_dev";

pub struct DispatchConfig {
    /// The repository root; worktrees live under `.chillax/worktrees/`.
    pub root: PathBuf,
    /// `[env.dev] base_branch`.
    pub base_branch: String,
    /// `[env.dev] test`. Without one, tests count as skipped.
    pub test: Option<String>,
    pub test_timeout: Duration,
    /// `[env.qa]`: the branch `dev` is promoted to and its environment commands.
    pub qa: QaEnv,
    /// `[env.prod]`: the branch releases land on and the deploy command.
    pub prod: ProdEnv,
    /// `[budget] max_concurrent_agents`.
    pub max_concurrent: usize,
    /// The live value of that limit, shared with the runtime. The dispatcher
    /// stores each `max_concurrent_agents_changed` into it before it decides again.
    pub max_concurrent_live: Option<Arc<AtomicUsize>>,
    /// `[vcs] worktrees`: ticket work in the shared dev worktree (default) or one per ticket.
    pub worktrees: WorktreeMode,
    /// How often to look again when nothing happens (retries, deferred rebases).
    pub poll: Duration,
    /// How long a ticket is left alone after an action on it failed.
    pub cooldown: Duration,
    /// First and longest wait after a ticket's agent session failed; it doubles per
    /// failure in a row.
    pub backoff_base: Duration,
    pub backoff_cap: Duration,
    /// How long after a turn ends without the ticket moving to decide the agent is stuck.
    pub idle_grace: Duration,
    /// How long a ticket whose agent went idle twice is left alone (a refresh ends it).
    pub park: Duration,
    /// How long the project manager gets to file a bug after a failure briefing before
    /// they are briefed again.
    pub triage_retry: Duration,
    /// How often the project manager gets a board check-in while work is open
    /// (`[team] checkin_minutes`). Zero turns them off.
    pub checkin: Duration,
    /// How long open sprint work may go without moving before the stall watchdog acts
    /// (`[team] stall_minutes`). Zero turns it off.
    pub stall: Duration,
    /// How long the project manager gets for a problem before the stakeholder is told
    /// (`[team] problem_grace_minutes`, ADR 0032). Zero alerts at once, as before.
    pub problem_grace: Duration,
    /// Most briefings one problem gets.
    pub problem_max_attempts: u32,
    /// Wait between briefings about one problem.
    pub problem_retry: Duration,
    /// Randomness for probe timing; tests pin it.
    pub jitter: Arc<dyn Jitter>,
    /// `[release] sync_after_deploy`: converge dev, qa and prod after a deploy.
    pub sync_after_deploy: bool,
    /// The apps to keep running (`run` + `port` under `[env.*]`, ADR 0015).
    pub apps: Vec<AppSpec>,
    /// Restart timing for those apps.
    pub apps_config: AppsConfig,
    /// The project's `.env` files, for the environment commands and apps.
    pub env_files: EnvFiles,
}

impl DispatchConfig {
    pub fn new(root: impl Into<PathBuf>, base_branch: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            base_branch: base_branch.into(),
            test: None,
            test_timeout: Duration::from_secs(600),
            qa: QaEnv::default(),
            prod: ProdEnv::default(),
            max_concurrent: 3,
            max_concurrent_live: None,
            worktrees: WorktreeMode::default(),
            poll: Duration::from_secs(5),
            cooldown: Duration::from_secs(30),
            backoff_base: Duration::from_secs(30),
            backoff_cap: Duration::from_secs(15 * 60),
            idle_grace: Duration::from_secs(3),
            park: Duration::from_secs(30 * 60),
            triage_retry: Duration::from_secs(10 * 60),
            checkin: Duration::ZERO,
            stall: Duration::ZERO,
            problem_grace: Duration::ZERO,
            problem_max_attempts: 2,
            problem_retry: Duration::from_secs(5 * 60),
            jitter: Arc::new(limits::ClockJitter),
            sync_after_deploy: true,
            apps: Vec::new(),
            apps_config: AppsConfig::default(),
            env_files: EnvFiles::default(),
        }
    }
}

/// The settings a config reload can change while the dispatcher runs. Everything here is
/// read fresh each time it is used; branch names, ports and the like need a restart.
#[derive(Clone)]
pub struct LiveSettings {
    /// `[vcs] worktrees`. Tickets already started keep the worktree they have.
    pub worktrees: WorktreeMode,
    pub test: Option<String>,
    pub test_timeout: Duration,
    pub qa: QaEnv,
    pub prod: ProdEnv,
    pub sync_after_deploy: bool,
    pub checkin: Duration,
    pub stall: Duration,
    pub problem_grace: Duration,
    pub problem_max_attempts: u32,
    pub problem_retry: Duration,
}

impl LiveSettings {
    /// The live settings a `chillax.toml` asks for.
    pub fn from_project(config: &chillax_core::Config) -> Self {
        let team = &config.team;
        Self {
            worktrees: config.vcs.worktrees,
            test: config.env.dev.test.clone(),
            test_timeout: Duration::from_secs(config.env.dev.test_timeout_secs),
            qa: config.env.qa.clone(),
            prod: config.env.prod.clone(),
            sync_after_deploy: config.release.sync_after_deploy,
            checkin: Duration::from_secs(u64::from(team.checkin_minutes) * 60),
            stall: Duration::from_secs(u64::from(team.stall_minutes) * 60),
            problem_grace: Duration::from_secs(u64::from(team.problem_grace_minutes) * 60),
            problem_max_attempts: team.problem_max_attempts,
            problem_retry: Duration::from_secs(u64::from(team.problem_retry_minutes) * 60),
        }
    }

    pub fn from_config(config: &DispatchConfig) -> Self {
        Self {
            worktrees: config.worktrees,
            test: config.test.clone(),
            test_timeout: config.test_timeout,
            qa: config.qa.clone(),
            prod: config.prod.clone(),
            sync_after_deploy: config.sync_after_deploy,
            checkin: config.checkin,
            stall: config.stall,
            problem_grace: config.problem_grace,
            problem_max_attempts: config.problem_max_attempts,
            problem_retry: config.problem_retry,
        }
    }
}

/// What the runner remembers between the project manager's board check-ins.
/// It lives in memory only: a restart starts a fresh interval.
struct CheckinBook {
    /// When the last interval ended, whether it sent a check-in or skipped one. Starts
    /// at construction, so nothing is sent at boot.
    last_sent: Instant,
    /// The board's fingerprint when the last check-in went out.
    last_fp: Option<String>,
    /// Intervals in a row that came due with the board unchanged.
    unchanged_intervals: u32,
    /// Per ticket: its last signature and the intervals it has stayed the same.
    tickets: HashMap<TicketId, (String, u32)>,
}

impl Default for CheckinBook {
    fn default() -> Self {
        Self {
            last_sent: Instant::now(),
            last_fp: None,
            unchanged_intervals: 0,
            tickets: HashMap::new(),
        }
    }
}

impl CheckinBook {
    /// Close an interval: remember each ticket's signature and how long it has not moved.
    fn note_tickets(&mut self, engine: &Engine) {
        let mut seen = HashMap::new();
        for t in &engine.state.tickets {
            let sig = format!(
                "{}|{:?}|{}|{}|{}",
                t.state,
                t.assignee,
                t.checks.len(),
                t.branch.is_some(),
                t.review_requested
            );
            let unchanged = match self.tickets.get(&t.id) {
                Some((prev, n)) if *prev == sig => n + 1,
                _ => 0,
            };
            seen.insert(t.id.clone(), (sig, unchanged));
        }
        self.tickets = seen;
    }

    fn stalled(&self) -> BTreeMap<TicketId, u32> {
        self.tickets
            .iter()
            .filter(|(_, (_, n))| *n > 0)
            .map(|(id, (_, n))| (id.clone(), *n))
            .collect()
    }
}

/// What the runner remembers about the current stall window. In memory only:
/// a restart counts as movement, so the window starts again.
struct StallBook {
    /// When the board last moved, or when the watchdog last had nothing to watch.
    last_progress: Instant,
    /// When the project manager was briefed about this stall.
    briefed_at: Option<Instant>,
    /// The stakeholder was told about this stall.
    alerted: bool,
    /// When each employee's agent last showed activity (output, a tool call, a plan).
    activity: HashMap<String, Instant>,
    /// Hung turns closed in this window, for the briefing ("Dana on T-3").
    closed: Vec<String>,
}

impl Default for StallBook {
    fn default() -> Self {
        Self {
            last_progress: Instant::now(),
            briefed_at: None,
            alerted: false,
            activity: HashMap::new(),
            closed: Vec::new(),
        }
    }
}

impl StallBook {
    /// The board moved (or there is nothing to watch): a new window starts. True when the
    /// stakeholder had been alerted, so the alert can be cleared.
    fn restart(&mut self) -> bool {
        self.last_progress = Instant::now();
        self.briefed_at = None;
        self.closed.clear();
        std::mem::take(&mut self.alerted)
    }
}

/// Events that mean the board moved, for the stall watchdog. Chillax's own
/// comments (a failed action, a closed hung turn) are not movement.
fn is_progress(event: &Event) -> bool {
    match &event.kind {
        EventKind::TicketCommented { .. } => event.actor != Actor::System,
        kind => matches!(
            kind,
            EventKind::TicketTransitioned { .. }
                | EventKind::TicketAssigned { .. }
                | EventKind::WorkStarted { .. }
                | EventKind::ReviewRequested { .. }
                | EventKind::TestsRun { .. }
                | EventKind::ReviewStarted { .. }
                | EventKind::ReviewSubmitted { .. }
                | EventKind::BranchMerged { .. }
                | EventKind::MergeFailed { .. }
                | EventKind::BranchRebased { .. }
                | EventKind::TicketHandedOff { .. }
                | EventKind::BranchSynced { .. }
                | EventKind::QaVerdictRecorded { .. }
                | EventKind::EnvCommandRun { .. }
                | EventKind::DeployRun { .. }
        ),
    }
}

#[derive(Default)]
struct Book {
    /// The project manager's board check-ins.
    checkin: CheckinBook,
    /// The stall watchdog's window.
    stall: StallBook,
    /// Problems deferred to the project manager, by key (ADR 0032). In memory: a restart
    /// forgets them.
    problems: HashMap<String, problems::ProblemRec>,
    /// Tickets with an action running.
    inflight: BTreeSet<TicketId>,
    /// Tickets left alone until the instant, after a failure.
    cooling: HashMap<TicketId, Instant>,
    /// Failed employees left alone until the instant, so a crashing agent is not
    /// restarted in a tight loop; afterwards `Failed` no longer blocks picking them.
    employee_cooling: HashMap<String, Instant>,
    /// Tickets whose current failure was already written on the ticket.
    reported: HashSet<TicketId>,
    /// Failed sessions in a row, per ticket; any finished turn resets it.
    failures: HashMap<TicketId, u32>,
    /// Failed actions in a row, per ticket; stretches the cooldown. Success resets it.
    action_failures: HashMap<TicketId, u32>,
    /// Turns that ended with the ticket not moved, per session.
    idle: HashMap<(String, TicketId), u32>,
    /// Backends that failed with `auth_required`.
    halted: BTreeSet<String>,
    /// When to test each rate-limited backend again.
    probes: HashMap<String, Probe>,
}

struct Probe {
    due: Instant,
    /// Failed probes so far; drives the backoff.
    attempt: u32,
    running: bool,
}

pub struct Dispatcher {
    hub: Arc<Hub>,
    runtime: Arc<AgentRuntime>,
    vcs: Arc<dyn VcsHost>,
    env: Arc<dyn Environment>,
    config: DispatchConfig,
    /// The hot-swappable part of the config (see [`LiveSettings`]).
    live: RwLock<LiveSettings>,
    /// The environment when Chillax built it, so a reload can hand it new commands.
    shell: Option<Arc<ShellEnvironment>>,
    book: Mutex<Book>,
    /// Merges and rebases change what `dev` is, so they take turns.
    git_lock: tokio::sync::Mutex<()>,
    wake: Notify,
    apps: Arc<AppSupervisor>,
}

impl Dispatcher {
    /// A dispatcher that runs the project's commands from `config` in the shell.
    pub fn new(
        hub: Arc<Hub>,
        runtime: Arc<AgentRuntime>,
        vcs: Arc<dyn VcsHost>,
        config: DispatchConfig,
    ) -> Arc<Self> {
        let shell = Arc::new(
            ShellEnvironment::new(
                config.test.clone(),
                config.test_timeout,
                config.qa.clone(),
                config.prod.clone(),
            )
            .with_env_files(config.env_files.clone()),
        );
        Self::build(hub, runtime, vcs, shell.clone(), Some(shell), config)
    }

    pub fn with_environment(
        hub: Arc<Hub>,
        runtime: Arc<AgentRuntime>,
        vcs: Arc<dyn VcsHost>,
        env: Arc<dyn Environment>,
        config: DispatchConfig,
    ) -> Arc<Self> {
        Self::build(hub, runtime, vcs, env, None, config)
    }

    fn build(
        hub: Arc<Hub>,
        runtime: Arc<AgentRuntime>,
        vcs: Arc<dyn VcsHost>,
        env: Arc<dyn Environment>,
        shell: Option<Arc<ShellEnvironment>>,
        config: DispatchConfig,
    ) -> Arc<Self> {
        let apps = AppSupervisor::new(
            hub.clone(),
            config.root.join(".chillax/state/env"),
            config.apps.clone(),
            AppsConfig {
                env_files: config.env_files.clone(),
                ..config.apps_config.clone()
            },
        );
        Arc::new(Self {
            hub,
            runtime,
            vcs,
            env,
            live: RwLock::new(LiveSettings::from_config(&config)),
            shell,
            config,
            book: Mutex::default(),
            git_lock: tokio::sync::Mutex::new(()),
            wake: Notify::new(),
            apps,
        })
    }

    /// The settings in force right now.
    fn live(&self) -> LiveSettings {
        self.live
            .read()
            .expect("live settings lock poisoned")
            .clone()
    }

    /// Apply a config reload: later decisions and steps use `settings`. Tickets already
    /// started keep the worktree they have (`is_shared` and `Ticket::worktree`).
    pub fn reload(&self, settings: LiveSettings) {
        if let Some(shell) = &self.shell {
            shell.set_commands(
                settings.test.clone(),
                settings.test_timeout,
                settings.qa.clone(),
                settings.prod.clone(),
            );
        }
        *self.live.write().expect("live settings lock poisoned") = settings;
        self.wake.notify_one();
    }

    /// The environment the dispatcher runs QA and release steps in.
    pub fn environment(&self) -> Arc<dyn Environment> {
        Arc::clone(&self.env)
    }

    /// The worktree mode new ticket work starts in.
    pub fn worktree_mode(&self) -> WorktreeMode {
        self.live().worktrees
    }

    /// The ticket-session cap right now.
    fn max_concurrent(&self) -> usize {
        self.config
            .max_concurrent_live
            .as_ref()
            .map_or(self.config.max_concurrent, |live| {
                live.load(Ordering::Relaxed)
            })
    }

    /// The supervisor of the dev, QA and prod apps. Call [`AppSupervisor::shutdown`] when
    /// Chillax quits.
    pub fn apps(&self) -> Arc<AppSupervisor> {
        Arc::clone(&self.apps)
    }

    /// Start the loop. It subscribes before returning, so no event is missed, and looks
    /// at the company straight away, which resumes work left over from an earlier run.
    pub fn start(self: &Arc<Self>) -> JoinHandle<()> {
        let mut rx = self.hub.subscribe();
        let dispatcher = Arc::clone(self);
        chillax_core::task::spawn_logged("dispatcher", async move {
            dispatcher.configure_apps().await;
            dispatcher.startup_sweep().await;
            let mut poll = tokio::time::interval(dispatcher.config.poll);
            loop {
                tokio::select! {
                    event = rx.recv() => match event {
                        Ok(event) => {
                            if !dispatcher.react(&event).await {
                                continue;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
                    _ = dispatcher.wake.notified() => {}
                    _ = poll.tick() => {}
                }
                // Let a burst of events land, then decide once.
                tokio::time::sleep(Duration::from_millis(30)).await;
                while let Ok(event) = rx.try_recv() {
                    dispatcher.react(&event).await;
                }
                dispatcher.tick().await;
            }
        })
    }

    /// Handle what an event means beyond the state change. False when the event cannot
    /// change what should happen next.
    async fn react(self: &Arc<Self>, event: &Event) -> bool {
        self.note_stall(event).await;
        match &event.kind {
            EventKind::AgentMessageChunk { .. }
            | EventKind::AgentToolCall { .. }
            | EventKind::AgentToolCallUpdated { .. }
            | EventKind::AgentToolCallContent { .. }
            | EventKind::AgentPlanUpdated { .. }
            | EventKind::ProgressReported { .. }
            | EventKind::CompanyChatPosted { .. } => return false,
            // The stakeholder writing to a stopped employee lifts the hold,
            // so their message is worth a tick.
            EventKind::ChatMessageSent { .. } if event.actor != Actor::Stakeholder => {
                return false;
            }
            EventKind::SprintApproved { id } => self.nudge_planner(id).await,
            EventKind::TicketDropped { id, .. } => {
                self.dropped(id).await;
                self.sweep_qa_scratch().await;
            }
            EventKind::QaRoundClosed { .. } => self.sweep_qa_scratch().await,
            EventKind::ReviewSubmitted {
                id,
                reviewer,
                verdict,
                body,
            } => {
                // The review is over either way: free the reviewer's slot. A rework
                // opens a new developer session with the feedback in its briefing.
                if *verdict == ReviewVerdict::RequestChanges {
                    let text = format!(
                        "{reviewer} reviewed {id} and requested changes:\n\n{body}\n\nAddress \
                         them in your worktree, then call request_review again."
                    );
                    self.tell_developer(id, text).await;
                }
                self.finish_session(reviewer, id).await;
                self.comment_on_pr(id, reviewer, *verdict, body).await;
            }
            EventKind::EnvRetryRequested { env, .. } if env == "prod" => {
                // A release that failed is waiting out a cooldown; a retry skips it.
                let mut book = self.book.lock().expect("book lock poisoned");
                book.cooling.remove(KEY_RELEASE);
                book.reported.remove(KEY_RELEASE);
            }
            EventKind::TestsRun {
                id,
                passed,
                skipped,
                ..
            } if *passed || *skipped => {
                // The work is handed over; the developer's context ends with it.
                if let Ok(ticket) = self.ticket(id).await
                    && let Some(developer) = &ticket.assignee
                {
                    self.finish_session(developer, id).await;
                }
            }
            // A turn the stakeholder stopped is not a stuck agent: no nudge, no backoff.
            EventKind::AgentTurnEnded {
                stop_reason,
                ticket: Some(_),
                ..
            } if stop_reason == "cancelled" => {}
            EventKind::AgentTurnEnded {
                employee,
                ticket: Some(ticket),
                ..
            } => self.turn_ended(employee, ticket),
            EventKind::AgentFailed {
                employee,
                error,
                ticket,
            } => self.agent_failed(employee, error, ticket.as_deref()).await,
            EventKind::AgentStarted { employee, .. } => self.signed_in(employee).await,
            EventKind::DispatchKicked => self.kick().await,
            // Store before the tick that follows, so a raised limit starts work at once.
            EventKind::MaxConcurrentAgentsChanged { n, .. } => {
                if let Some(live) = &self.config.max_concurrent_live {
                    live.store(*n, Ordering::Relaxed);
                }
            }
            EventKind::ProblemResolved { key, summary } => self.problem_resolved(key, summary),
            EventKind::ActionRetryRequested { key } => self.retry_action(key),
            EventKind::TicketTransitioned { id, to, .. } => {
                self.problem_solved(&format!("stuck-{id}"));
                if self.hub.engine().await.workflow.terminal_state() == to {
                    self.sweep_qa_scratch().await;
                }
            }
            EventKind::AppCrashed {
                env,
                tail,
                will_retry: false,
                exit,
                ..
            } => self.app_gave_up(env, *exit, tail).await,
            EventKind::AppStarted { env, .. } => {
                self.problem_solved(&format!("app-{env}"));
                self.clear_alerts(&app_alert_title(env)).await;
            }
            EventKind::TriageBugFiled {
                fingerprint, bug, ..
            } => self.bug_filed(fingerprint, bug).await,
            EventKind::BackendLimited { backend, until, .. } => {
                self.plan_probe(backend, *until, 0);
            }
            EventKind::BackendRecovered { backend } => self.recovered(backend).await,
            EventKind::MergeFailed { id, reason } => {
                let text = format!(
                    "Chillax could not merge {id} into {}:\n\n{reason}\n\nMerge `{}` into your \
                     branch, resolve the conflicts, commit, then call request_review again.",
                    self.config.base_branch, self.config.base_branch
                );
                self.tell_developer(id, text).await;
                self.close_reviewer(id).await;
            }
            EventKind::QaVerdictRecorded {
                id,
                employee,
                verdict,
                ..
            } if *verdict != QaVerdict::Env => {
                self.finish_session(employee, id).await;
                self.sweep_qa_scratch().await;
            }
            EventKind::DispatchPaused => {
                let closed = self.runtime.close_ticket_sessions().await;
                tracing::info!(sessions = closed.len(), "dispatch paused");
            }
            _ => {}
        }
        true
    }

    /// Decide what to do now and start doing it.
    async fn tick(self: &Arc<Self>) {
        let engine = self.hub.engine().await;
        self.start_probes(&engine);
        // Not inline in the struct: that would hold the book's lock while `busy()` takes it.
        let (halted_backends, cooling_employees) = self.book_halts();
        let checkin = self.rearm_checkin(&engine);
        let stall = self.stall_ctx(&engine).await;
        self.escalate_problems().await;
        let cooling_tickets = self.cooling_tickets();
        let ctx = Context {
            problem: self.problem_ctx(&engine),
            checkin,
            max_concurrent: self.max_concurrent(),
            sessions_open: self.runtime.ticket_session_count(),
            halted_backends,
            cooling_employees,
            ticket_sessions: self.runtime.ticket_sessions(),
            busy: self.busy(),
            app_targets: self.app_targets(&engine).await,
            app_live: self.apps.live(),
            shared_dev_worktree: self.live().worktrees == WorktreeMode::Shared,
            cooling_tickets,
            stall,
        };
        for action in next_actions(&engine, &ctx) {
            let ticket = action.ticket().to_string();
            if !self.claim(&ticket) {
                continue;
            }
            let dispatcher = Arc::clone(self);
            chillax_core::task::spawn_logged("dispatch-action", async move {
                dispatcher.run(action).await;
                dispatcher.release(&ticket);
                dispatcher.wake.notify_one();
            });
        }
    }

    /// Feed the stall watchdog: agent output keeps that agent's turn alive,
    /// board movement starts a new window and clears a stall alert.
    async fn note_stall(&self, event: &Event) {
        let agent = match &event.kind {
            EventKind::AgentMessageChunk { employee, .. }
            | EventKind::AgentToolCall { employee, .. }
            | EventKind::AgentToolCallUpdated { employee, .. }
            | EventKind::AgentToolCallContent { employee, .. }
            | EventKind::AgentPlanUpdated { employee, .. }
            | EventKind::ProgressReported { employee, .. } => Some(employee),
            _ => None,
        };
        let was_alerted = {
            let mut book = self.book.lock().expect("book lock poisoned");
            if let Some(employee) = agent {
                book.stall.activity.insert(employee.clone(), Instant::now());
            }
            is_progress(event) && book.stall.restart()
        };
        if was_alerted {
            self.clear_alerts(chillax_core::stall::ALERT_TITLE).await;
        }
    }

    /// The stall window for the decision, or `None` when the watchdog is off. While there
    /// is nothing to watch (paused, no open sprint work) the window keeps restarting, so
    /// a resume or a new sprint never counts as stalled time.
    async fn stall_ctx(&self, engine: &Engine) -> Option<StallCtx> {
        let threshold = self.live().stall;
        if threshold.is_zero() {
            return None;
        }
        let busy = self.runtime.busy_sessions();
        let (ctx, was_alerted) = {
            let mut book = self.book.lock().expect("book lock poisoned");
            let book = &mut book.stall;
            let was_alerted = !chillax_core::stall::watching(engine) && book.restart();
            let busy_sessions = busy
                .into_iter()
                .map(|(key, since_turn)| {
                    let since_output = book
                        .activity
                        .get(&key.employee)
                        .map_or(since_turn, Instant::elapsed);
                    BusySession {
                        employee: key.employee,
                        ticket: key.ticket,
                        quiet: since_turn.min(since_output),
                    }
                })
                .collect();
            let ctx = StallCtx {
                threshold,
                idle_for: book.last_progress.elapsed(),
                briefed_for: book.briefed_at.map(|t| t.elapsed()),
                alerted: book.alerted,
                busy_sessions,
            };
            (ctx, was_alerted)
        };
        if was_alerted {
            self.clear_alerts(chillax_core::stall::ALERT_TITLE).await;
        }
        Some(ctx)
    }

    /// Tickets cooling down after a failed action, for `why_idle`.
    fn cooling_tickets(&self) -> BTreeSet<TicketId> {
        let book = self.book.lock().expect("book lock poisoned");
        let now = Instant::now();
        book.cooling
            .iter()
            .filter(|(_, until)| **until > now)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Close a turn that hangs while the board is stalled. The ticket gets a
    /// comment so the board shows why its session restarted.
    async fn close_hung(&self, employee: &str, ticket: Option<&str>) -> Result<(), String> {
        let key = match ticket {
            Some(t) => SessionKey::ticket(employee, t),
            None => SessionKey::chat(employee),
        };
        if !self.runtime.close(&key).await {
            return Ok(());
        }
        let engine = self.hub.engine().await;
        let name = engine
            .state
            .actor_name(&Actor::Employee(employee.to_string()));
        let what = ticket.map_or(format!("{name} (chat)"), |t| format!("{name} on {t}"));
        tracing::warn!(%employee, ticket = ticket.unwrap_or("chat"), "closed a hung agent turn");
        self.book
            .lock()
            .expect("book lock poisoned")
            .stall
            .closed
            .push(what);
        if let Some(t) = ticket {
            let minutes = self.live().stall.as_secs().div_ceil(60);
            let _ = self
                .send(Command::CommentTicket {
                    id: t.to_string(),
                    body: format!(
                        "Chillax closed {name}'s session: the board had not moved and the agent                          showed no activity for {minutes}+ minutes. It restarts with a fresh                          briefing."
                    ),
                    external_id: None,
                    attachments: Vec::new(),
                })
                .await;
        }
        Ok(())
    }

    /// Hand a stalled board to the project manager. A briefing that cannot be
    /// delivered is logged; the window stays open, so the next tick tries again.
    async fn unstick(&self, pm: &str) -> Result<(), String> {
        let engine = self.hub.engine().await;
        let ctx = self.reason_ctx(&engine).await;
        let (idle_for, closed) = {
            let book = self.book.lock().expect("book lock poisoned");
            (
                book.stall.last_progress.elapsed(),
                book.stall.closed.clone(),
            )
        };
        let text = chillax_core::stall::briefing(&engine, &ctx, idle_for, &closed);
        match self
            .send(Command::SendChat {
                to: pm.to_string(),
                text,
            })
            .await
        {
            Ok(()) => {
                self.book
                    .lock()
                    .expect("book lock poisoned")
                    .stall
                    .briefed_at = Some(Instant::now());
            }
            Err(error) => tracing::warn!(%pm, %error, "could not send the stall briefing"),
        }
        Ok(())
    }

    /// The stall outlasted the project manager's briefing: tell the stakeholder, once.
    async fn stall_alert(&self) -> Result<(), String> {
        let engine = self.hub.engine().await;
        let ctx = self.reason_ctx(&engine).await;
        let idle_for = self
            .book
            .lock()
            .expect("book lock poisoned")
            .stall
            .last_progress
            .elapsed();
        self.send(Command::RaiseAlert {
            title: chillax_core::stall::ALERT_TITLE.to_string(),
            detail: chillax_core::stall::alert_detail(&engine, &ctx, idle_for),
        })
        .await?;
        self.book.lock().expect("book lock poisoned").stall.alerted = true;
        Ok(())
    }

    /// What `why_idle` needs to know about the runtime right now.
    async fn reason_ctx(&self, engine: &Engine) -> Context {
        let (halted_backends, cooling_employees) = self.book_halts();
        Context {
            max_concurrent: self.max_concurrent(),
            sessions_open: self.runtime.ticket_session_count(),
            halted_backends,
            cooling_employees,
            ticket_sessions: self.runtime.ticket_sessions(),
            busy: self.busy(),
            cooling_tickets: self.cooling_tickets(),
            shared_dev_worktree: self.live().worktrees == WorktreeMode::Shared,
            stall: self.stall_ctx(engine).await,
            ..Context::default()
        }
    }

    /// The check-in memory for the decision. A due interval that would send nothing (no
    /// open work, or an unchanged board not yet quiet long enough) is closed here, so the
    /// 5 s poll does not look at it again until the next interval.
    fn rearm_checkin(&self, engine: &Engine) -> Option<CheckinCtx> {
        if self.live().checkin.is_zero() {
            return None;
        }
        let mut book = self.book.lock().expect("book lock poisoned");
        let book = &mut book.checkin;
        let mut ctx = CheckinCtx {
            due: book.last_sent.elapsed() >= self.live().checkin,
            last_fingerprint: book.last_fp.clone(),
            unchanged_intervals: book.unchanged_intervals,
        };
        if ctx.due && !checkin_wanted(engine, &ctx) {
            book.note_tickets(engine);
            book.last_sent = Instant::now();
            book.unchanged_intervals = if chillax_core::checkin::has_open_work(engine) {
                book.unchanged_intervals + 1
            } else {
                0
            };
            ctx.due = false;
        }
        Some(ctx)
    }

    /// Send the project manager the board digest. A chat that cannot be
    /// delivered is logged and the interval closed; it is no reason to alert the
    /// stakeholder, the next interval tries again.
    async fn checkin(&self, pm: &str) -> Result<(), String> {
        let engine = self.hub.engine().await;
        let stalled = {
            let mut book = self.book.lock().expect("book lock poisoned");
            book.checkin.note_tickets(&engine);
            book.checkin.stalled()
        };
        let digest = chillax_core::checkin::digest(&engine, &stalled, Some(self.max_concurrent()));
        let fingerprint = chillax_core::checkin::fingerprint(&engine.state);
        let sent = match digest {
            Some(digest) => {
                let text = chillax_core::checkin::briefing(&digest);
                Some(
                    self.send(Command::SendChat {
                        to: pm.to_string(),
                        text,
                    })
                    .await,
                )
            }
            None => None,
        };
        let mut book = self.book.lock().expect("book lock poisoned");
        let book = &mut book.checkin;
        book.last_sent = Instant::now();
        match sent {
            Some(Ok(())) => {
                book.last_fp = Some(fingerprint);
                // Whatever the reason it went out, the next quiet stretch starts now.
                book.unchanged_intervals = 0;
            }
            Some(Err(error)) => tracing::warn!(%pm, %error, "could not send the board check-in"),
            None => {}
        }
        Ok(())
    }

    async fn run(&self, action: Action) {
        let ticket = action.ticket().to_string();
        // Round-wide and release actions belong to no ticket; they report to the inbox.
        let general = ticket.starts_with('@');
        match self.perform(&action).await {
            Ok(()) => {
                self.book
                    .lock()
                    .expect("book lock poisoned")
                    .reported
                    .remove(&ticket);
                self.book
                    .lock()
                    .expect("book lock poisoned")
                    .action_failures
                    .remove(&ticket);
                self.problem_solved(&ticket);
                if general {
                    self.clear_alerts(&alert_title(&action)).await;
                }
            }
            Err(error) => {
                tracing::warn!(%ticket, %error, "dev loop action failed");
                let first = {
                    let mut book = self.book.lock().expect("book lock poisoned");
                    // The same action failing again and again waits longer each time.
                    let n = book.action_failures.entry(ticket.clone()).or_insert(0);
                    *n += 1;
                    let wait = limits::backoff(*n, self.config.cooldown, self.config.backoff_cap);
                    book.cooling.insert(ticket.clone(), Instant::now() + wait);
                    book.reported.insert(ticket.clone())
                };
                // Say it once on the ticket; the cooldown stops the retry storm.
                if first && general {
                    // The project manager may be able to fix it (ADR 0032); sign-in and
                    // limit errors, releases and deploys alert at once.
                    if problems::deferrable_key(&ticket) && !problems::needs_a_person(&error) {
                        self.alert_or_defer(
                            &ticket,
                            ProblemKind::ActionFailed,
                            describe(&action),
                            None,
                            alert_title(&action),
                            error.clone(),
                        )
                        .await;
                    } else {
                        let _ = self
                            .send(Command::RaiseAlert {
                                title: alert_title(&action),
                                detail: error.clone(),
                            })
                            .await;
                    }
                } else if first {
                    let body = format!("Chillax could not {}: {error}", describe(&action));
                    let _ = self
                        .send(Command::CommentTicket {
                            id: ticket,
                            body,
                            external_id: None,
                            attachments: Vec::new(),
                        })
                        .await;
                }
            }
        }
    }

    async fn perform(&self, action: &Action) -> Result<(), String> {
        match action {
            Action::StartWork { ticket, .. } => self.start_work(ticket).await,
            Action::Reassign { ticket, to } => {
                self.send(Command::AssignTicket {
                    id: ticket.clone(),
                    assignee: Some(to.clone()),
                })
                .await
            }
            Action::RaiseAlert { title, detail, .. } => {
                self.send(Command::RaiseAlert {
                    title: title.clone(),
                    detail: detail.clone(),
                })
                .await
            }
            Action::OpenSession {
                ticket,
                employee,
                kind,
            } => self.open_session(ticket, employee, *kind).await,
            Action::RunTests { ticket } => self.run_tests(ticket).await,
            Action::StartReview { ticket, reviewer } => {
                self.send(Command::StartReview {
                    id: ticket.clone(),
                    reviewer: reviewer.clone(),
                })
                .await
            }
            Action::Merge { ticket } => self.merge(ticket).await,
            Action::Rebase { ticket } => self.rebase(ticket).await,
            Action::ResolveConflict { ticket, employee } => {
                self.resolve_conflict(ticket, employee).await
            }
            Action::Promote => self.promote().await,
            Action::RunEnv { step } => self.run_env(*step).await,
            Action::CloseRound => self.close_round().await,
            Action::Release { id } => self.make_release(id).await,
            Action::Deploy { id } => self.deploy(id).await,
            Action::TriageFailure {
                key,
                fingerprint,
                pm,
            } => self.triage_failure(key, fingerprint, pm.as_deref()).await,
            Action::PlanSprint { pm } => self.plan_sprint(pm).await,
            Action::PmCheckin { pm } => self.checkin(pm).await,
            Action::PmHandleProblem { pm } => self.handle_problems(pm).await,
            Action::CloseHungSession {
                employee, ticket, ..
            } => self.close_hung(employee, ticket.as_deref()).await,
            Action::PmUnstick { pm } => self.unstick(pm).await,
            Action::StallAlert => self.stall_alert().await,
            Action::TriageRequest { key, id, pm } => {
                self.triage_request(key, id, pm.as_deref()).await
            }
            Action::RunApp { env, commit, why } => self.run_app(env, commit, why).await,
            Action::StopApp { env, reason } => {
                self.apps.stop(env).await;
                self.send(Command::RecordAppStopped {
                    env: env.clone(),
                    reason: reason.clone(),
                })
                .await
            }
        }
    }

    /// Tell the hub which apps `chillax.toml` asks for (none removes the old ones).
    async fn configure_apps(&self) {
        let apps = self
            .config
            .apps
            .iter()
            .map(|s| chillax_core::AppInfo {
                env: s.env.clone(),
                port: s.port,
                url: s.url.clone(),
            })
            .collect();
        if let Err(error) = self.send(Command::ConfigureApps { apps }).await {
            tracing::warn!(%error, "could not record the configured apps");
        }
    }

    /// What each configured app should run now, from git (ADR 0015). Apps whose answer
    /// cannot be read this time are left out, so a git hiccup never stops one.
    async fn app_targets(&self, engine: &Engine) -> BTreeMap<String, AppTarget> {
        let mut targets = BTreeMap::new();
        for spec in &self.config.apps {
            if engine.state.app(&spec.env).is_none() {
                continue;
            }
            let target = match spec.env.as_str() {
                "dev" => self.branch_target(&self.config.base_branch).await,
                "qa" => self.branch_target(&self.config.qa.branch).await,
                "prod" => self.release_target(&engine.state).await,
                _ => None,
            };
            if let Some(target) = target {
                targets.insert(spec.env.clone(), target);
            }
        }
        targets
    }

    async fn branch_target(&self, branch: &str) -> Option<AppTarget> {
        match self.vcs.head(branch).await {
            Ok(commit) => Some(AppTarget::Commit(commit)),
            Err(chillax_core::VcsError::Git { .. }) => {
                Some(AppTarget::Idle(format!("no `{branch}` branch yet")))
            }
            Err(error) => {
                tracing::debug!(%error, branch, "could not read a branch head for an app");
                None
            }
        }
    }

    /// Prod runs the latest release tag.
    async fn release_target(&self, state: &chillax_core::engine::State) -> Option<AppTarget> {
        let Some(tag) = state
            .releases
            .iter()
            .rev()
            .find(|r| {
                matches!(
                    r.status,
                    ReleaseStatus::Tagged | ReleaseStatus::Deployed | ReleaseStatus::DeployFailed
                )
            })
            .and_then(|r| r.tag.clone())
        else {
            return Some(AppTarget::Idle("no release yet".into()));
        };
        match self.vcs.tag_commit(&tag).await {
            Ok(Some(commit)) => Some(AppTarget::Commit(commit)),
            Ok(None) => Some(AppTarget::Idle(format!("tag {tag} is missing"))),
            Err(error) => {
                tracing::debug!(%error, %tag, "could not read a release tag for an app");
                None
            }
        }
    }

    /// Start or restart an app in the checkout that holds its code.
    async fn run_app(&self, env: &str, commit: &str, why: &str) -> Result<(), String> {
        let dir = match env {
            "prod" => self.vcs.detached_checkout("prod", commit).await,
            "qa" => self.vcs.checkout(&self.config.qa.branch).await,
            _ => self.vcs.checkout(&self.config.base_branch).await,
        }
        .map_err(|e| e.to_string())?;
        self.apps.start(env, dir, commit.to_string(), why).await
    }

    /// An app crashed too often in a row: tell the stakeholder how to try again.
    async fn app_gave_up(&self, env: &str, exit: Option<i32>, tail: &str) {
        let how = match exit {
            Some(code) => format!("exited with {code}"),
            None => "could not start".to_string(),
        };
        let detail = format!(
            "The {env} app {how} and Chillax stopped retrying. Fix the cause, then restart it \
             from the Environments view (or `chillax env --restart {env}`); new code on the \
             branch also starts it again. The log is .chillax/state/env/{env}.log.\n\n{tail}"
        );
        self.alert_or_defer(
            &format!("app-{env}"),
            ProblemKind::AppCrashing,
            &format!("{env} app"),
            None,
            app_alert_title(env),
            detail,
        )
        .await;
    }

    /// Brief the project manager to propose the next sprint.
    async fn plan_sprint(&self, pm: &str) -> Result<(), String> {
        let engine = self.hub.engine().await;
        let Some(request) = engine.state.planning.clone().filter(|p| !p.briefed) else {
            return Ok(());
        };
        let text = chillax_core::planning::briefing(&engine, request.note.as_deref());
        self.send(Command::SendChat {
            to: pm.to_string(),
            text,
        })
        .await?;
        self.send(Command::RecordPlanningBriefed).await
    }

    /// Brief the project manager to file and assign a stakeholder request, and
    /// look again after `triage_retry` if they have not said they are done. After too
    /// many briefings, tell the stakeholder.
    async fn triage_request(&self, key: &str, id: &str, pm: Option<&str>) -> Result<(), String> {
        let engine = self.hub.engine().await;
        let Some(request) = engine
            .state
            .request(id)
            .filter(|r| r.status.is_open())
            .cloned()
        else {
            return Ok(());
        };
        let Some(pm) = pm else {
            self.send(Command::AbandonRequest { id: id.to_string() })
                .await?;
            let filed = if request.tickets.is_empty() {
                "nothing".to_string()
            } else {
                request.tickets.join(" ")
            };
            let _ = self
                .send(Command::RaiseAlert {
                    title: format!("Request {id} was not handled"),
                    detail: format!(
                        "The project manager was briefed {} times and did not finish it. Filed \
                         so far: {filed}. File and assign the rest yourself.\n\n{}",
                        chillax_core::intake::MAX_ATTEMPTS,
                        request.text
                    ),
                })
                .await;
            return Ok(());
        };
        let text = chillax_core::intake::briefing(&engine, &request);
        self.book
            .lock()
            .expect("book lock poisoned")
            .cooling
            .insert(key.to_string(), Instant::now() + self.config.triage_retry);
        self.send(Command::SendChat {
            to: pm.to_string(),
            text,
        })
        .await?;
        self.send(Command::RecordRequestBriefed { id: id.to_string() })
            .await
    }

    /// The stakeholder dropped a ticket: end every session on it and free its worktree.
    /// Uncommitted work is committed to the branch first; the branch itself stays.
    async fn dropped(&self, id: &str) {
        for (employee, _) in self
            .runtime
            .ticket_sessions()
            .into_iter()
            .filter(|(_, ticket)| ticket == id)
        {
            self.finish_session(&employee, id).await;
        }
        let Ok(ticket) = self.ticket(id).await else {
            return;
        };
        if ticket.worktree.is_none() {
            return;
        }
        let path = self.worktree(&ticket);
        if self.is_shared(&ticket) {
            // The shared worktree stays; the work goes to the ticket's branch and the
            // worktree returns to `dev`.
            if path.join(".git").exists()
                && let Some(branch) = &ticket.branch
            {
                let saved = async {
                    self.vcs
                        .switch_branch(&path, branch, Some(&self.config.base_branch))
                        .await?;
                    self.vcs
                        .commit_all(&path, &format!("{id}: work in progress when dropped"))
                        .await?;
                    self.park_dev_worktree(&path).await
                };
                if let Err(error) = saved.await {
                    tracing::warn!(ticket = id, %error, "could not save the dropped ticket's work");
                }
            }
            return;
        }
        if path.exists()
            && let Err(error) = self
                .vcs
                .commit_all(&path, &format!("{id}: work in progress when dropped"))
                .await
        {
            tracing::warn!(ticket = id, %error, "could not save the dropped ticket's work");
        }
        if let Err(error) = self.vcs.remove_worktree(&path).await {
            tracing::warn!(ticket = id, %error, "could not remove the dropped ticket's worktree");
        }
    }

    /// Brief the project manager about a failure, and look again after
    /// `triage_retry` if no bug came of it. After too many briefings, tell the stakeholder.
    async fn triage_failure(
        &self,
        key: &str,
        fingerprint: &str,
        pm: Option<&str>,
    ) -> Result<(), String> {
        let engine = self.hub.engine().await;
        let Some(failure) = engine
            .state
            .failures
            .iter()
            .find(|f| f.fingerprint == fingerprint)
            .filter(|f| f.status == chillax_core::TriageStatus::Pending)
            .cloned()
        else {
            return Ok(());
        };
        let Some(pm) = pm else {
            self.send(Command::AbandonTriage {
                fingerprint: fingerprint.to_string(),
            })
            .await?;
            let _ = self
                .send(Command::RaiseAlert {
                    title: format!("No bug was filed for a {}", failure.class.label()),
                    detail: format!(
                        "{} on {}. The project manager was briefed {} times and filed nothing. \
                         File the bug yourself.\n\n{}",
                        failure.class.label(),
                        failure.subject,
                        chillax_core::triage::MAX_ATTEMPTS,
                        failure.output
                    ),
                })
                .await;
            return Ok(());
        };
        let owner = chillax_core::triage::bug_owner(&engine);
        let text = chillax_core::triage::briefing(
            &engine,
            &failure,
            owner.as_deref(),
            &self.config.base_branch,
        );
        // The wait starts now, so a slow project manager is not briefed again at once.
        self.book
            .lock()
            .expect("book lock poisoned")
            .cooling
            .insert(key.to_string(), Instant::now() + self.config.triage_retry);
        self.send(Command::SendChat {
            to: pm.to_string(),
            text,
        })
        .await?;
        self.send(Command::RecordTriageBriefed {
            fingerprint: fingerprint.to_string(),
        })
        .await
    }

    async fn start_work(&self, id: &str) -> Result<(), String> {
        let ticket = self.ticket(id).await?;
        let prefix = if ticket.kind == TicketKind::Bug {
            "fix"
        } else {
            "feat"
        };
        let branch = format!("{prefix}/{}-{}", ticket.id, short_slug(&ticket.title));
        let shared = self.live().worktrees == WorktreeMode::Shared;
        let worktree = if shared {
            DEV_WORKTREE.to_string()
        } else {
            format!(".chillax/worktrees/{}", ticket.id)
        };
        let path = self.config.root.join(&worktree);
        if shared && path.join(".git").exists() {
            if !self.vcs.is_clean(&path).await.map_err(|e| e.to_string())? {
                return Err(format!(
                    "the dev worktree {} has uncommitted changes; commit or discard them so \
                     {} can start",
                    path.display(),
                    ticket.id
                ));
            }
            self.vcs
                .switch_branch(&path, &branch, Some(&self.config.base_branch))
                .await
                .map_err(|e| e.to_string())?;
        } else {
            self.vcs
                .create_worktree(&branch, &path)
                .await
                .map_err(|e| e.to_string())?;
        }
        self.send(Command::StartWork {
            id: ticket.id,
            branch,
            worktree,
        })
        .await
    }

    async fn open_session(
        &self,
        id: &str,
        employee: &str,
        kind: SessionKind,
    ) -> Result<(), String> {
        let ticket = self.ticket(id).await?;
        if kind == SessionKind::Qa {
            return self.open_qa_session(&ticket, employee).await;
        }
        let branch = ticket
            .branch
            .clone()
            .ok_or_else(|| format!("{id} has no branch"))?;
        let path = self.enter_worktree(&ticket).await?;
        let base = &self.config.base_branch;
        let briefing = match kind {
            SessionKind::Work => match &ticket.conflict {
                Some(conflict) => briefing::resolve_conflict(&ticket, base, &conflict.files),
                None => briefing::work(&ticket, base),
            },
            SessionKind::Review => {
                let diff = self
                    .vcs
                    .diff(&branch, DIFF_BYTES)
                    .await
                    .map_err(|e| e.to_string())?;
                briefing::review(&ticket, base, &diff)
            }
            SessionKind::Qa => unreachable!("QA sessions open in open_qa_session"),
        };
        self.close_stale_sessions(employee, &ticket.id).await;
        self.runtime
            .open_ticket_session(TicketSession {
                employee: employee.to_string(),
                ticket: ticket.id,
                kind,
                cwd: path,
                scratch: None,
                briefing,
            })
            .await
    }

    async fn run_tests(&self, id: &str) -> Result<(), String> {
        let ticket = self.ticket(id).await?;
        let path = self.enter_worktree(&ticket).await?;
        let base = self.config.base_branch.clone();
        let conflict = ticket.conflict.clone();
        let mut base_merged = true;
        if let Some(conflict) = &conflict {
            // Never commit a half-done merge or conflict markers.
            let status = self
                .vcs
                .conflict_status(&path, &conflict.files)
                .await
                .map_err(|e| e.to_string())?;
            if !status.clean() {
                let files = if status.markers.is_empty() {
                    conflict.files.clone()
                } else {
                    status.markers.clone()
                };
                let note = if status.merging {
                    format!("the merge of {base} is still in progress")
                } else {
                    format!("conflict markers are left in {}", status.markers.join(", "))
                };
                return self.unresolved(&ticket, files, note).await;
            }
            base_merged = status.base_merged;
        }
        self.vcs
            .commit_all(&path, &format!("{}: {}", ticket.id, ticket.title))
            .await
            .map_err(|e| e.to_string())?;
        if !base_merged {
            // The developer merged an older `dev`, or none: whatever is left must merge
            // cleanly now, or the conflict is still theirs.
            let branch = ticket.branch.as_deref().unwrap_or_default();
            let synced = self
                .vcs
                .sync_with_base(&path, &format!("Merge {base} into {branch}"))
                .await
                .map_err(|e| e.to_string())?;
            match synced {
                SyncResult::UpToDate | SyncResult::Merged { .. } => {}
                SyncResult::Conflict { files, detail } => {
                    return self.unresolved(&ticket, files, detail).await;
                }
                SyncResult::Dirty => {
                    return Err(format!(
                        "the worktree of {} has uncommitted changes Chillax could not commit",
                        ticket.id
                    ));
                }
            }
        }
        let (passed, skipped, tail) = self.run_test_command(&path).await?;
        if (passed || skipped) && self.vcs.uses_pull_requests() {
            // Before the hand-over is recorded, so a failure here is simply retried.
            self.open_pull_request(&ticket).await?;
        }
        self.send(Command::RecordTests {
            id: ticket.id.clone(),
            passed,
            skipped,
            output_tail: tail.clone(),
        })
        .await?;
        if !passed {
            self.tell_tests_failed(&ticket, &tail);
        }
        Ok(())
    }

    /// The developer's turn ended with the conflict still there: count the attempt.
    async fn unresolved(
        &self,
        ticket: &Ticket,
        files: Vec<String>,
        note: String,
    ) -> Result<(), String> {
        self.send(Command::RecordSync {
            id: ticket.id.clone(),
            outcome: SyncOutcome::Unresolved,
            files,
            note,
        })
        .await
    }

    /// Run the project's dev test command in `path`: (passed, skipped, output tail).
    async fn run_test_command(
        &self,
        path: &std::path::Path,
    ) -> Result<(bool, bool, String), String> {
        let run = self
            .env
            .run("dev", EnvStep::Test, path)
            .await
            .map_err(|e| e.to_string())?;
        Ok((run.ok, run.skipped, run.output_tail))
    }

    /// Tell the developer's session the tests failed on their branch.
    fn tell_tests_failed(&self, ticket: &Ticket, tail: &str) {
        if let Some(developer) = &ticket.assignee {
            let command = self.live().test.unwrap_or_default();
            self.runtime.send_to(
                &SessionKey::ticket(developer, &ticket.id),
                format!(
                    "Chillax ran `{command}` on your branch and it failed:\n\n{tail}\n\nFix it, \
                     then call request_review again."
                ),
            );
        }
    }

    /// Give the assignee their turn to resolve the ticket's conflict with the base branch:
    /// into their running session, or a new one briefed for it.
    async fn resolve_conflict(&self, id: &str, employee: &str) -> Result<(), String> {
        let ticket = self.ticket(id).await?;
        let Some(conflict) = ticket.conflict.clone().filter(|c| !c.briefed) else {
            return Ok(());
        };
        let path = self.enter_worktree(&ticket).await?;
        let briefing =
            briefing::resolve_conflict(&ticket, &self.config.base_branch, &conflict.files);
        let key = SessionKey::ticket(employee, &ticket.id);
        if !self.runtime.send_to(&key, briefing.clone()) {
            self.close_stale_sessions(employee, &ticket.id).await;
            self.runtime
                .open_ticket_session(TicketSession {
                    employee: employee.to_string(),
                    ticket: ticket.id.clone(),
                    kind: SessionKind::Work,
                    cwd: path,
                    scratch: None,
                    briefing,
                })
                .await?;
        }
        self.send(Command::RecordConflictBriefed { id: ticket.id })
            .await
    }

    async fn merge(&self, id: &str) -> Result<(), String> {
        let _turn = self.git_lock.lock().await;
        let ticket = self.ticket(id).await?;
        let waiting = ticket.state == IN_REVIEW
            && ticket.checks.contains("review_approved")
            && !ticket.checks.contains("merged_to_dev");
        if !waiting {
            return Ok(());
        }
        let branch = ticket
            .branch
            .clone()
            .ok_or_else(|| format!("{id} has no branch"))?;
        let message = format!(
            "Merge {}: {}\n\nBranch: {branch}\nReviewer: {}",
            ticket.id,
            ticket.title,
            ticket.reviewer.as_deref().unwrap_or("unknown")
        );
        // Bring `dev` into the branch first and test the result, so what merges is what was
        // tested; a conflict goes back to the developer while the approval stands.
        let path = self.enter_worktree(&ticket).await?;
        self.vcs
            .commit_all(&path, &format!("{}: {}", ticket.id, ticket.title))
            .await
            .map_err(|e| e.to_string())?;
        let base = self.config.base_branch.clone();
        let synced = self
            .vcs
            .sync_with_base(&path, &format!("Merge {base} into {branch}"))
            .await
            .map_err(|e| e.to_string())?;
        match synced {
            SyncResult::UpToDate => {}
            SyncResult::Merged { .. } => {
                let (passed, skipped, tail) = self.run_test_command(&path).await?;
                if !passed && !skipped {
                    self.send(Command::RecordSync {
                        id: ticket.id.clone(),
                        outcome: SyncOutcome::TestsFailed,
                        files: Vec::new(),
                        note: tail.clone(),
                    })
                    .await?;
                    self.tell_tests_failed(&ticket, &tail);
                    return Ok(());
                }
                if self.vcs.uses_pull_requests() {
                    // The pull request merges what was just tested.
                    self.vcs
                        .push(&branch, true)
                        .await
                        .map_err(|e| e.to_string())?;
                }
                self.send(Command::RecordSync {
                    id: ticket.id.clone(),
                    outcome: SyncOutcome::Merged,
                    files: Vec::new(),
                    note: String::new(),
                })
                .await?;
            }
            SyncResult::Conflict { files, detail } => {
                return self
                    .send(Command::RecordSync {
                        id: ticket.id.clone(),
                        outcome: SyncOutcome::Conflict,
                        files,
                        note: detail,
                    })
                    .await;
            }
            SyncResult::Dirty => {
                return Err(format!(
                    "the worktree of {} has uncommitted changes Chillax could not commit",
                    ticket.id
                ));
            }
        }
        if self.is_shared(&ticket) {
            // Merges happen on `dev` in the dev worktree, which the ticket branch held.
            self.park_dev_worktree(&path)
                .await
                .map_err(|e| e.to_string())?;
        }
        let merged = if self.vcs.uses_pull_requests() {
            self.merge_pull_request(&ticket, &message).await
        } else {
            self.vcs.merge(&branch, &message).await
        };
        match merged {
            Ok(MergeOutcome::Merged { commit }) => {
                self.send(Command::RecordMerge {
                    id: ticket.id.clone(),
                    commit,
                })
                .await?;
                self.wrap_up(&ticket).await;
                self.sync_local_base().await;
                Ok(())
            }
            // `dev` moved under us (a remote merge raced this one): resolve it like any
            // other conflict.
            Ok(MergeOutcome::Conflict { files, detail }) => {
                self.send(Command::RecordSync {
                    id: ticket.id,
                    outcome: SyncOutcome::Conflict,
                    files,
                    note: detail,
                })
                .await
            }
            Err(error) => Err(error.to_string()),
        }
    }

    /// Push the ticket's branch and make sure a pull request into the base branch exists.
    /// Each hand-over (a rework too) pushes the new commits; the pull request is opened once.
    async fn open_pull_request(&self, ticket: &Ticket) -> Result<(), String> {
        let branch = ticket
            .branch
            .clone()
            .ok_or_else(|| format!("{} has no branch", ticket.id))?;
        let err = |e: chillax_core::vcs::VcsError| e.to_string();
        self.vcs.push(&branch, true).await.map_err(err)?;
        if ticket.pr_number.is_some() {
            return Ok(());
        }
        let mut body = format!("{}\n", ticket.description.trim());
        if !ticket.acceptance_criteria.is_empty() {
            body.push_str("\n## Acceptance criteria\n");
            for criterion in &ticket.acceptance_criteria {
                body.push_str(&format!("- [ ] {criterion}\n"));
            }
        }
        body.push_str(&format!("\nOpened by Chillax for ticket {}.", ticket.id));
        let title = format!("{}: {}", ticket.id, ticket.title);
        let opened = self
            .vcs
            .open_pr(&branch, &self.config.base_branch, &title, &body)
            .await
            .map_err(err)?;
        if let Some(pr) = opened {
            self.send(Command::RecordPr {
                id: ticket.id.clone(),
                number: pr.number,
                url: pr.url,
            })
            .await?;
        }
        Ok(())
    }

    /// Merge the ticket's pull request. A branch that never got one had nothing to merge
    /// (the base has it already), so the base's head stands for the merge.
    async fn merge_pull_request(
        &self,
        ticket: &Ticket,
        message: &str,
    ) -> Result<MergeOutcome, chillax_core::vcs::VcsError> {
        let base = &self.config.base_branch;
        match ticket.pr_number {
            Some(number) => self.vcs.merge_pr(number, base, message).await,
            None => {
                let commit = match self.vcs.remote_head(base).await? {
                    Some(commit) => commit,
                    None => self.vcs.head(base).await?,
                };
                Ok(MergeOutcome::Merged { commit })
            }
        }
    }

    /// After the remote merged something, bring the local base branch along. When a
    /// checkout of it has uncommitted changes it stays behind; say so instead of failing.
    async fn sync_local_base(&self) {
        self.sync_local_branch(&self.config.base_branch).await;
    }

    /// The reviewer's verdict goes on the pull request too. Chillax keeps the
    /// `review_approved` fact itself: GitHub does not let an account approve its own PR.
    async fn comment_on_pr(&self, id: &str, reviewer: &str, verdict: ReviewVerdict, body: &str) {
        let Ok(ticket) = self.ticket(id).await else {
            return;
        };
        let Some(number) = ticket.pr_number else {
            return;
        };
        let name = self
            .hub
            .snapshot()
            .await
            .employee(reviewer)
            .map_or_else(|| reviewer.to_string(), |e| e.employee.name.clone());
        let headline = match verdict {
            ReviewVerdict::Approve => "approved",
            ReviewVerdict::RequestChanges => "requested changes",
        };
        let text = format!("**{name}** {headline}\n\n{body}");
        if let Err(error) = self.vcs.comment_pr(number, &text).await {
            tracing::warn!(ticket = id, %error, "could not comment on the pull request");
        }
    }

    /// The branch is in `dev`: close the sessions and free the worktree (in shared mode,
    /// return it to `dev`). The branch stays.
    async fn wrap_up(&self, ticket: &Ticket) {
        for employee in ticket.assignee.iter().chain(ticket.reviewer.iter()) {
            self.finish_session(employee, &ticket.id).await;
        }
        if self.is_shared(ticket) {
            // The dev worktree stays and goes back to `dev` for the next ticket.
            if let Err(error) = self.park_dev_worktree(&self.worktree(ticket)).await {
                tracing::warn!(ticket = %ticket.id, %error, "could not return the dev worktree to dev");
            }
        } else if let Err(error) = self.vcs.remove_worktree(&self.worktree(ticket)).await {
            tracing::warn!(ticket = %ticket.id, %error, "could not remove worktree");
        }
    }

    async fn rebase(&self, id: &str) -> Result<(), String> {
        let _turn = self.git_lock.lock().await;
        let ticket = self.ticket(id).await?;
        if ticket.state != IN_PROGRESS || !ticket.needs_rebase {
            return Ok(());
        }
        let path = self.enter_worktree(&ticket).await?;
        match self.vcs.rebase(&path).await {
            Ok(RebaseResult::Rebased) => {
                // The pull request follows the rewritten branch.
                if ticket.pr_number.is_some()
                    && let Some(branch) = &ticket.branch
                {
                    self.vcs
                        .push(branch, true)
                        .await
                        .map_err(|e| e.to_string())?;
                }
                self.send(Command::RecordRebase {
                    id: ticket.id,
                    outcome: RebaseOutcome::Rebased,
                    note: String::new(),
                })
                .await
            }
            Ok(RebaseResult::Conflict { files, detail }) => {
                self.send(Command::RecordRebase {
                    id: ticket.id.clone(),
                    outcome: RebaseOutcome::Conflict,
                    note: detail.clone(),
                })
                .await?;
                // The developer gets a resolve turn for it.
                self.send(Command::RecordSync {
                    id: ticket.id,
                    outcome: SyncOutcome::Conflict,
                    files,
                    note: detail,
                })
                .await
            }
            // Uncommitted changes: leave the tree alone and look again later.
            Ok(RebaseResult::Dirty) => {
                self.cool(&ticket.id);
                Ok(())
            }
            Err(error) => Err(error.to_string()),
        }
    }

    /// Advance `qa` to `dev` and start a round over what is waiting in `in_qa`.
    async fn promote(&self) -> Result<(), String> {
        let _turn = self.git_lock.lock().await;
        let state = self.hub.snapshot().await;
        if state.open_round().is_some() || state.waiting_for_qa().is_empty() {
            return Ok(());
        }
        let moved = self
            .vcs
            .fast_forward(&self.config.qa.branch, &self.config.base_branch)
            .await
            .map_err(|e| e.to_string())?;
        match moved {
            FastForward::Advanced { commit } | FastForward::UpToDate { commit } => {
                self.vcs
                    .push(&self.config.qa.branch, false)
                    .await
                    .map_err(|e| e.to_string())?;
                self.send(Command::RecordPromotion { commit }).await
            }
            FastForward::Diverged { detail } => Err(detail),
        }
    }

    /// Run `up` or `test` of the QA environment in the shared `qa` tree.
    async fn run_env(&self, step: EnvStep) -> Result<(), String> {
        let state = self.hub.snapshot().await;
        if state.open_round().is_none() {
            return Ok(());
        }
        let tree = self
            .vcs
            .checkout(&self.config.qa.branch)
            .await
            .map_err(|e| e.to_string())?;
        let run = self
            .env
            .run("qa", step, &tree)
            .await
            .map_err(|e| e.to_string())?;
        self.send(Command::RecordEnvRun {
            step,
            ok: run.ok,
            skipped: run.skipped,
            output_tail: run.output_tail,
        })
        .await
    }

    /// Starting `employee` on `ticket`: end their sessions for other tickets that no longer
    /// wait on them, so context never carries across tickets (ADR 0008).
    async fn close_stale_sessions(&self, employee: &str, ticket: &str) {
        let engine = self.hub.engine().await;
        self.runtime
            .close_other_ticket_sessions(employee, ticket, |t| {
                session_is_waiting(&engine, employee, t)
            })
            .await;
    }

    /// A tester starts on a ticket of the round, in the shared `qa` tree.
    async fn open_qa_session(&self, ticket: &Ticket, employee: &str) -> Result<(), String> {
        let state = self.hub.snapshot().await;
        let round = state
            .open_round()
            .filter(|r| r.tickets.contains(&ticket.id))
            .cloned()
            .ok_or_else(|| format!("{} is not under test", ticket.id))?;
        let tree = self
            .vcs
            .checkout(&self.config.qa.branch)
            .await
            .map_err(|e| e.to_string())?;
        let scratch = self.config.root.join(QA_SCRATCH).join(&ticket.id);
        tokio::fs::create_dir_all(&scratch)
            .await
            .map_err(|e| format!("could not create {}: {e}", scratch.display()))?;
        let bugs: Vec<&Ticket> = state
            .tickets
            .iter()
            .filter(|t| t.found_in.as_deref() == Some(ticket.id.as_str()))
            .collect();
        let briefing = briefing::qa(ticket, &round, &tree, &scratch, &bugs);
        self.close_stale_sessions(employee, &ticket.id).await;
        self.runtime
            .open_ticket_session(TicketSession {
                employee: employee.to_string(),
                ticket: ticket.id.clone(),
                kind: SessionKind::Qa,
                cwd: tree,
                scratch: Some(scratch),
                briefing,
            })
            .await
    }

    /// Every ticket has a verdict: tear the environment down, then close the round,
    /// which proposes a release when nothing failed.
    async fn close_round(&self) -> Result<(), String> {
        let state = self.hub.snapshot().await;
        let Some(round) = state
            .open_round()
            .filter(|r| r.status == chillax_core::QaRoundStatus::Testing)
            .cloned()
        else {
            return Ok(());
        };
        let tree = self
            .vcs
            .checkout(&self.config.qa.branch)
            .await
            .map_err(|e| e.to_string())?;
        let down = self
            .env
            .run("qa", EnvStep::Down, &tree)
            .await
            .map_err(|e| e.to_string())?;
        // A failed `down` raises its own alert; the round still closes.
        self.send(Command::RecordEnvRun {
            step: EnvStep::Down,
            ok: down.ok,
            skipped: down.skipped,
            output_tail: down.output_tail,
        })
        .await?;
        for (employee, ticket) in self.runtime.ticket_sessions() {
            if round.tickets.contains(&ticket)
                && state
                    .ticket(&ticket)
                    .is_some_and(|t| t.state != IN_PROGRESS)
            {
                self.finish_session(&employee, &ticket).await;
            }
        }
        // Say early, while the stakeholder can still act, if the release would be refused.
        let (qa, prod, base) = (
            self.config.qa.branch.as_str(),
            self.config.prod.branch.as_str(),
            self.config.base_branch.as_str(),
        );
        if let Ok(problems) = self.vcs.release_preflight(qa, prod, base).await
            && !problems.is_empty()
        {
            let _ = self
                .send(Command::RaiseAlert {
                    title: "The next release will not go through".into(),
                    detail: format!("- {}", problems.join("\n- ")),
                })
                .await;
        }
        self.send(Command::CloseQaRound).await
    }

    /// The release is approved: put its notes in `CHANGELOG.md` on `qa`, merge `qa` into
    /// the prod branch, tag the merge, and merge prod back into `dev` so the next
    /// promotion stays a fast-forward. An interrupted release finds its tag and goes on.
    ///
    /// With pull requests the merge is a pull request of `qa` into the prod
    /// branch; the tag and a GitHub release follow, and `dev` gets the merge back by a
    /// direct push. Nothing in a stakeholder's own checkouts is needed for that.
    async fn make_release(&self, id: &str) -> Result<(), String> {
        let _turn = self.git_lock.lock().await;
        let state = self.hub.snapshot().await;
        let Some(release) = state
            .release(id)
            .filter(|r| r.status == ReleaseStatus::Approved)
            .cloned()
        else {
            return Ok(());
        };
        let (qa, prod, base) = (
            self.config.qa.branch.as_str(),
            self.config.prod.branch.as_str(),
            self.config.base_branch.as_str(),
        );
        let version = release.version.as_str();
        let vcs = &self.vcs;
        let err = |e: chillax_core::vcs::VcsError| e.to_string();
        let pull_requests = vcs.uses_pull_requests();

        let commit = match vcs.tag_commit(version).await.map_err(err)? {
            // Tagged by an earlier attempt that was interrupted before it was recorded.
            Some(commit) => {
                if !pull_requests && vcs.head(prod).await.ok().as_deref() != Some(commit.as_str()) {
                    return Err(format!(
                        "tag {version} already exists at another commit than the head of \
                         {prod}; approve the release again with a new version"
                    ));
                }
                commit
            }
            None => {
                // Nothing has changed yet, so a refusal here leaves no half-made release.
                let problems = vcs.release_preflight(qa, prod, base).await.map_err(err)?;
                if !problems.is_empty() {
                    return Err(preflight_message(version, &problems));
                }
                let existing = vcs.read_file(qa, release::CHANGELOG).await.map_err(err)?;
                if !existing
                    .as_deref()
                    .is_some_and(|text| release::has_entry(text, version))
                {
                    let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
                    let entry = release::dated_entry(&release.notes, version, &date);
                    vcs.commit_file(
                        qa,
                        release::CHANGELOG,
                        &release::prepend(existing.as_deref(), &entry),
                        &format!("docs: Release notes for {version}"),
                    )
                    .await
                    .map_err(err)?;
                }
                if pull_requests {
                    vcs.fetch().await.map_err(err)?;
                }
                // The prod branch may not exist yet (the very first release).
                if vcs.remote_head(prod).await.map_err(err)?.is_none()
                    && vcs.head(prod).await.is_err()
                {
                    vcs.fast_forward(prod, qa).await.map_err(err)?;
                }
                let message = format!("Release {version}");
                let commit = if pull_requests {
                    vcs.push(qa, false).await.map_err(err)?;
                    let opened = vcs
                        .open_pr(qa, prod, &message, &release.notes)
                        .await
                        .map_err(err)?;
                    let merged = match opened {
                        Some(pr) => vcs.merge_pr(pr.number, prod, &message).await.map_err(err)?,
                        // `qa` is in the prod branch already (an earlier attempt merged it).
                        None => MergeOutcome::Merged {
                            commit: vcs
                                .remote_head(prod)
                                .await
                                .map_err(err)?
                                .ok_or_else(|| format!("{prod} is not on the remote"))?,
                        },
                    };
                    match merged {
                        MergeOutcome::Merged { commit } => commit,
                        MergeOutcome::Conflict { detail, .. } => {
                            return Err(format!(
                                "the release pull request of {qa} into {prod} conflicts: \
                                 {detail}\n\nMerge {prod} into {base} so {qa} can catch up, \
                                 or resolve it on GitHub, then retry the release"
                            ));
                        }
                    }
                } else {
                    match vcs.merge_into(prod, qa, &message).await.map_err(err)? {
                        MergeOutcome::Merged { commit } => commit,
                        MergeOutcome::Conflict { detail, .. } => {
                            return Err(format!("merging {qa} into {prod} conflicted: {detail}"));
                        }
                    }
                };
                vcs.tag(
                    version,
                    &commit,
                    &release::tag_message(version, &release.notes),
                )
                .await
                .map_err(err)?;
                commit
            }
        };

        if pull_requests {
            vcs.push_tag(version).await.map_err(err)?;
            vcs.create_release(version, version, &release.notes)
                .await
                .map_err(err)?;
            self.sync_local_branch(prod).await;
        }

        // The release stands whatever happens to the back-merge; the stakeholder gets an
        // alert saying how to finish it.
        if let Err(detail) = self.merge_back(prod, base, version, pull_requests).await {
            let _ = self
                .send(Command::RaiseAlert {
                    title: format!("Could not merge release {version} back into {base}"),
                    detail: format!(
                        "{detail}\n\nMerge `{prod}` into `{base}` yourself{}; until then `{qa}` \
                         cannot fast-forward to `{base}`.",
                        if pull_requests {
                            " (a pull request from the prod branch works if `dev` is protected)"
                        } else {
                            ""
                        }
                    ),
                })
                .await;
        }
        self.send(Command::RecordRelease {
            id: release.id,
            tag: version.to_string(),
            commit,
        })
        .await
    }

    /// Merge the prod branch back into the base branch (and push it with pull requests).
    async fn merge_back(
        &self,
        prod: &str,
        base: &str,
        version: &str,
        pull_requests: bool,
    ) -> Result<(), String> {
        let err = |e: chillax_core::vcs::VcsError| e.to_string();
        let vcs = &self.vcs;
        let mut source = prod.to_string();
        if pull_requests {
            vcs.sync_local(base).await.map_err(err)?;
            // The remote's copy, which a stale or busy local branch may lack.
            if let Some(head) = vcs.remote_head(prod).await.map_err(err)? {
                source = head;
            }
        }
        let back = format!("Merge release {version} back into {base}");
        match vcs.merge_into(base, &source, &back).await.map_err(err)? {
            MergeOutcome::Merged { .. } => {}
            MergeOutcome::Conflict { detail, .. } => return Err(detail),
        }
        vcs.push(base, false).await.map_err(err)
    }

    /// Bring a local branch up to the remote's, raising an alert when it has to stay behind.
    async fn sync_local_branch(&self, branch: &str) {
        match self.vcs.sync_local(branch).await {
            Ok(None) => {}
            Ok(Some(note)) => {
                let _ = self
                    .send(Command::RaiseAlert {
                        title: format!("Local {branch} is behind the remote"),
                        detail: note,
                    })
                    .await;
            }
            Err(error) => tracing::warn!(%error, "could not update the local {branch}"),
        }
    }

    /// Run the prod deploy command in the prod branch's tree.
    async fn deploy(&self, id: &str) -> Result<(), String> {
        let state = self.hub.snapshot().await;
        let waiting = state.release(id).is_some_and(|r| {
            matches!(
                r.status,
                ReleaseStatus::Tagged | ReleaseStatus::DeployFailed
            )
        });
        if !waiting {
            return Ok(());
        }
        let tree = self
            .vcs
            .checkout(&self.config.prod.branch)
            .await
            .map_err(|e| e.to_string())?;
        let run = self
            .env
            .run("prod", EnvStep::Deploy, &tree)
            .await
            .map_err(|e| e.to_string())?;
        self.send(Command::RecordDeploy {
            id: id.to_string(),
            ok: run.ok,
            skipped: run.skipped,
            output_tail: run.output_tail,
        })
        .await?;
        if run.ok || run.skipped {
            self.converge_release(id).await;
        }
        Ok(())
    }

    /// After a deploy: dev, qa and the prod branch to one commit, pushed, and the main
    /// checkout on prod. Never fails the release; what was skipped is an alert.
    async fn converge_release(&self, release_id: &str) {
        if !self.live().sync_after_deploy {
            return;
        }
        let Some(tag) = self
            .hub
            .snapshot()
            .await
            .release(release_id)
            .map(|r| r.version.clone())
        else {
            return;
        };
        let (prod, base, qa) = (
            self.config.prod.branch.as_str(),
            self.config.base_branch.as_str(),
            self.config.qa.branch.as_str(),
        );
        let notes = match self.vcs.land_release(prod, base, qa, &tag).await {
            Ok(notes) => notes,
            Err(error) => vec![error.to_string()],
        };
        if notes.is_empty() {
            return;
        }
        tracing::warn!(
            ?notes,
            "release {tag} deployed but branches not fully synced"
        );
        let _ = self
            .send(Command::RaiseAlert {
                title: format!("Release {tag} deployed but branches not fully synced"),
                detail: notes.join("\n"),
            })
            .await;
    }

    /// A ticket session finished a turn. If the ticket still waits on that session, the
    /// agent stopped without finishing: nudge it once, and after the second idle turn
    /// close the session and tell the stakeholder, instead of letting it hold a slot.
    fn turn_ended(self: &Arc<Self>, employee: &str, ticket: &str) {
        self.book
            .lock()
            .expect("book lock poisoned")
            .failures
            .remove(ticket);
        let dispatcher = Arc::clone(self);
        let (employee, ticket) = (employee.to_string(), ticket.to_string());
        chillax_core::task::spawn_logged("idle-check", async move {
            // Give the events of a finishing turn (a review request, a verdict) time to land.
            tokio::time::sleep(dispatcher.config.idle_grace).await;
            dispatcher.check_idle(&employee, &ticket).await;
            dispatcher.wake.notify_one();
        });
    }

    async fn check_idle(&self, employee: &str, ticket: &str) {
        let key = SessionKey::ticket(employee, ticket);
        let session = (employee.to_string(), ticket.to_string());
        if !self.runtime.ticket_sessions().contains(&session) {
            return;
        }
        let engine = self.hub.engine().await;
        let working = engine
            .state
            .employee(employee)
            .is_some_and(|e| e.turn_open || e.status == chillax_core::EmployeeStatus::Working);
        // The stakeholder stopped them and has not written back: they are waiting on a
        // person, not stuck.
        if working
            || chillax_core::dispatch::held(&engine, employee)
            || !session_is_waiting(&engine, employee, ticket)
        {
            self.book
                .lock()
                .expect("book lock poisoned")
                .idle
                .remove(&session);
            return;
        }
        // A resolve turn that ended without `request_review`: if the merge is
        // done, take it as finished; otherwise the nudge is about the conflict.
        let conflict = engine
            .state
            .ticket(ticket)
            .filter(|t| t.assignee.as_deref() == Some(employee))
            .and_then(|t| {
                t.conflict
                    .as_ref()
                    .map(|c| (c.files.clone(), t.state.clone()))
            });
        let mut resolving = false;
        if let Some((files, state)) = conflict
            && let Ok(t) = self.ticket(ticket).await
            && let Ok(path) = self.enter_worktree(&t).await
        {
            match self.vcs.conflict_status(&path, &files).await {
                Ok(status) if status.resolved() => {
                    let _ = self
                        .send(Command::RecordSync {
                            id: ticket.to_string(),
                            outcome: SyncOutcome::Resolved,
                            files,
                            note: String::new(),
                        })
                        .await;
                    if state == IN_REVIEW {
                        self.book
                            .lock()
                            .expect("book lock poisoned")
                            .idle
                            .remove(&session);
                        return;
                    }
                }
                Ok(_) => resolving = true,
                Err(error) => {
                    tracing::warn!(%ticket, %error, "could not check the conflict resolution");
                    resolving = true;
                }
            }
        }
        let idle_turns = {
            let mut book = self.book.lock().expect("book lock poisoned");
            let n = book.idle.entry(session.clone()).or_default();
            *n += 1;
            *n
        };
        if idle_turns == 1 {
            tracing::info!(%employee, %ticket, "agent stopped without finishing; nudging");
            let base = &self.config.base_branch;
            let text = if resolving {
                format!(
                    "[Chillax] Your turn ended but the conflict of {ticket} with `{base}` is not \
                     resolved. Run `git merge {base}`, resolve the conflicts, commit, then call \
                     `request_review`. Call `ask_stakeholder` if you are blocked."
                )
            } else {
                format!(
                    "[Chillax] Your turn ended but {ticket} has not moved. Finish with \
                     `request_review` (or `submit_review` / `submit_qa` for your role), or \
                     call `ask_stakeholder` if you are blocked. Do not just stop."
                )
            };
            self.runtime.send_to(&key, text);
            return;
        }
        self.runtime.close(&key).await;
        {
            let mut book = self.book.lock().expect("book lock poisoned");
            book.idle.remove(&session);
            book.cooling
                .insert(ticket.to_string(), Instant::now() + self.config.park);
        }
        let name = engine
            .state
            .actor_name(&Actor::Employee(employee.to_string()));
        self.alert_or_defer(
            &format!("stuck-{ticket}"),
            ProblemKind::AgentStuck,
            &format!("{name} on {ticket}"),
            Some(employee),
            format!("{name} stopped on {ticket} without finishing"),
            "The agent ended two turns in a row without moving the ticket, so Chillax \
             closed its session. Refresh (R) to start it again with a fresh briefing."
                .to_string(),
        )
        .await;
    }

    /// A session failed. Back the ticket off, and act on why: a signed-out agent halts
    /// its backend, a usage limit pauses it until a probe gets through.
    async fn agent_failed(&self, employee: &str, error: &str, ticket: Option<&str>) {
        let Some(backend) = self
            .hub
            .snapshot()
            .await
            .employee(employee)
            .map(|e| e.employee.backend.clone())
        else {
            return;
        };
        let kind = limits::classify_failure(error, chrono::Utc::now(), local_offset());
        let title = backend_title(&backend);
        {
            let mut book = self.book.lock().expect("book lock poisoned");
            book.employee_cooling.insert(
                employee.to_string(),
                Instant::now() + self.config.backoff_base,
            );
            if let Some(ticket) = ticket {
                let wait = if kind == FailureKind::Auth {
                    // No retry helps until someone signs in; a refresh clears this.
                    self.config.park
                } else {
                    let n = book.failures.entry(ticket.to_string()).or_default();
                    *n += 1;
                    limits::backoff(*n, self.config.backoff_base, self.config.backoff_cap)
                };
                book.cooling
                    .insert(ticket.to_string(), Instant::now() + wait);
            }
            if kind == FailureKind::Auth {
                book.halted.insert(backend.clone());
            }
        }
        match kind {
            FailureKind::Auth => {
                let _ = self
                    .send(Command::RaiseAlert {
                        title: format!("{title} is not signed in"),
                        detail: error.to_string(),
                    })
                    .await;
            }
            FailureKind::RateLimit { reset } => {
                let message: String = error.chars().take(400).collect();
                let _ = self
                    .send(Command::RecordBackendLimit {
                        backend: backend.clone(),
                        until: reset,
                        message: message.clone(),
                    })
                    .await;
                let detail = match reset {
                    Some(at) => format!(
                        "{message}\n\nChillax starts no work on {title} until it resets (about {}) \
                         and checks then.",
                        at.with_timezone(&chrono::Local).format("%H:%M")
                    ),
                    None => format!(
                        "{message}\n\nChillax starts no work on {title} and checks every few \
                         minutes whether it is back."
                    ),
                };
                let _ = self
                    .send(Command::RaiseAlert {
                        title: limit_alert_title(&backend),
                        detail,
                    })
                    .await;
            }
            FailureKind::Other => {
                // A teammate's crash goes to the project manager first (ADR 0032). The
                // planner's own crash, and a stopped teammate's, are not his to handle.
                let engine = self.hub.engine().await;
                let planner = engine
                    .roles
                    .get(&role_of(&engine, employee))
                    .is_some_and(|r| {
                        engine
                            .workflow
                            .role_owns(r, engine.workflow.initial_state())
                    });
                if !planner && !chillax_core::dispatch::held(&engine, employee) {
                    let name = engine
                        .state
                        .actor_name(&Actor::Employee(employee.to_string()));
                    let detail: String = error.chars().take(400).collect();
                    self.defer_quietly(
                        &format!("agent-{employee}"),
                        ProblemKind::AgentCrashed,
                        &name,
                        Some(employee),
                        format!("{name}'s agent failed"),
                        detail,
                    )
                    .await;
                }
            }
        }
    }

    /// A bug was filed for a failure: leave the stakeholder a note (dismissable, nothing
    /// to do), and let go of the wait for it. The failure's own alert, if any, stays.
    async fn bug_filed(&self, fingerprint: &str, bug: &str) {
        self.book
            .lock()
            .expect("book lock poisoned")
            .cooling
            .remove(&chillax_core::dispatch::triage_key(fingerprint));
        let state = self.hub.snapshot().await;
        if let Some(failure) = state.failures.iter().find(|f| f.fingerprint == fingerprint) {
            let (title, detail) = chillax_core::triage::filed_note(&state, failure, bug);
            let _ = self.send(Command::RaiseAlert { title, detail }).await;
        }
    }

    /// A session of `employee` started, so their backend is signed in after all.
    async fn signed_in(&self, employee: &str) {
        let Some(backend) = self
            .hub
            .snapshot()
            .await
            .employee(employee)
            .map(|e| e.employee.backend.clone())
        else {
            return;
        };
        let was_halted = self
            .book
            .lock()
            .expect("book lock poisoned")
            .halted
            .remove(&backend);
        self.problem_solved(&format!("agent-{employee}"));
        if was_halted {
            self.clear_alerts(&format!("{} is not signed in", backend_title(&backend)))
                .await;
        }
    }

    /// The refresh command: forget every wait and give every idle agent a fresh start.
    async fn kick(&self) {
        {
            let mut book = self.book.lock().expect("book lock poisoned");
            book.cooling.clear();
            book.failures.clear();
            book.action_failures.clear();
            book.idle.clear();
            book.halted.clear();
            book.reported.clear();
            book.problems.clear();
            let now = Instant::now();
            for probe in book.probes.values_mut() {
                probe.due = now;
            }
        }
        let closed = self.runtime.close_idle().await;
        tracing::info!(sessions = closed.len(), "dispatch refreshed");
    }

    /// Schedule the next probe of a limited backend, replacing any earlier plan.
    fn plan_probe(
        &self,
        backend: &str,
        reset: Option<chrono::DateTime<chrono::Utc>>,
        attempt: u32,
    ) {
        let wait = limits::probe_delay(reset, chrono::Utc::now(), attempt, &*self.config.jitter);
        let mut book = self.book.lock().expect("book lock poisoned");
        let running = book.probes.get(backend).is_some_and(|p| p.running);
        book.probes.insert(
            backend.to_string(),
            Probe {
                due: Instant::now() + wait,
                attempt,
                running,
            },
        );
    }

    /// Start the probes that are due, and make sure every limited backend has a plan
    /// (after a restart the limit is in the event log but no plan is).
    fn start_probes(self: &Arc<Self>, engine: &Engine) {
        let limited = &engine.state.backend_limits;
        let missing: Vec<(String, Option<chrono::DateTime<chrono::Utc>>)> = {
            let mut book = self.book.lock().expect("book lock poisoned");
            book.probes
                .retain(|backend, _| limited.contains_key(backend));
            limited
                .iter()
                .filter(|(backend, _)| !book.probes.contains_key(*backend))
                .map(|(backend, limit)| (backend.clone(), limit.until))
                .collect()
        };
        for (backend, until) in missing {
            self.plan_probe(&backend, until, 0);
        }
        let due: Vec<String> = {
            let mut book = self.book.lock().expect("book lock poisoned");
            let now = Instant::now();
            book.probes
                .iter_mut()
                .filter(|(_, p)| p.due <= now && !p.running)
                .map(|(backend, p)| {
                    p.running = true;
                    backend.clone()
                })
                .collect()
        };
        for backend in due {
            let Some(employee) = prober(engine, &backend) else {
                // Nobody on this backend: nothing to wait for.
                let dispatcher = Arc::clone(self);
                chillax_core::task::spawn_logged("probe", async move {
                    let _ = dispatcher
                        .send(Command::RecordBackendRecovered { backend })
                        .await;
                });
                continue;
            };
            let dispatcher = Arc::clone(self);
            chillax_core::task::spawn_logged("probe", async move {
                dispatcher.probe(&backend, &employee).await;
                dispatcher.wake.notify_one();
            });
        }
    }

    /// Ask the backend one cheap question through `employee` (the PM, if there is one).
    async fn probe(&self, backend: &str, employee: &str) {
        tracing::info!(%backend, %employee, "probing a rate-limited backend");
        let outcome = self.runtime.probe_backend(employee).await;
        let attempt = {
            let mut book = self.book.lock().expect("book lock poisoned");
            let probe = book.probes.get_mut(backend);
            match (&outcome, probe) {
                (Ok(()), _) => {
                    book.probes.remove(backend);
                    None
                }
                (Err(_), Some(probe)) => {
                    probe.running = false;
                    probe.attempt += 1;
                    Some(probe.attempt)
                }
                (Err(_), None) => Some(1),
            }
        };
        let Err(error) = outcome else {
            let _ = self
                .send(Command::RecordBackendRecovered {
                    backend: backend.to_string(),
                })
                .await;
            return;
        };
        let attempt = attempt.unwrap_or(1);
        match limits::classify_failure(&error, chrono::Utc::now(), local_offset()) {
            FailureKind::RateLimit { reset } => {
                let message: String = error.chars().take(400).collect();
                let _ = self
                    .send(Command::RecordBackendLimit {
                        backend: backend.to_string(),
                        until: reset,
                        message,
                    })
                    .await;
                self.plan_probe(backend, reset, attempt);
            }
            FailureKind::Auth => {
                // Not a limit any more: sign-in is the problem, and says so.
                self.book
                    .lock()
                    .expect("book lock poisoned")
                    .halted
                    .insert(backend.to_string());
                let _ = self
                    .send(Command::RaiseAlert {
                        title: format!("{} is not signed in", backend_title(backend)),
                        detail: error,
                    })
                    .await;
                let _ = self
                    .send(Command::RecordBackendRecovered {
                        backend: backend.to_string(),
                    })
                    .await;
            }
            FailureKind::Other => {
                tracing::warn!(%backend, %error, "probe failed");
                self.plan_probe(backend, None, attempt);
            }
        }
    }

    /// A limited backend answered: forget the waits its failures caused and start again.
    async fn recovered(&self, backend: &str) {
        {
            let mut book = self.book.lock().expect("book lock poisoned");
            book.cooling.clear();
            book.failures.clear();
            book.probes.remove(backend);
        }
        self.clear_alerts(&limit_alert_title(backend)).await;
    }

    /// Dismiss the alerts a problem raised once it is solved.
    async fn clear_alerts(&self, title: &str) {
        let ids: Vec<String> = self
            .hub
            .snapshot()
            .await
            .inbox
            .iter()
            .filter_map(|item| match item {
                InboxItem::Alert { id, title: t, .. } if t == title => Some(id.clone()),
                _ => None,
            })
            .collect();
        for id in ids {
            let _ = self.send(Command::DismissAlert { id }).await;
        }
    }

    /// Ask the planner to assign the sprint's loose tickets; nothing else starts them.
    async fn nudge_planner(&self, sprint: &str) {
        let engine = self.hub.engine().await;
        let state = &engine.state;
        let initial = engine.workflow.initial_state();
        let Some(planner) = state.employees.iter().find(|e| {
            engine
                .roles
                .get(&e.employee.role)
                .is_some_and(|r| engine.workflow.role_owns(r, initial))
        }) else {
            return;
        };
        let loose: Vec<&str> = state
            .sprint(sprint)
            .into_iter()
            .flat_map(|sp| sp.tickets.iter())
            .filter_map(|id| state.ticket(id))
            .filter(|t| t.assignee.is_none())
            .map(|t| t.id.as_str())
            .collect();
        if loose.is_empty() {
            return;
        }
        let text = format!(
            "[Chillax] Sprint {sprint} was approved. These tickets have no assignee yet: {}. \
             Assign each one to a teammate with `assign`; Chillax starts assigned work on its own.",
            loose.join(", ")
        );
        let _ = self
            .send(Command::SendChat {
                to: planner.employee.id.clone(),
                text,
            })
            .await;
    }

    /// Close an employee's ticket session because their work on it is done, then empty
    /// their live transcript (ADR 0008). Only when a ticket session really ended and they
    /// have no other: the buffer is per employee, so another live ticket keeps it. Runs
    /// after the close returns, which waits out the turn, so no late chunk
    /// refills the buffer. Replay is deterministic: the clear is an ordinary event.
    async fn finish_session(&self, employee: &str, ticket: &str) {
        if !self
            .runtime
            .close_after_turn(&SessionKey::ticket(employee, ticket), TURN_GRACE)
            .await
        {
            return;
        }
        if self
            .runtime
            .ticket_sessions()
            .iter()
            .any(|(e, _)| e == employee)
        {
            return;
        }
        let _ = self
            .send(Command::ClearEmployeeContext {
                employee: employee.to_string(),
                ticket: ticket.to_string(),
            })
            .await;
        // A chat session (teammate message, check-in) would otherwise keep the old
        // context until the idle timeout; it closes once its turn is over.
        self.runtime.start_fresh_next(employee);
    }

    /// The review is over and the ticket is back with its developer: free the reviewer's
    /// session, which would otherwise sit idle against the concurrent-agent limit.
    async fn close_reviewer(&self, id: &str) {
        if let Ok(ticket) = self.ticket(id).await
            && let Some(reviewer) = &ticket.reviewer
        {
            self.runtime
                .close_after_turn(&SessionKey::ticket(reviewer, id), TURN_GRACE)
                .await;
        }
    }

    /// Message the developer's session on a ticket, if it is running. A session that is
    /// not running gets the news in its briefing when it opens.
    async fn tell_developer(&self, id: &str, text: String) {
        let Ok(ticket) = self.ticket(id).await else {
            return;
        };
        if let Some(developer) = &ticket.assignee {
            self.runtime
                .send_to(&SessionKey::ticket(developer, id), text);
        }
    }

    /// Delete the QA scratch folders of tickets that are over: done or dropped,
    /// in no open round, with no session left. They hold build output and clones,
    /// gigabytes each. The delete runs off the async threads.
    async fn sweep_qa_scratch(&self) {
        let engine = self.hub.engine().await;
        let dirs = self.qa_orphans(&engine);
        self.remove_dirs(dirs).await;
    }

    fn qa_orphans(&self, engine: &Engine) -> Vec<PathBuf> {
        cleanup::orphan_qa_dirs(
            &self.config.root.join(QA_SCRATCH),
            &engine.state,
            engine.workflow.terminal_state(),
            &self.runtime.ticket_sessions(),
        )
    }

    async fn remove_dirs(&self, dirs: Vec<PathBuf>) {
        if dirs.is_empty() {
            return;
        }
        let removed = tokio::task::spawn_blocking(move || {
            dirs.iter()
                .map(|dir| match cleanup::remove_measured(dir) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        tracing::warn!(dir = %dir.display(), %error, "could not remove scratch folder");
                        0
                    }
                })
                .sum::<u64>()
        })
        .await;
        match removed {
            Ok(bytes) if bytes > 0 => tracing::info!(bytes, "freed finished tickets' QA scratch"),
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "scratch cleanup task failed"),
        }
    }

    /// Before dispatching: clear what earlier runs left behind. QA scratch
    /// and worktrees of finished tickets go, as does a crashed merge's `_merge-*`
    /// tree. Worktrees leave through git so its metadata stays consistent. `_dev`,
    /// `_qa`, `_prod` and anything in progress are never touched.
    async fn startup_sweep(&self) {
        let engine = self.hub.engine().await;
        self.remove_dirs(self.qa_orphans(&engine)).await;
        let worktrees = self.config.root.join(".chillax/worktrees");
        let stale = cleanup::stale_worktrees(
            &worktrees,
            &engine.state,
            engine.workflow.terminal_state(),
            &self.runtime.ticket_sessions(),
        );
        for path in &stale {
            if let Err(error) = self.vcs.remove_worktree(path).await {
                tracing::warn!(dir = %path.display(), %error, "could not remove stale worktree");
            }
        }
        if !stale.is_empty() {
            // A path that is gone makes the adapter run `git worktree prune`.
            let _ = self.vcs.remove_worktree(&worktrees.join("_pruned")).await;
            tracing::info!(count = stale.len(), "removed stale worktrees");
        }
    }

    async fn ticket(&self, id: &str) -> Result<Ticket, String> {
        self.hub
            .snapshot()
            .await
            .ticket(id)
            .cloned()
            .ok_or_else(|| format!("{id} no longer exists"))
    }

    /// Whether the ticket's work lives in the shared dev worktree. A ticket started in
    /// `per-ticket` mode keeps its own worktree if the setting changes.
    fn is_shared(&self, ticket: &Ticket) -> bool {
        self.live().worktrees == WorktreeMode::Shared
            && ticket.worktree.as_deref().is_none_or(|w| w == DEV_WORKTREE)
    }

    /// Where the ticket's agents work, ready for them: its own worktree (created if it
    /// is gone), or the dev worktree switched to the ticket's branch.
    async fn enter_worktree(&self, ticket: &Ticket) -> Result<PathBuf, String> {
        let branch = ticket
            .branch
            .clone()
            .ok_or_else(|| format!("{} has no branch", ticket.id))?;
        let path = if self.is_shared(ticket) {
            self.config.root.join(DEV_WORKTREE)
        } else {
            self.worktree(ticket)
        };
        let err = |e: chillax_core::vcs::VcsError| e.to_string();
        if !path.join(".git").exists() {
            self.vcs
                .create_worktree(&branch, &path)
                .await
                .map_err(err)?;
        } else if self.is_shared(ticket) {
            self.vcs
                .switch_branch(&path, &branch, Some(&self.config.base_branch))
                .await
                .map_err(err)?;
        }
        Ok(path)
    }

    /// Put the dev worktree back on the base branch.
    async fn park_dev_worktree(
        &self,
        path: &std::path::Path,
    ) -> Result<(), chillax_core::vcs::VcsError> {
        if !path.join(".git").exists() {
            return Ok(());
        }
        self.vcs
            .switch_branch(path, &self.config.base_branch, None)
            .await
    }

    fn worktree(&self, ticket: &Ticket) -> PathBuf {
        ticket.worktree_path(&self.config.root)
    }

    async fn send(&self, cmd: Command) -> Result<(), String> {
        self.hub
            .send(Actor::System, cmd)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Halted backends and employees still cooling after a failure (expired ones dropped).
    fn book_halts(&self) -> (BTreeSet<String>, BTreeSet<String>) {
        let mut book = self.book.lock().expect("book lock poisoned");
        let now = Instant::now();
        book.employee_cooling.retain(|_, until| *until > now);
        (
            book.halted.clone(),
            book.employee_cooling.keys().cloned().collect(),
        )
    }

    fn busy(&self) -> BTreeSet<TicketId> {
        let mut book = self.book.lock().expect("book lock poisoned");
        let now = Instant::now();
        book.cooling.retain(|_, until| *until > now);
        book.inflight
            .iter()
            .chain(book.cooling.keys())
            .cloned()
            .collect()
    }

    fn claim(&self, ticket: &str) -> bool {
        self.book
            .lock()
            .expect("book lock poisoned")
            .inflight
            .insert(ticket.to_string())
    }

    fn release(&self, ticket: &str) {
        self.book
            .lock()
            .expect("book lock poisoned")
            .inflight
            .remove(ticket);
    }

    fn cool(&self, ticket: &str) {
        self.book
            .lock()
            .expect("book lock poisoned")
            .cooling
            .insert(ticket.to_string(), Instant::now() + self.config.cooldown);
    }
}

fn role_of(engine: &Engine, employee: &str) -> String {
    engine
        .state
        .employee(employee)
        .map(|e| e.employee.role.clone())
        .unwrap_or_default()
}

fn local_offset() -> chrono::FixedOffset {
    *chrono::Local::now().offset()
}

fn limit_alert_title(backend: &str) -> String {
    format!("{} usage limit reached", backend_title(backend))
}

/// Who probes a limited backend: its project manager if there is one (the planner is the
/// cheapest to spare), else anyone hired onto it.
fn prober(engine: &Engine, backend: &str) -> Option<String> {
    let on_backend = || {
        engine
            .state
            .employees
            .iter()
            .filter(|e| e.employee.backend == backend)
    };
    let initial = engine.workflow.initial_state();
    on_backend()
        .find(|e| {
            engine
                .roles
                .get(&e.employee.role)
                .is_some_and(|r| engine.workflow.role_owns(r, initial))
        })
        .or_else(|| on_backend().next())
        .map(|e| e.employee.id.clone())
}

/// Why a release was refused before it began, and how to get it going again.
fn preflight_message(version: &str, problems: &[String]) -> String {
    format!(
        "release {version} cannot be made yet:\n- {}\n\nFix that, then press Retry on the \
         release (Environments view, or `chillax env --retry`); or reject the release.",
        problems.join("\n- ")
    )
}

/// The inbox alert's title for an action that failed without a ticket to report on.
fn app_alert_title(env: &str) -> String {
    format!("The {env} app keeps crashing")
}

fn alert_title(action: &Action) -> String {
    format!("Chillax could not {}", describe(action))
}

/// What a failed action was trying to do, for the note on the ticket.
fn describe(action: &Action) -> &'static str {
    match action {
        Action::StartWork { .. } => "create the worktree and start work",
        Action::Reassign { .. } => "reassign the ticket",
        Action::RaiseAlert { .. } => "raise the alert",
        Action::OpenSession { .. } => "start the agent session",
        Action::RunTests { .. } => "run the tests",
        Action::StartReview { .. } => "hand the ticket to a reviewer",
        Action::Merge { .. } => "merge the branch into dev",
        Action::Rebase { .. } => "rebase the branch onto dev",
        Action::ResolveConflict { .. } => "hand the conflict to its developer",
        Action::Promote => "promote dev to qa",
        Action::RunEnv { .. } => "run the QA environment",
        Action::CloseRound => "close the QA round",
        Action::Release { .. } => "make the release",
        Action::Deploy { .. } => "deploy the release",
        Action::TriageFailure { .. } => "brief the project manager about a failure",
        Action::PlanSprint { .. } => "brief the project manager to plan the sprint",
        Action::PmCheckin { .. } => "brief the project manager with the board check-in",
        Action::PmHandleProblem { .. } => "brief the project manager about problems",
        Action::CloseHungSession { .. } => "close a hung agent turn",
        Action::PmUnstick { .. } => "brief the project manager about the stalled board",
        Action::StallAlert => "tell the stakeholder the board has stalled",
        Action::TriageRequest { .. } => "brief the project manager about a request",
        Action::RunApp { .. } => "start the app",
        Action::StopApp { .. } => "stop the app",
    }
}

/// A branch-name fragment from a ticket title.
fn short_slug(title: &str) -> String {
    let slug: String = slug(title).chars().take(40).collect();
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        "work".into()
    } else {
        slug.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_names_come_from_titles() {
        assert_eq!(
            short_slug("Add password reset email"),
            "add-password-reset-email"
        );
        assert_eq!(short_slug("  !!  "), "work");
        let long = short_slug(&"word ".repeat(30));
        assert!(long.len() <= 40 && !long.ends_with('-'), "{long}");
    }
}
