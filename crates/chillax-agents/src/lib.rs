//! ACP client runtime. One supervised agent subprocess per employee.
//!
//! The runtime reacts to hub events: a chat message to an employee starts their session
//! (lazily, to save cost) and becomes an ACP prompt; answers and permission decisions
//! flow back the same way. Everything the agent streams is recorded as events.

mod acp;
mod convert;
mod directory;
mod doctor;
mod editor;
mod envfiles;
pub mod procgroup;
mod session;
mod usage;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use agent_client_protocol::Responder;
use agent_client_protocol::schema::v1::{
    RequestPermissionOutcome, RequestPermissionResponse, SelectedPermissionOutcome,
};
use chillax_core::config::{AgentConfig, PermissionMode};
use chillax_core::dispatch::SessionKind;
use chillax_core::{
    Actor, Command, CompanyHandle, EmployeeId, Event, EventKind, Hub, InboxId, PriceTable, TicketId,
};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

/// Company chat notes kept per employee until their next prompt.
const COMPANY_NOTES_KEEP: usize = 20;

/// How long closing a session mid-turn waits for the agent to answer `session/cancel`
/// (its answer carries the turn's token usage) before killing the process.
const CANCEL_GRACE: Duration = Duration::from_secs(2);

/// How long a closing session's process group gets to exit after SIGTERM before SIGKILL.
const GROUP_GRACE: Duration = Duration::from_secs(5);

/// Default for how long a stopped turn gets to wind down before the session is closed
/// (`RuntimeConfig::stop_grace`).
const STOP_GRACE: Duration = Duration::from_secs(10);

pub use acp::describe_error;
pub use directory::{AcpAgentDirectory, ConfigWriter};
pub use doctor::{AgentInfo, HandshakeError, handshake};
pub use editor::AcpPromptEditor;
pub use envfiles::EnvFiles;

/// The `[agents.*]` backends, shared so a backend enabled at runtime is usable by
/// sessions, the persona editor and the directory without a restart.
pub type SharedBackends = Arc<RwLock<BTreeMap<String, AgentConfig>>>;

pub fn shared_backends(backends: BTreeMap<String, AgentConfig>) -> SharedBackends {
    Arc::new(RwLock::new(backends))
}

/// A unique-enough suffix for scratch directories and prompt fences.
pub(crate) fn nonce() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:x}{:x}{n:x}", std::process::id())
}

/// Issues the bearer token an employee's session uses for the Chillax MCP endpoint.
pub type TokenIssuer = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// Where agent sessions reach the Chillax MCP tools.
#[derive(Clone)]
pub struct McpEndpoint {
    pub url: String,
    pub issue_token: TokenIssuer,
}

/// What the runtime is told at start-up.
pub struct RuntimeConfig {
    pub company: String,
    /// Working directory for chat sessions: the repository root.
    pub cwd: PathBuf,
    pub backends: SharedBackends,
    pub mcp: Option<McpEndpoint>,
    /// `[budget] max_concurrent_agents`: ticket sessions only. Chats do not count.
    pub max_concurrent: usize,
    /// The live value of that limit when it can change while running; read
    /// instead of `max_concurrent` when set. Lowering never closes a running session.
    pub max_concurrent_live: Option<Arc<AtomicUsize>>,
    /// Chat sessions kept open at once; starting another closes the least recently used
    /// idle one.
    pub max_chat: usize,
    /// A chat session with no turn running for this long is closed (it restarts on the
    /// next message).
    pub chat_idle: Duration,
    /// How often idle chats are looked for.
    pub sweep: Duration,
    /// `[agents] permissions`.
    pub permissions: PermissionMode,
    /// `[pricing]`: estimates cost for backends that report tokens but no cost.
    pub pricing: Arc<PriceTable>,
    /// How long a turn the stakeholder stopped gets to answer `session/cancel` before the
    /// session is closed.
    pub stop_grace: Duration,
    /// The project's `.env` files: QA sessions get the `qa` stage, every other session
    /// `dev`.
    pub env_files: EnvFiles,
}

impl RuntimeConfig {
    /// The ticket-session cap right now.
    fn limit(&self) -> usize {
        self.max_concurrent_live
            .as_ref()
            .map_or(self.max_concurrent, |live| live.load(Ordering::Relaxed))
    }
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            company: String::new(),
            cwd: PathBuf::new(),
            backends: SharedBackends::default(),
            mcp: None,
            max_concurrent: 3,
            max_concurrent_live: None,
            max_chat: 3,
            chat_idle: Duration::from_secs(10 * 60),
            sweep: Duration::from_secs(15),
            permissions: PermissionMode::default(),
            pricing: Arc::default(),
            stop_grace: STOP_GRACE,
            env_files: EnvFiles::default(),
        }
    }
}

/// Parked permission requests by inbox id, with the employee who asked (a stop answers
/// theirs with `Cancelled`).
type PendingPermissions =
    Arc<Mutex<HashMap<InboxId, (EmployeeId, Responder<RequestPermissionResponse>)>>>;

/// Whether a session is mid-turn and when it last did anything, for idle timeouts and
/// for telling which sessions a refresh may close.
#[derive(Debug)]
pub(crate) struct Activity {
    busy: AtomicBool,
    last: Mutex<Instant>,
    /// Set when a Chillax briefing is done: close the chat once the turn ends so the
    /// next task starts fresh. Cleared when another turn begins.
    fresh_next: AtomicBool,
    /// A close asked the running turn to cancel and is waiting for it to wind down, so
    /// the session task must not announce the stop itself.
    closing: AtomicBool,
    /// Wakes the session task to send `session/cancel` for the running turn.
    cancel: tokio::sync::Notify,
    /// Bumped each time the stakeholder stops this session; the session task compares it
    /// with the value it last saw. Does not end the session.
    stop_gen: AtomicU64,
    stop: tokio::sync::Notify,
    /// Turns begun so far, so the stop watchdog knows whether the turn it watches is
    /// still the running one.
    turns: AtomicU64,
    /// Prompts sent to the session task that it has not taken yet.
    queued: AtomicUsize,
    /// Process group of the agent process (0 until known), so a close can stop what the
    /// agent started before the process goes (ADR 0015).
    pgid: AtomicU32,
}

impl Activity {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            busy: AtomicBool::new(false),
            last: Mutex::new(Instant::now()),
            fresh_next: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            cancel: tokio::sync::Notify::new(),
            stop_gen: AtomicU64::new(0),
            stop: tokio::sync::Notify::new(),
            turns: AtomicU64::new(0),
            queued: AtomicUsize::new(0),
            pgid: AtomicU32::new(0),
        })
    }

    pub(crate) fn set_pgid(&self, pgid: u32) {
        self.pgid.store(pgid, Ordering::SeqCst);
    }

    /// End the agent's process group gracefully (SIGTERM, then SIGKILL after `grace`).
    async fn end_group(&self, grace: Duration) {
        let pgid = self.pgid.swap(0, Ordering::SeqCst);
        if pgid != 0 {
            // The session task will see its agent die; the closer announces the stop.
            self.closing.store(true, Ordering::SeqCst);
            procgroup::terminate(pgid, grace).await;
        }
    }

    pub(crate) fn is_closing(&self) -> bool {
        self.closing.load(Ordering::SeqCst)
    }

    pub(crate) async fn cancelled(&self) {
        self.cancel.notified().await;
    }

    pub(crate) fn stop_gen(&self) -> u64 {
        self.stop_gen.load(Ordering::SeqCst)
    }

    /// Resolves once the stop generation moves past `seen`.
    pub(crate) async fn stopped_since(&self, seen: u64) {
        loop {
            let notified = self.stop.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.stop_gen() != seen {
                return;
            }
            notified.await;
        }
    }

    /// A prompt is on its way to the session task.
    pub(crate) fn enqueued(&self) {
        self.queued.fetch_add(1, Ordering::SeqCst);
    }

    /// The session task took a queued prompt.
    pub(crate) fn dequeued(&self) {
        if self.queued.load(Ordering::SeqCst) > 0 {
            self.queued.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// Ask the session to drop its running turn, or the prompts still queued for it,
    /// without ending the session. False when it has nothing to stop.
    fn request_stop(&self) -> bool {
        if !self.is_busy() && self.queued.load(Ordering::SeqCst) == 0 {
            return false;
        }
        self.stop_gen.fetch_add(1, Ordering::SeqCst);
        self.stop.notify_waiters();
        true
    }

    fn touch(&self) {
        *self.last.lock().expect("activity lock poisoned") = Instant::now();
    }

    pub(crate) fn begin_turn(&self) {
        self.fresh_next.store(false, Ordering::SeqCst);
        self.turns.fetch_add(1, Ordering::SeqCst);
        self.busy.store(true, Ordering::SeqCst);
        self.touch();
    }

    pub(crate) fn end_turn(&self) {
        self.busy.store(false, Ordering::SeqCst);
        self.touch();
    }

    fn wants_fresh(&self) -> bool {
        self.fresh_next.load(Ordering::SeqCst)
            && !self.is_busy()
            && self.queued.load(Ordering::SeqCst) == 0
    }

    fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }

    fn idle_for(&self) -> Duration {
        if self.is_busy() {
            Duration::ZERO
        } else {
            self.last.lock().expect("activity lock poisoned").elapsed()
        }
    }
}

/// Identifies a session: an employee's chat session has no ticket; the dev loop opens
/// one session per ticket the employee is building or reviewing.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionKey {
    pub employee: EmployeeId,
    pub ticket: Option<TicketId>,
}

impl SessionKey {
    pub fn chat(employee: impl Into<EmployeeId>) -> Self {
        Self {
            employee: employee.into(),
            ticket: None,
        }
    }

    pub fn ticket(employee: impl Into<EmployeeId>, ticket: impl Into<TicketId>) -> Self {
        Self {
            employee: employee.into(),
            ticket: Some(ticket.into()),
        }
    }
}

/// What the dev loop needs to start an employee on a ticket.
pub struct TicketSession {
    pub employee: EmployeeId,
    pub ticket: TicketId,
    pub kind: SessionKind,
    /// The ticket's worktree.
    pub cwd: PathBuf,
    /// A directory outside the worktree where a QA session may write its own scratch
    /// files; everything else in the shared tree stays read-only.
    pub scratch: Option<PathBuf>,
    /// The first message: the assignment, with everything the agent needs to start.
    pub briefing: String,
}

struct SessionHandle {
    tx: mpsc::UnboundedSender<String>,
    /// Aborting the task drops the connection, which kills the agent process. Awaiting it
    /// afterwards waits for that to have happened.
    task: JoinHandle<()>,
    activity: Arc<Activity>,
    /// Kept here so the close paths can flush it after the task is aborted.
    usage: Arc<usage::SessionUsage>,
}

pub struct AgentRuntime {
    hub: Arc<Hub>,
    config: RuntimeConfig,
    sessions: Mutex<HashMap<SessionKey, SessionHandle>>,
    pending: PendingPermissions,
    /// Question id → asking employee, so answers can be routed after the inbox item
    /// is gone from the projection.
    questions: Mutex<HashMap<InboxId, EmployeeId>>,
    /// Sessions the stakeholder stopped mid-turn, until they next write to the employee:
    /// that message goes back into the session they stopped.
    stopped: Mutex<HashSet<SessionKey>>,
    /// Company chat posts to everyone that did not mention the employee: they ride along
    /// with the employee's next prompt instead of waking them. In memory only.
    company_notes: Mutex<HashMap<EmployeeId, VecDeque<String>>>,
    /// Set by [`AgentRuntime::shutdown_all`]: no session may start afterwards.
    shutting_down: AtomicBool,
}

impl AgentRuntime {
    pub fn new(hub: Arc<Hub>, config: RuntimeConfig) -> Arc<Self> {
        Arc::new(Self {
            hub,
            config,
            sessions: Mutex::default(),
            pending: Arc::default(),
            questions: Mutex::default(),
            stopped: Mutex::default(),
            company_notes: Mutex::default(),
            shutting_down: AtomicBool::new(false),
        })
    }

    /// Start reacting to hub events. Subscribe before returning so no event is missed.
    pub fn start(self: &Arc<Self>) -> JoinHandle<()> {
        let sweeper = Arc::clone(self);
        chillax_core::task::spawn_logged("agent-chat-sweeper", async move {
            let mut tick = tokio::time::interval(sweeper.config.sweep);
            loop {
                tick.tick().await;
                let closed = sweeper.close_idle_chats(sweeper.config.chat_idle).await;
                if !closed.is_empty() {
                    tracing::info!(sessions = closed.len(), "closed idle chat sessions");
                }
            }
        });
        let mut rx = self.hub.subscribe();
        let runtime = Arc::clone(self);
        chillax_core::task::spawn_logged("agent-runtime", async move {
            loop {
                match rx.recv().await {
                    Ok(event) => runtime.on_event(event).await,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(missed = n, "agent runtime lagged behind the event bus");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }

    /// Record what every live session has reported but not yet recorded (cost so far this
    /// turn), without ending anything. Tokens only arrive with a finished turn, so a
    /// running turn's tokens show up when it ends or is cancelled by a close.
    pub async fn flush_usage(&self) {
        let tallies: Vec<Arc<usage::SessionUsage>> = self
            .sessions
            .lock()
            .expect("sessions lock poisoned")
            .values()
            .map(|h| Arc::clone(&h.usage))
            .collect();
        for usage in tallies {
            if let Some(flush) = usage.take_flush() {
                let actor = Actor::Employee(usage.employee.clone());
                let _ = self.hub.record(actor, flush).await;
            }
        }
    }

    /// Every agent session that is open, of any kind.
    pub fn session_count(&self) -> usize {
        self.sessions.lock().expect("sessions lock poisoned").len()
    }

    /// Open ticket sessions: the ones `max_concurrent_agents` limits.
    pub fn ticket_session_count(&self) -> usize {
        self.sessions
            .lock()
            .expect("sessions lock poisoned")
            .keys()
            .filter(|k| k.ticket.is_some())
            .count()
    }

    /// The open ticket sessions, as (employee, ticket).
    pub fn ticket_sessions(&self) -> BTreeSet<(EmployeeId, TicketId)> {
        self.sessions
            .lock()
            .expect("sessions lock poisoned")
            .keys()
            .filter_map(|k| Some((k.employee.clone(), k.ticket.clone()?)))
            .collect()
    }

    /// Sessions in the middle of a turn, with how long ago the turn started or a prompt
    /// was last queued for them. The stall watchdog combines it with the
    /// agent's own output to find turns that hang.
    pub fn busy_sessions(&self) -> Vec<(SessionKey, Duration)> {
        self.sessions
            .lock()
            .expect("sessions lock poisoned")
            .iter()
            .filter(|(_, h)| h.activity.is_busy())
            .map(|(k, h)| {
                let since = h.activity.last.lock().expect("activity lock poisoned");
                (k.clone(), since.elapsed())
            })
            .collect()
    }

    /// Start an employee on a ticket in its worktree. If they already have a session for
    /// it, the briefing is queued as another message instead.
    pub async fn open_ticket_session(
        self: &Arc<Self>,
        session: TicketSession,
    ) -> Result<(), String> {
        let key = SessionKey::ticket(&session.employee, &session.ticket);
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err("Chillax is shutting down".to_string());
        }
        if self.send_to(&key, session.briefing.clone()) {
            return Ok(());
        }
        let ticket = session::TicketSpec {
            id: session.ticket,
            kind: session.kind,
            scratch: session.scratch,
        };
        self.start_session(key, session.briefing, session.cwd, Some(ticket))
            .await
    }

    /// Queue a message for an existing session. False when there is none.
    pub fn send_to(&self, key: &SessionKey, text: String) -> bool {
        let (text, noted) = self.with_notes(&key.employee, text);
        let sent = self
            .sessions
            .lock()
            .expect("sessions lock poisoned")
            .get(key)
            .is_some_and(|s| {
                s.activity.touch();
                s.activity.enqueued();
                let sent = s.tx.send(text).is_ok();
                if !sent {
                    s.activity.dequeued();
                }
                sent
            });
        if sent {
            self.drop_notes(&key.employee, noted);
        }
        sent
    }

    /// Queue a company chat post for `employee`'s next prompt. Keeps the newest
    /// `COMPANY_NOTES_KEEP`.
    fn queue_note(&self, employee: &str, note: String) {
        let mut notes = self.company_notes.lock().expect("notes lock poisoned");
        let queue = notes.entry(employee.to_string()).or_default();
        queue.push_back(note);
        while queue.len() > COMPANY_NOTES_KEEP {
            queue.pop_front();
        }
    }

    /// `text` with the employee's queued company chat notes in front, and how many notes
    /// that took. The notes stay queued until [`Self::drop_notes`], so a failed send
    /// loses nothing.
    fn with_notes(&self, employee: &str, text: String) -> (String, usize) {
        let notes = self.company_notes.lock().expect("notes lock poisoned");
        let Some(queue) = notes.get(employee).filter(|q| !q.is_empty()) else {
            return (text, 0);
        };
        let list: Vec<String> = queue.iter().map(|n| format!("- {n}")).collect();
        (
            format!(
                "Company chat since your last prompt:\n{}\n\n{text}",
                list.join("\n")
            ),
            queue.len(),
        )
    }

    fn drop_notes(&self, employee: &str, count: usize) {
        if count == 0 {
            return;
        }
        let mut notes = self.company_notes.lock().expect("notes lock poisoned");
        if let Some(queue) = notes.get_mut(employee) {
            queue.drain(..count.min(queue.len()));
        }
    }

    /// Stop a session and its agent process.
    pub async fn close(&self, key: &SessionKey) -> bool {
        let handle = self
            .sessions
            .lock()
            .expect("sessions lock poisoned")
            .remove(key);
        let Some(handle) = handle else {
            return false;
        };
        Self::end_task(&handle.activity, handle.task).await;
        self.finish_session(handle.usage).await;
        self.record_stopped(&key.employee).await;
        true
    }

    /// Close a session whose agent just submitted its own outcome (a verdict, a
    /// report). The session leaves the table at once, so its slot frees and no new
    /// prompt reaches it, but a turn still running is given up to `grace` to end by
    /// itself: cancelling it mid-call aborts the tool call that carried the outcome
    /// before its response reaches the agent. Past the grace it is cancelled as usual.
    pub async fn close_after_turn(&self, key: &SessionKey, grace: Duration) -> bool {
        let handle = self
            .sessions
            .lock()
            .expect("sessions lock poisoned")
            .remove(key);
        let Some(handle) = handle else {
            return false;
        };
        let deadline = Instant::now() + grace;
        while handle.activity.is_busy() && !handle.task.is_finished() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Self::end_task(&handle.activity, handle.task).await;
        self.finish_session(handle.usage).await;
        self.record_stopped(&key.employee).await;
        true
    }

    /// Stop a session task. A turn in flight is asked to cancel first and given
    /// [`CANCEL_GRACE`] to answer, because the answer is the only place an agent reports
    /// the turn's tokens; killing it outright loses them. A task that ends by itself has
    /// recorded the turn but not announced the stop (the caller does).
    async fn end_task(activity: &Arc<Activity>, mut task: JoinHandle<()>) {
        if activity.is_busy() {
            activity.closing.store(true, Ordering::SeqCst);
            activity.cancel.notify_one();
            if tokio::time::timeout(CANCEL_GRACE, &mut task).await.is_ok() {
                activity.end_group(GROUP_GRACE).await;
                return;
            }
        }
        activity.end_group(GROUP_GRACE).await;
        task.abort();
    }

    /// Before `employee` starts on ticket `keep`, close their other idle ticket sessions
    /// so context never carries across tickets and the slots are freed. A session that
    /// is mid-turn, or whose ticket `spare` says still waits on them (parallel QA), is
    /// kept; the chat session is never touched.
    pub async fn close_other_ticket_sessions(
        &self,
        employee: &str,
        keep: &str,
        spare: impl Fn(&str) -> bool,
    ) -> Vec<SessionKey> {
        let keys: Vec<SessionKey> = self
            .sessions
            .lock()
            .expect("sessions lock poisoned")
            .iter()
            .filter(|(k, h)| {
                k.employee == employee
                    && k.ticket.as_deref().is_some_and(|t| t != keep && !spare(t))
                    && !h.activity.is_busy()
            })
            .map(|(k, _)| k.clone())
            .collect();
        let mut closed = Vec::new();
        for key in keys {
            if self.close(&key).await {
                tracing::info!(
                    %employee,
                    closed = key.ticket.as_deref().unwrap_or_default(),
                    new = keep,
                    "closing stale session before new ticket"
                );
                closed.push(key);
            }
        }
        closed
    }

    /// Stop every ticket session (pause). Chat sessions are left alone.
    pub async fn close_ticket_sessions(&self) -> Vec<SessionKey> {
        let keys: Vec<SessionKey> = self
            .sessions
            .lock()
            .expect("sessions lock poisoned")
            .keys()
            .filter(|k| k.ticket.is_some())
            .cloned()
            .collect();
        for key in &keys {
            self.close(key).await;
        }
        keys
    }

    /// Stop every session that is not in the middle of a turn (the refresh command). A
    /// ticket whose session closed gets a new one, with a fresh briefing, on the next
    /// dispatcher tick.
    pub async fn close_idle(&self) -> Vec<SessionKey> {
        self.close_where(|_, activity| !activity.is_busy()).await
    }

    /// Quit: stop every session, wait (up to `timeout`) for each agent process to be
    /// killed, and record one `AgentStopped` per employee so the next start does not show
    /// them as working. No session can start afterwards.
    pub async fn shutdown_all(&self, timeout: Duration) {
        self.shutting_down.store(true, Ordering::SeqCst);
        let handles: Vec<(SessionKey, SessionHandle)> = self
            .sessions
            .lock()
            .expect("sessions lock poisoned")
            .drain()
            .collect();
        let employees: BTreeSet<EmployeeId> =
            handles.iter().map(|(k, _)| k.employee.clone()).collect();
        let mut tallies = Vec::new();
        // Stop what the agents started first, all groups at once.
        futures::future::join_all(handles.iter().map(|(_, h)| h.activity.end_group(timeout))).await;
        let tasks: Vec<JoinHandle<()>> = handles
            .into_iter()
            .map(|(_, handle)| {
                handle.task.abort();
                tallies.push(handle.usage);
                handle.task
            })
            .collect();
        let waited = tokio::time::timeout(timeout, async {
            for task in tasks {
                // Cancelled tasks return an error; all that matters is that they ended.
                let _ = task.await;
            }
        })
        .await;
        if waited.is_err() {
            tracing::warn!("agent sessions did not stop in time; quitting anyway");
        }
        for usage in tallies {
            self.finish_session(usage).await;
        }
        for employee in employees {
            self.record_stopped(&employee).await;
        }
    }

    /// Stop chat sessions that have had no turn running for `idle`. Ticket sessions are
    /// the dispatcher's to close.
    ///
    /// A session marked by [`Self::start_fresh_next`] closes as soon as it is idle with
    /// nothing queued; when that leaves the employee with no session at all, their
    /// transcript is emptied too (ADR 0008), so the next task starts from a clean slate.
    pub async fn close_idle_chats(&self, idle: Duration) -> Vec<SessionKey> {
        let wrapped_up = Mutex::new(BTreeSet::new());
        let closed = self
            .close_where(|key, activity| {
                let wrap_up = activity.wants_fresh();
                if key.ticket.is_none() && wrap_up {
                    wrapped_up
                        .lock()
                        .expect("wrap-up lock poisoned")
                        .insert(key.employee.clone());
                }
                key.ticket.is_none() && (wrap_up || activity.idle_for() >= idle)
            })
            .await;
        let wrapped_up = wrapped_up.into_inner().expect("wrap-up lock poisoned");
        for key in &closed {
            let has_other = self
                .sessions
                .lock()
                .expect("sessions lock poisoned")
                .keys()
                .any(|k| k.employee == key.employee);
            if wrapped_up.contains(&key.employee) && !has_other {
                let _ = self
                    .hub
                    .send(
                        Actor::System,
                        Command::ClearEmployeeContext {
                            employee: key.employee.clone(),
                            ticket: String::new(),
                        },
                    )
                    .await;
            }
        }
        closed
    }

    /// The employee's work is done (a Chillax briefing, or their last ticket): close
    /// their chat session so the next task starts fresh. A running turn is never cut
    /// off and a queued message is delivered first; the sweeper closes it once idle.
    pub fn start_fresh_next(&self, employee: &str) {
        let key = SessionKey::chat(employee);
        if let Some(handle) = self
            .sessions
            .lock()
            .expect("sessions lock poisoned")
            .get(&key)
        {
            handle.activity.fresh_next.store(true, Ordering::SeqCst);
        }
    }

    async fn close_where(&self, pick: impl Fn(&SessionKey, &Activity) -> bool) -> Vec<SessionKey> {
        let keys: Vec<SessionKey> = self
            .sessions
            .lock()
            .expect("sessions lock poisoned")
            .iter()
            .filter(|(key, handle)| pick(key, &handle.activity))
            .map(|(key, _)| key.clone())
            .collect();
        let mut closed = Vec::new();
        for key in keys {
            if self.close(&key).await {
                closed.push(key);
            }
        }
        closed
    }

    /// Check that an employee's backend answers: a throwaway session sends one tiny
    /// prompt and waits for the reply. It takes no slot and records no events. Used to
    /// find out whether a usage limit has lifted.
    pub async fn probe_backend(&self, employee: &str) -> Result<(), String> {
        let snapshot = self.hub.snapshot().await;
        let view = snapshot
            .employee(employee)
            .ok_or_else(|| format!("{employee} is not on the team"))?;
        let backend = self
            .config
            .backends
            .read()
            .expect("backends lock poisoned")
            .get(&view.employee.backend)
            .cloned()
            .ok_or_else(|| {
                format!(
                    "no `[agents.{}]` backend in chillax.toml",
                    view.employee.backend
                )
            })?;
        let role = self
            .hub
            .roles()
            .into_iter()
            .find(|r| r.id == view.employee.role)
            .ok_or_else(|| format!("role `{}` no longer exists", view.employee.role))?;
        let spec = session::SessionSpec {
            employee: view.employee.clone(),
            role,
            company: self.config.company.clone(),
            cwd: self.config.cwd.clone(),
            backend,
            mcp: None,
            ticket: None,
            permissions: self.config.permissions,
            probe: true,
            pricing: Arc::clone(&self.config.pricing),
            env: BTreeMap::new(),
        };
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send("Reply with the single word: ok".to_string())
            .map_err(|e| e.to_string())?;
        // Dropping the sender ends the session after that one turn.
        drop(tx);
        session::run(
            spec,
            Arc::clone(&self.hub),
            self.pending.clone(),
            rx,
            Activity::new(),
            Arc::new(usage::SessionUsage::new(
                format!("{employee}-probe"),
                employee.to_string(),
                None,
            )),
        )
        .await
    }

    /// Flush what a closed session still owes (cost reported after its last sample) and
    /// post its one usage comment on the ticket. Safe to call twice.
    async fn finish_session(&self, usage: Arc<usage::SessionUsage>) {
        let actor = Actor::Employee(usage.employee.clone());
        if let Some(flush) = usage.take_flush() {
            let _ = self.hub.record(actor, flush).await;
        }
        let Some(data) = usage.take_comment() else {
            return;
        };
        let totals = self
            .hub
            .snapshot()
            .await
            .usage
            .by_ticket
            .get(&data.ticket)
            .cloned()
            .unwrap_or_default();
        let body = chillax_core::format_usage_comment(&chillax_core::UsageComment {
            employee: &data.employee,
            model: data.model.as_deref(),
            input_tokens: data.input,
            output_tokens: data.output,
            cached_tokens: data.cached,
            cost_micros: data.cost_micros,
            currency: &data.currency,
            estimated: data.estimated,
            ticket_total_micros: totals.total_cost_micros(),
            ticket_total_estimated: totals.estimated_cost_micros > 0,
        });
        let _ = self
            .hub
            .record(
                Actor::System,
                EventKind::TicketCommented {
                    id: data.ticket,
                    body,
                    external_id: None,
                    attachments: Vec::new(),
                },
            )
            .await;
    }

    /// Record that an employee has no agent running, unless another session of theirs
    /// still does.
    async fn record_stopped(&self, employee: &str) {
        let others = self
            .sessions
            .lock()
            .expect("sessions lock poisoned")
            .keys()
            .any(|k| k.employee == employee);
        if !others {
            let _ = self
                .hub
                .record(
                    Actor::System,
                    EventKind::AgentStopped {
                        employee: employee.to_string(),
                    },
                )
                .await;
        }
    }

    async fn on_event(self: &Arc<Self>, event: Event) {
        match event.kind {
            EventKind::ChatMessageSent { to, text } => {
                let snapshot = self.hub.snapshot().await;
                // The stakeholder's words belong in the chat. A teammate's message may
                // reach the ticket session the employee is busy in.
                let from_teammate = matches!(event.actor, Actor::Employee(_));
                let text = match &event.actor {
                    Actor::Employee(from) => {
                        let name = snapshot.actor_name(&event.actor);
                        let title = snapshot
                            .employee(from)
                            .map(|e| e.employee.role_title.clone())
                            .unwrap_or_default();
                        format!(
                            "Message from your teammate {name} ({title}, id `{from}`):\n\n{text}"
                        )
                    }
                    _ => text,
                };
                // A message to an employee whose turn the stakeholder stopped goes back
                // into the session they stopped, ticket or chat.
                let stopped_ticket = if from_teammate {
                    None
                } else {
                    self.take_stopped(&to)
                };
                if let Some(key) = stopped_ticket
                    && self.send_to(&key, text.clone())
                {
                    return;
                }
                self.deliver(&to, text, from_teammate).await;
            }
            EventKind::CompanyChatPosted { text, mentions } => {
                let snapshot = self.hub.snapshot().await;
                for e in &snapshot.employees {
                    let id = &e.employee.id;
                    if mentions.contains(id) {
                        let text = format!(
                            "The stakeholder wrote in the company chat and mentioned you:\n\n{text}"
                        );
                        self.deliver(id, text, false).await;
                    } else {
                        self.queue_note(id, format!("Stakeholder to everyone: {text}"));
                    }
                }
            }
            EventKind::UsageRefreshRequested => {
                self.flush_usage().await;
                let _ = self
                    .hub
                    .record(Actor::System, EventKind::UsageRefreshed)
                    .await;
            }
            EventKind::QuestionAsked { id, employee, .. } => {
                self.questions
                    .lock()
                    .expect("questions lock poisoned")
                    .insert(id, employee);
            }
            EventKind::QuestionAnswered { id, answer } => {
                let employee = self
                    .questions
                    .lock()
                    .expect("questions lock poisoned")
                    .remove(&id);
                if let Some(employee) = employee {
                    let text = format!("The stakeholder answered your question {id}:\n\n{answer}");
                    // The employee asked from wherever they were working.
                    self.deliver(&employee, text, true).await;
                }
            }
            EventKind::PermissionResolved {
                id,
                choice,
                automatic: false,
            } => {
                let responder = self
                    .pending
                    .lock()
                    .expect("pending lock poisoned")
                    .remove(&id);
                if let Some((_, responder)) = responder {
                    let outcome = match choice {
                        Some(choice) => RequestPermissionOutcome::Selected(
                            SelectedPermissionOutcome::new(choice),
                        ),
                        None => RequestPermissionOutcome::Cancelled,
                    };
                    if let Err(err) = responder.respond(RequestPermissionResponse::new(outcome)) {
                        tracing::warn!(%id, %err, "could not deliver permission decision");
                    }
                }
            }
            EventKind::TurnCancelled { employee } => self.cancel_turn(&employee),
            EventKind::ChatCleared { employee } => {
                // The next chat message opens a fresh session with no old context.
                self.close(&SessionKey::chat(employee.as_str())).await;
            }
            // The project manager finished a briefing from Chillax.
            EventKind::SprintProposed { .. }
            | EventKind::TriageBugFiled { .. }
            | EventKind::RequestTriaged { .. } => {
                if let Actor::Employee(employee) = &event.actor {
                    self.start_fresh_next(employee);
                }
            }
            // A change the stakeholder queued while the employee was replying
            // takes effect once their turn is over, however it ended.
            EventKind::AgentTurnEnded { employee, .. }
            | EventKind::AgentFailed { employee, .. }
            | EventKind::AgentStopped { employee } => {
                let queued = self
                    .hub
                    .snapshot()
                    .await
                    .employee(&employee)
                    .is_some_and(|e| e.pending_agent.is_some());
                if queued
                    && let Err(err) = self
                        .hub
                        .send(Actor::System, Command::ApplyQueuedAgentChange { employee })
                        .await
                {
                    tracing::warn!(%err, "could not apply a queued agent change");
                }
            }
            EventKind::EmployeeLetGo { employee } => self.close_employee_sessions(&employee).await,
            // Sessions never resume, so the old ones end; the next chat message or
            // dispatcher briefing starts on the new agent.
            EventKind::EmployeeAgentChanged { employee, .. } => {
                self.close_employee_sessions(&employee).await
            }
            // The project manager restarted a teammate (ADR 0032): end their sessions, which
            // records `AgentStopped` (a restart is not a failure).
            EventKind::EmployeeRestarted { employee } => {
                self.close_employee_sessions(&employee).await
            }
            _ => {}
        }
    }

    /// Stop what `employee` is doing but keep their sessions and context: each
    /// session mid-turn (or with a prompt queued) is told to drop it, their parked
    /// permission requests are answered `Cancelled`, and a session that does not wind
    /// down within `stop_grace` is closed. Nothing is recorded here: the turn ends with
    /// the usual `AgentTurnEnded { stop_reason: "cancelled" }`.
    fn cancel_turn(self: &Arc<Self>, employee: &str) {
        let parked: Vec<_> = {
            let mut pending = self.pending.lock().expect("pending lock poisoned");
            let ids: Vec<InboxId> = pending
                .iter()
                .filter(|(_, (who, _))| who == employee)
                .map(|(id, _)| id.clone())
                .collect();
            ids.iter().filter_map(|id| pending.remove(id)).collect()
        };
        for (_, responder) in parked {
            let outcome = RequestPermissionOutcome::Cancelled;
            if let Err(err) = responder.respond(RequestPermissionResponse::new(outcome)) {
                tracing::warn!(%employee, %err, "could not cancel a permission request");
            }
        }
        let stopping: Vec<(SessionKey, Arc<Activity>, bool)> = self
            .sessions
            .lock()
            .expect("sessions lock poisoned")
            .iter()
            .filter(|(k, _)| k.employee == employee)
            .filter_map(|(k, h)| {
                let busy = h.activity.is_busy();
                h.activity
                    .request_stop()
                    .then(|| (k.clone(), Arc::clone(&h.activity), busy))
            })
            .collect();
        for (key, activity, busy) in stopping {
            self.stopped
                .lock()
                .expect("stopped lock poisoned")
                .insert(key.clone());
            if !busy {
                continue;
            }
            let turn = activity.turns.load(Ordering::SeqCst);
            let runtime = Arc::clone(self);
            let grace = self.config.stop_grace;
            chillax_core::task::spawn_logged("agent-stop-watchdog", async move {
                tokio::time::sleep(grace).await;
                if activity.is_busy() && activity.turns.load(Ordering::SeqCst) == turn {
                    tracing::warn!(employee = %key.employee, "agent ignored a stop; closing its session");
                    runtime.close(&key).await;
                }
            });
        }
    }

    /// Forget the sessions of `employee` the stakeholder stopped and return the ticket
    /// session among them, if any, to receive their next message.
    fn take_stopped(&self, employee: &str) -> Option<SessionKey> {
        let mut stopped = self.stopped.lock().expect("stopped lock poisoned");
        let keys: Vec<SessionKey> = stopped
            .iter()
            .filter(|k| k.employee == employee)
            .cloned()
            .collect();
        for key in &keys {
            stopped.remove(key);
        }
        keys.into_iter()
            .filter(|k| k.ticket.is_some())
            .min_by(|a, b| a.ticket.cmp(&b.ticket))
    }

    /// End every session of `employee` (chat and tickets) and record one `AgentStopped`
    /// if there were any.
    async fn close_employee_sessions(&self, employee: &str) {
        let handles: Vec<SessionHandle> = {
            let mut sessions = self.sessions.lock().expect("sessions lock poisoned");
            let keys: Vec<SessionKey> = sessions
                .keys()
                .filter(|k| k.employee == employee)
                .cloned()
                .collect();
            keys.iter().filter_map(|k| sessions.remove(k)).collect()
        };
        // Aborting the task drops the connection, which kills the process.
        let had_session = !handles.is_empty();
        for handle in handles {
            Self::end_task(&handle.activity, handle.task).await;
            self.finish_session(handle.usage).await;
        }
        if had_session {
            let _ = self
                .hub
                .record(
                    Actor::System,
                    EventKind::AgentStopped {
                        employee: employee.to_string(),
                    },
                )
                .await;
        }
    }

    /// Queue a prompt for an employee. Chat goes to their chat session, which starts if
    /// needed. `prefer_ticket` (a teammate's message, an answer to their question) goes
    /// to the session they have on a ticket instead, if there is one: a stakeholder's chat
    /// never lands in the middle of a ticket's context.
    async fn deliver(self: &Arc<Self>, employee: &str, text: String, prefer_ticket: bool) {
        if prefer_ticket {
            let ticket_key = {
                let sessions = self.sessions.lock().expect("sessions lock poisoned");
                sessions
                    .keys()
                    .filter(|k| k.employee == employee && k.ticket.is_some())
                    .min_by(|a, b| a.ticket.cmp(&b.ticket))
                    .cloned()
            };
            if let Some(key) = ticket_key
                && self.send_to(&key, text.clone())
            {
                return;
            }
        }
        let chat = SessionKey::chat(employee);
        if self.send_to(&chat, text.clone()) {
            return;
        }
        if let Err(error) = self
            .start_session(chat, text, self.config.cwd.clone(), None)
            .await
        {
            let _ = self
                .hub
                .record(
                    Actor::Employee(employee.to_string()),
                    EventKind::AgentFailed {
                        employee: employee.to_string(),
                        error,
                        ticket: None,
                    },
                )
                .await;
        }
    }

    async fn start_session(
        self: &Arc<Self>,
        key: SessionKey,
        first: String,
        cwd: PathBuf,
        ticket: Option<session::TicketSpec>,
    ) -> Result<(), String> {
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err("Chillax is shutting down".to_string());
        }
        let employee = key.employee.clone();
        let (first, noted) = self.with_notes(&employee, first);
        let snapshot = self.hub.snapshot().await;
        let view = snapshot
            .employee(&employee)
            .ok_or_else(|| format!("{employee} is not on the team"))?;
        let backend = self
            .config
            .backends
            .read()
            .expect("backends lock poisoned")
            .get(&view.employee.backend)
            .cloned()
            .ok_or_else(|| {
                format!(
                    "no `[agents.{}]` backend in chillax.toml",
                    view.employee.backend
                )
            })?;
        let role = self
            .hub
            .roles()
            .into_iter()
            .find(|r| r.id == view.employee.role)
            .ok_or_else(|| format!("role `{}` no longer exists", view.employee.role))?;
        let spec = session::SessionSpec {
            employee: view.employee.clone(),
            role,
            company: self.config.company.clone(),
            cwd,
            backend,
            mcp: self.config.mcp.clone(),
            env: self
                .config
                .env_files
                .vars(ticket.as_ref().map_or("dev", |t| t.kind.stage()))
                .await,
            ticket,
            permissions: self.config.permissions,
            probe: false,
            pricing: Arc::clone(&self.config.pricing),
        };
        let usage = Arc::new(usage::SessionUsage::new(
            format!("{employee}-{}", nonce()),
            employee.clone(),
            spec.ticket.as_ref().map(|t| t.id.clone()),
        ));
        let task_usage = Arc::clone(&usage);

        let (tx, rx) = mpsc::unbounded_channel();
        let activity = Activity::new();
        activity.enqueued();
        tx.send(first).map_err(|e| e.to_string())?;
        self.drop_notes(&employee, noted);
        let runtime = Arc::clone(self);
        let task_key = key.clone();
        let task_tx = tx.clone();
        let task_activity = Arc::clone(&activity);
        let task_activity_closing = Arc::clone(&activity);
        let evicted;
        {
            let mut sessions = self.sessions.lock().expect("sessions lock poisoned");
            evicted = self.make_room(&mut sessions, &key)?;
            let task = tokio::spawn(async move {
                let result = session::run(
                    spec,
                    Arc::clone(&runtime.hub),
                    runtime.pending.clone(),
                    rx,
                    task_activity,
                    Arc::clone(&task_usage),
                )
                .await;
                // Forget the session (unless a newer one replaced it) so the next message
                // starts a fresh one.
                {
                    let mut sessions = runtime.sessions.lock().expect("sessions lock poisoned");
                    if sessions
                        .get(&task_key)
                        .is_some_and(|s| s.tx.same_channel(&task_tx))
                    {
                        sessions.remove(&task_key);
                    }
                }
                runtime.finish_session(task_usage).await;
                let employee = task_key.employee;
                let kind = match result {
                    Ok(()) => EventKind::AgentStopped {
                        employee: employee.clone(),
                    },
                    Err(error) => {
                        tracing::warn!(%employee, %error, "agent session failed");
                        EventKind::AgentFailed {
                            employee: employee.clone(),
                            error,
                            ticket: task_key.ticket,
                        }
                    }
                };
                let still_employed = runtime.hub.snapshot().await.employee(&employee).is_some();
                // A close that cancelled the turn announces the stop itself.
                if still_employed && !task_activity_closing.is_closing() {
                    let _ = runtime.hub.record(Actor::Employee(employee), kind).await;
                }
            });
            sessions.insert(
                key,
                SessionHandle {
                    tx,
                    task,
                    activity,
                    usage,
                },
            );
        }
        for (old, old_usage) in evicted {
            self.finish_session(old_usage).await;
            self.record_stopped(&old.employee).await;
        }
        Ok(())
    }

    /// Check the cap that applies to `key` and, for a chat, free a slot by closing the
    /// least recently used idle chat. Returns the evicted keys, to be reported once the
    /// lock is released.
    fn make_room(
        &self,
        sessions: &mut HashMap<SessionKey, SessionHandle>,
        key: &SessionKey,
    ) -> Result<Vec<(SessionKey, Arc<usage::SessionUsage>)>, String> {
        if key.ticket.is_some() {
            let open = sessions.keys().filter(|k| k.ticket.is_some()).count();
            let limit = self.config.limit();
            if open >= limit {
                return Err(format!(
                    "max_concurrent_agents ({limit}) reached; let someone go or raise the limit"
                ));
            }
            return Ok(Vec::new());
        }
        let chats = sessions.keys().filter(|k| k.ticket.is_none()).count();
        if chats < self.config.max_chat {
            return Ok(Vec::new());
        }
        let oldest = sessions
            .iter()
            .filter(|(k, h)| k.ticket.is_none() && !h.activity.is_busy())
            .max_by_key(|(_, h)| h.activity.idle_for())
            .map(|(k, _)| k.clone());
        let Some(oldest) = oldest else {
            return Err(format!(
                "{} chats are busy at once; wait for one to finish",
                self.config.max_chat
            ));
        };
        let mut evicted = Vec::new();
        if let Some(handle) = sessions.remove(&oldest) {
            handle.task.abort();
            evicted.push((oldest, handle.usage));
        }
        Ok(evicted)
    }
}
