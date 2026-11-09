//! Chillax terminal UI. State lives in [`App`]; drawing reads it (`view`), key handlers
//! write it. The TUI never mutates company state: keys produce [`Command`]s that the
//! run loop sends to the company, and the event bus brings the results back.

mod clipboard;
mod editor;
mod env;
mod form;
mod hire;
mod keys;
mod link;
mod live;
mod mouse;
mod picker;
mod projects;
mod scroll;
mod spinner;
mod transcript;
mod usage;
mod view;

use editor::Editor;
use form::FormOutcome;
pub use form::{Field, FormTarget, TicketForm};
use keys::{Act, Lookup};

use std::collections::VecDeque;
use std::io;
use std::sync::Arc;

use chillax_core::engine::State;
use chillax_core::{
    Actor, AgentDirectory, Attachment, AttachmentStore, Command, CompanyHandle, EnvStep, Event,
    InboxItem, InstanceInfo, InstanceSource, Recruiter, ReleaseStatus, Role, SprintStatus,
    SprintView, Ticket,
};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as TermEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton,
    MouseEventKind,
};
use futures::StreamExt;
use ratatui::DefaultTerminal;
use ratatui::layout::{Position, Rect};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

pub use hire::{AgentTarget, HireFlow, HireStep, RecruitReply, RecruitRequest, Review, ReviewView};
pub use picker::{AgentPicker, PickStage, Picked};
pub use projects::{ProjectList, ProjectPick};

/// How many recent events the Events tab keeps.
const EVENT_HISTORY: usize = 1000;

/// Enter the alternate screen with mouse capture on. The panic hook releases both
/// (ratatui's hook only restores the screen), so a crash leaves the terminal usable.
pub fn init_terminal() -> io::Result<DefaultTerminal> {
    let terminal = ratatui::init();
    crossterm::execute!(io::stdout(), EnableMouseCapture, EnableBracketedPaste)?;
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::execute!(io::stdout(), DisableMouseCapture, DisableBracketedPaste);
        previous(info);
    }));
    Ok(terminal)
}

/// Undo [`init_terminal`] on a normal exit.
pub fn restore_terminal() {
    let _ = crossterm::execute!(io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Dashboard,
    Team,
    Chat,
    Board,
    Live,
    Environments,
    Inbox,
    Events,
    Settings,
    Usage,
}

impl Tab {
    pub const ALL: [Tab; 10] = [
        Tab::Dashboard,
        Tab::Team,
        Tab::Chat,
        Tab::Board,
        Tab::Live,
        Tab::Environments,
        Tab::Inbox,
        Tab::Events,
        Tab::Settings,
        Tab::Usage,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Dashboard => "Dashboard",
            Tab::Team => "Team",
            Tab::Chat => "Chat",
            Tab::Board => "Board",
            Tab::Live => "Live",
            Tab::Environments => "Environments",
            Tab::Inbox => "Inbox",
            Tab::Events => "Events",
            Tab::Settings => "Settings",
            Tab::Usage => "Usage",
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }
}

/// Modal input state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Normal,
    /// Typing a chat message.
    Compose,
    Hire(Box<HireFlow>),
    Answer {
        id: String,
        text: String,
    },
    ConfirmLetGo {
        employee: String,
    },
    /// Asking before the TUI quits.
    ConfirmQuit,
    ConfirmClearChat {
        employee: String,
    },
    /// The detail panel for one Board ticket.
    Ticket {
        id: String,
    },
    /// Creating or editing a work item.
    Form(TicketForm),
    /// Changing the hiring defaults from the Settings tab.
    Settings(Box<AgentPicker>),
    /// Changing the agent of an existing hire.
    EmployeeAgent {
        employee: String,
        picker: Box<AgentPicker>,
    },
    /// The running-projects switcher.
    Projects(ProjectList),
    /// Editing the version and notes of a release at the gate.
    Release(Box<ReleaseEdit>),
    /// Typing why a release is rejected.
    RejectRelease {
        id: String,
        text: String,
    },
    /// Asking before the release gate opens over tickets that failed QA.
    ConfirmForce,
    /// Typing a comment on a ticket.
    Comment {
        id: String,
        text: String,
        /// Image paths, comma separated.
        files: String,
        /// 0 is the text, 1 the image paths.
        field: usize,
    },
    /// Choosing who a ticket is assigned to. `choice` 0 is "unassigned", then the team.
    Assign {
        id: String,
        choice: usize,
    },
    /// Editing which tickets block this one (ids separated by spaces or commas).
    Blockers {
        id: String,
        text: String,
    },
    /// Dropping a ticket: first the reason, then a yes/no (`confirm`).
    Drop {
        id: String,
        reason: String,
        confirm: bool,
    },
    /// Asking the project manager for the next sprint, with an optional note.
    PlanSprint {
        text: String,
    },
    /// Asking before an active sprint is closed.
    ConfirmCloseSprint {
        id: String,
    },
    /// Describing a need for the project manager to file and assign (multi-line).
    Request {
        text: Editor,
        start_now: bool,
    },
    /// The list of requests and what became of them.
    Requests,
}

/// The release gate's editor: two fields, the version and the (multi-line) notes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseEdit {
    pub id: String,
    pub version: String,
    pub notes: String,
    /// 0 is the version, 1 the notes.
    pub field: usize,
}

#[derive(Debug, Default)]
pub struct App {
    pub state: State,
    /// Spinner clock, advanced every 100 ms while [`App::is_busy`].
    pub tick: u64,
    pub roles: Vec<Role>,
    pub events: VecDeque<Event>,
    pub tab: Tab,
    pub mode: Mode,
    /// Selected employee (Team and Chat tabs).
    pub selected: usize,
    pub inbox_selected: usize,
    /// Ticket highlighted on the Board. Held by id so the highlight follows the ticket
    /// when an event moves it to another column.
    pub board_selected: Option<String>,
    /// Board tab: the board has been entered (Enter or ↓), so ←/→ move columns instead of
    /// changing view. The selection is kept while this is off.
    pub board_focus: bool,
    /// Chat tab: show the company chat feed instead of one person's conversation, and
    /// send what is typed to everyone.
    pub company_view: bool,
    /// Team tab, grid mode: the grid has been entered (Enter or ↓), so the arrows walk the
    /// tiles instead of changing view.
    pub team_focus: bool,
    /// The chat message being typed.
    pub draft: Editor,
    /// Live view: tool output shown in full instead of folded.
    pub expanded: bool,
    /// Team tab: tiles instead of a list. Kept for the session.
    pub grid: bool,
    /// Lines scrolled up from the bottom of the chat / event log.
    pub scroll: u16,
    /// The most `scroll` can usefully be, as of the last draw (the log is that much taller
    /// than its window). Keeps PageUp from running past the top.
    scroll_max: std::sync::atomic::AtomicU16,
    /// Usage tab: the all-time totals when the view was first loaded (or `r` last pressed),
    /// for the "since you opened this view" line. Client side; not tied to any run.
    pub(crate) usage_base: Option<(
        chillax_core::usage::UsageTotals,
        chrono::DateTime<chrono::Utc>,
    )>,
    /// Usage tab: the token chart shows hours instead of days.
    pub(crate) usage_hourly: bool,
    /// Which tickets the Board shows: the active sprint, a past sprint or the backlog.
    pub sprint_view: SprintView,
    /// Lines scrolled down in each Board column, by column index.
    pub board_scroll: Vec<u16>,
    /// Interior of a Board column (width << 16 | height) as of the last draw.
    board_view: std::sync::atomic::AtomicU32,
    /// Lines scrolled down in the Requests popup, and the most it can scroll as drawn.
    requests_scroll: u16,
    requests_max: std::sync::atomic::AtomicU16,
    /// Lines scrolled down in the ticket detail panel.
    pub ticket_scroll: u16,
    /// The most the ticket panel can scroll and its window height, as of the last draw.
    ticket_max: std::sync::atomic::AtomicU16,
    ticket_view: std::sync::atomic::AtomicU16,
    /// Row where each section of the ticket panel starts, as of the last draw.
    ticket_marks: std::sync::Mutex<Vec<u16>>,
    /// The section Tab last jumped to; `None` after scrolling by hand.
    ticket_section: Option<usize>,
    pub status: Option<String>,
    /// A confirmation ("Copied N chars"), shown in the footer in place of nothing; cleared by the next key.
    pub toast: Option<String>,
    /// OSC 52 sequence waiting for the run loop to write it.
    pending_clipboard: Option<String>,
    /// Mouse capture is off so the terminal can select text (inverted so the default is on).
    pub mouse_released: bool,
    /// Capture change waiting for the run loop: `Some(true)` on, `Some(false)` off.
    pending_mouse: Option<bool>,
    pub web_url: Option<String>,
    /// Where the running projects are listed from; `None` hides the switcher.
    instances: Option<InstanceHandle>,
    /// Root of this TUI's own project, marked in the switcher.
    project_root: Option<String>,
    /// The switcher wants the list loaded; the run loop does it.
    projects_request: bool,
    /// Catalog work queued by the hire wizard for the run loop.
    recruit_requests: Vec<RecruitRequest>,
    /// Where comment images are stored; `None` hides nothing but refuses attaching.
    attachments: Option<StoreHandle>,
    /// Image files queued for the system viewer; the run loop opens them.
    open_requests: Vec<std::path::PathBuf>,
    /// The image `o` opens next in the ticket panel (it cycles).
    attach_cursor: usize,
    /// The app selected in the Environments view (index into `state.apps`).
    pub app_selected: usize,
    /// Draw URLs as plain text instead of OSC 8 hyperlinks.
    pub no_hyperlinks: bool,
    exit: bool,
}

/// `Debug` for the instance source so [`App`] can stay `#[derive(Debug)]`.
#[derive(Clone)]
struct InstanceHandle(Arc<dyn InstanceSource>);

impl std::fmt::Debug for InstanceHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("InstanceSource")
    }
}

/// `Debug` for the store handle so [`App`] can stay `#[derive(Debug)]`.
#[derive(Clone)]
struct StoreHandle(Arc<dyn AttachmentStore>);

impl std::fmt::Debug for StoreHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AttachmentStore")
    }
}

/// Every image on a ticket's comments, oldest first.
pub(crate) fn ticket_images(ticket: &chillax_core::Ticket) -> Vec<&Attachment> {
    ticket
        .comments
        .iter()
        .flat_map(|c| c.attachments.iter())
        .collect()
}

/// A path as typed or dropped into the terminal: trimmed, quotes and `\ ` escapes removed.
fn clean_path(raw: &str) -> String {
    let t = raw.trim().trim_matches(['\'', '"']);
    let mut out = String::new();
    let mut chars = t.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn expand_home(path: &str) -> std::path::PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => std::path::Path::new(&home).join(rest),
        _ => std::path::PathBuf::from(path),
    }
}

/// A web address that is safe to hand to the system opener: http or https only, no
/// control characters, and nothing an opener could read as an option. Not limited to
/// localhost, because the project switcher opens 127.0.0.1 addresses and reported apps
/// are validated elsewhere.
pub(crate) fn browser_url(raw: &str) -> Option<String> {
    let url = raw.trim();
    let lower = url.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("http://")
        .or_else(|| lower.strip_prefix("https://"))?;
    (!rest.is_empty() && !url.starts_with('-') && !url.chars().any(|c| c.is_control()))
        .then(|| url.to_string())
}

/// The program and arguments that open `target` on `os` (a `std::env::consts::OS` value).
pub(crate) fn opener_command(os: &str, target: &str) -> (&'static str, Vec<String>) {
    match os {
        "macos" => ("open", vec![target.to_string()]),
        // The empty string is `start`'s window title; without it a quoted URL is taken as one.
        "windows" => (
            "cmd",
            ["/c", "start", "", target].map(String::from).to_vec(),
        ),
        _ => ("xdg-open", vec![target.to_string()]),
    }
}

/// Open a file or web address with the system viewer without waiting for it or blocking
/// the loop. Anything that looks like a URL must pass `browser_url`.
fn open_in_viewer(path: std::path::PathBuf) {
    let raw = path.to_string_lossy().into_owned();
    let target = if raw.contains("://") {
        match browser_url(&raw) {
            Some(url) => url,
            None => {
                tracing::warn!("refusing to open a non-web address");
                return;
            }
        }
    } else {
        raw
    };
    tokio::task::spawn_blocking(move || {
        let (program, args) = opener_command(std::env::consts::OS, &target);
        match std::process::Command::new(program)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                let _ = child.wait();
            }
            Err(err) => tracing::warn!(%err, target = %target, "cannot open"),
        }
    });
}

impl App {
    pub fn new(state: State, roles: Vec<Role>) -> Self {
        Self {
            state,
            roles,
            ..Self::default()
        }
    }

    pub fn with_web_url(mut self, url: impl Into<String>) -> Self {
        self.web_url = Some(url.into());
        self
    }

    /// List the running projects in the switcher (`O`, see keys.rs).
    pub fn with_instances(mut self, source: Arc<dyn InstanceSource>) -> Self {
        self.instances = Some(InstanceHandle(source));
        self
    }

    /// The root directory of this TUI's project, so the switcher can mark it.
    pub fn with_project_root(mut self, root: impl Into<String>) -> Self {
        self.project_root = Some(root.into());
        self
    }

    /// Instance list work queued by the switcher, for the run loop.
    pub fn take_projects_request(&mut self) -> Option<Arc<dyn InstanceSource>> {
        if std::mem::take(&mut self.projects_request) {
            self.instances.as_ref().map(|i| i.0.clone())
        } else {
            None
        }
    }

    /// The registry's answer for the open switcher.
    pub fn on_projects(&mut self, items: Vec<InstanceInfo>) {
        if let Mode::Projects(list) = &mut self.mode {
            list.loaded(items);
        }
    }

    /// True while something slow is loading (backends, models, a hire step, a working chat
    /// target), so the run loop only ticks the spinner when it has something to animate.
    pub fn is_busy(&self) -> bool {
        if matches!(&self.mode, Mode::Projects(list) if list.is_loading()) {
            return true;
        }
        let picker = match &self.mode {
            Mode::Hire(flow) => {
                if flow.busy.is_some() {
                    return true;
                }
                match &flow.step {
                    HireStep::Agent { picker, .. } => Some(picker),
                    _ => None,
                }
            }
            Mode::Settings(picker) | Mode::EmployeeAgent { picker, .. } => Some(picker),
            _ => None,
        };
        if picker.is_some_and(|p| p.is_loading()) {
            return true;
        }
        self.tab == Tab::Chat
            && self.state.employees.get(self.selected).is_some_and(|e| {
                matches!(
                    e.status,
                    chillax_core::EmployeeStatus::Working | chillax_core::EmployeeStatus::Starting
                )
            })
    }

    pub fn should_exit(&self) -> bool {
        self.exit
    }

    /// Record a live event (for the Events tab). State itself is refreshed from a
    /// snapshot by the run loop.
    pub fn push_event(&mut self, event: Event) {
        self.events.push_back(event);
        while self.events.len() > EVENT_HISTORY {
            self.events.pop_front();
        }
    }

    pub fn set_state(&mut self, state: State) {
        self.state = state;
        if self.usage_base.is_none() {
            self.usage_base = Some((self.state.usage.total.clone(), chrono::Utc::now()));
        }
        self.selected = self
            .selected
            .min(self.state.employees.len().saturating_sub(1));
        self.inbox_selected = self
            .inbox_selected
            .min(self.state.inbox.len().saturating_sub(1));
        if !self.state.sprint_view_exists(&self.sprint_view) {
            self.set_board_view(SprintView::Active);
        }
        if let Mode::Ticket { id } = &self.mode
            && self.state.ticket(id).is_none_or(|t| t.dropped.is_some())
        {
            self.mode = Mode::Normal;
        }
    }

    /// The non-empty Board columns in workflow order, each in ticket order.
    fn board_columns_filled(&self) -> Vec<Vec<&Ticket>> {
        let tickets = self.state.board_tickets(&self.sprint_view);
        self.state
            .workflow_states
            .iter()
            .map(|key| {
                tickets
                    .iter()
                    .filter(|t| &t.state == key)
                    .copied()
                    .collect::<Vec<_>>()
            })
            .filter(|column| !column.is_empty())
            .collect()
    }

    /// Board tickets in reading order: column by column, in workflow order.
    pub(crate) fn board_order(&self) -> Vec<&Ticket> {
        self.board_columns_filled().into_iter().flatten().collect()
    }

    /// The highlighted Board ticket, defaulting to the first one.
    pub(crate) fn board_ticket(&self) -> Option<&Ticket> {
        let order = self.board_order();
        self.board_selected
            .as_deref()
            .and_then(|id| order.iter().find(|t| t.id == id))
            .or(order.first())
            .copied()
    }

    /// Change what the Board shows and start from a clean selection and scroll.
    fn set_board_view(&mut self, view: SprintView) {
        if self.sprint_view == view {
            return;
        }
        self.sprint_view = view;
        self.board_selected = None;
        self.board_focus = false;
        self.board_scroll.clear();
    }

    /// Why the Board refuses changes in the current view.
    pub(crate) fn read_only_reason(&self) -> &'static str {
        match self.sprint_view {
            SprintView::Backlog => "The backlog is read-only",
            _ => "Past sprints are read-only",
        }
    }

    fn move_board_selection(&mut self, down: bool) {
        let order = self.board_order();
        let current = self
            .board_ticket()
            .and_then(|t| order.iter().position(|o| o.id == t.id))
            .unwrap_or(0);
        let next = if down {
            (current + 1).min(order.len().saturating_sub(1))
        } else {
            current.saturating_sub(1)
        };
        let id = order.get(next).map(|t| t.id.clone());
        self.select_board(id);
    }

    fn select_board(&mut self, id: Option<String>) {
        self.board_selected = id;
        self.reveal_board_selection();
    }

    /// The selected ticket's column index (among non-empty columns) and row in it.
    fn board_position(&self, columns: &[Vec<&Ticket>]) -> Option<(usize, usize)> {
        let id = &self.board_ticket()?.id;
        columns
            .iter()
            .enumerate()
            .find_map(|(c, column)| column.iter().position(|t| &t.id == id).map(|r| (c, r)))
    }

    /// Select the ticket at `row` of non-empty column `col`, clamped to its last row.
    fn select_board_cell(&mut self, col: usize, row: usize) {
        let columns = self.board_columns_filled();
        let id = columns
            .get(col)
            .and_then(|c| c.get(row.min(c.len().saturating_sub(1))))
            .map(|t| t.id.clone());
        if id.is_some() {
            self.select_board(id);
        }
    }

    /// One non-empty column left or right, keeping the row; a no-op at either end.
    fn move_board_column(&mut self, right: bool) {
        let columns = self.board_columns_filled();
        let Some((col, row)) = self.board_position(&columns) else {
            return;
        };
        let target = if right { col + 1 } else { col.wrapping_sub(1) };
        if target < columns.len() {
            self.select_board_cell(target, row);
        }
    }

    /// The first or last non-empty column, keeping the row.
    fn jump_board_column(&mut self, last: bool) {
        let columns = self.board_columns_filled();
        let Some((_, row)) = self.board_position(&columns) else {
            return;
        };
        let target = if last { columns.len() - 1 } else { 0 };
        self.select_board_cell(target, row);
    }

    /// The first or last ticket of the selected column.
    fn move_board_edge(&mut self, last: bool) {
        let columns = self.board_columns_filled();
        let Some((col, _)) = self.board_position(&columns) else {
            return;
        };
        self.select_board_cell(col, if last { usize::MAX } else { 0 });
    }

    /// About a screenful of cards up or down within the column, stopping at its ends.
    fn move_board_page(&mut self, down: bool) {
        let columns = self.board_columns_filled();
        let Some((col, row)) = self.board_position(&columns) else {
            return;
        };
        let height = (self.board_view.load(std::sync::atomic::Ordering::Relaxed) & 0xffff) as usize;
        let page = if height == 0 { 5 } else { (height / 3).max(1) };
        let row = if down {
            row.saturating_add(page)
        } else {
            row.saturating_sub(page)
        };
        self.select_board_cell(col, row);
    }

    /// Scroll the selected ticket's column just far enough to show its card.
    fn reveal_board_selection(&mut self) {
        let view = self.board_view.load(std::sync::atomic::Ordering::Relaxed);
        let (width, height) = ((view >> 16) as u16, (view & 0xffff) as usize);
        let Some(ticket) = self.board_ticket() else {
            return;
        };
        let Some(col) = self
            .state
            .workflow_states
            .iter()
            .position(|s| *s == ticket.state)
        else {
            return;
        };
        let (lines, card) = self.board_column(&ticket.state, width);
        let (Some((start, end)), true) = (card, height > 0) else {
            return;
        };
        let max = lines.len().saturating_sub(height);
        if self.board_scroll.len() <= col {
            self.board_scroll.resize(col + 1, 0);
        }
        let top = (self.board_scroll[col] as usize).min(max);
        let top = if start < top {
            start
        } else if end > top + height {
            end - height
        } else {
            top
        };
        self.board_scroll[col] = top as u16;
    }

    /// Whether the Assign key is offered: there is someone to assign to.
    pub(crate) fn can_assign(&self) -> bool {
        !self.state.employees.is_empty()
    }

    /// Whether the Blockers key is offered: another ticket could block this one.
    pub(crate) fn can_block(&self, id: &str) -> bool {
        self.state
            .tickets
            .iter()
            .any(|t| t.id != id && t.dropped.is_none())
    }

    /// Whether the Plan-next-sprint key is offered: no sprint waits or runs, and the
    /// project manager has not been asked and left unbriefed.
    pub(crate) fn can_plan(&self) -> bool {
        self.state.sprint_in_flight().is_none()
            && self.state.planning.as_ref().is_none_or(|p| p.briefed)
    }

    /// Whether the Dashboard has a next step to offer: a sprint was released, or work waits.
    pub(crate) fn can_start_sprint(&self) -> bool {
        self.state
            .sprints
            .iter()
            .any(|s| s.status == SprintStatus::Released)
            || !chillax_core::planning::candidates(&self.state).is_empty()
    }

    /// The employee who owns the first workflow state (the project manager), if hired.
    fn planner_index(&self) -> Option<usize> {
        let first = self.state.workflow_states.first()?;
        self.state.employees.iter().position(|e| {
            self.roles
                .iter()
                .find(|r| r.id == e.employee.role)
                .map_or(e.employee.role == "project-manager", |r| {
                    r.owns_states.contains(first)
                })
        })
    }

    /// The active sprint a stakeholder may close, if any.
    pub(crate) fn closable_sprint(&self) -> Option<&chillax_core::Sprint> {
        self.state
            .sprints
            .iter()
            .find(|s| s.status == SprintStatus::Active)
    }

    fn epic_choices(&self) -> Vec<(String, String)> {
        self.state
            .epics
            .iter()
            .map(|e| (e.id.clone(), e.title.clone()))
            .collect()
    }

    /// Keys in the ticket detail panel, resolved through the ticket key table: a number
    /// moves the ticket to that state; letters open the comment, assign, blockers, edit and
    /// drop dialogs when they apply.
    fn ticket_key(&mut self, key: KeyEvent, id: String) -> Vec<Command> {
        self.status = None;
        self.toast = None;
        let tables = [keys::ticket_table()];
        let act = match keys::lookup(self, &tables, Some(&id), key.code) {
            Lookup::Run(act) => act,
            Lookup::Refused(why) => {
                if !why.is_empty() {
                    self.status = Some(why);
                }
                self.mode = Mode::Ticket { id };
                return Vec::new();
            }
            Lookup::Unbound => {
                self.mode = Mode::Ticket { id };
                return Vec::new();
            }
        };
        let Some(ticket) = self.state.ticket(&id) else {
            return Vec::new();
        };
        let mut commands = Vec::new();
        match act {
            Act::CloseTicket => return Vec::new(),
            Act::Scroll => {
                let max = self.ticket_max.load(std::sync::atomic::Ordering::Relaxed);
                let page = self
                    .ticket_view
                    .load(std::sync::atomic::Ordering::Relaxed)
                    .saturating_sub(1)
                    .max(1);
                let cur = self.ticket_scroll.min(max);
                self.ticket_scroll = match key.code {
                    KeyCode::Up => cur.saturating_sub(1),
                    KeyCode::Down => cur.saturating_add(1),
                    KeyCode::PageUp => cur.saturating_sub(page),
                    KeyCode::PageDown => cur.saturating_add(page),
                    KeyCode::Home | KeyCode::Char('g') => 0,
                    _ => max,
                }
                .min(max);
                self.ticket_section = None;
            }
            Act::NextSection | Act::PrevSection => {
                let marks = self
                    .ticket_marks
                    .lock()
                    .map(|m| m.clone())
                    .unwrap_or_default();
                let max = self.ticket_max.load(std::sync::atomic::Ordering::Relaxed);
                let cur = self.ticket_scroll.min(max);
                let n = marks.len();
                if n > 0 {
                    let next = act == Act::NextSection;
                    let at = match (self.ticket_section, next) {
                        (Some(i), true) => (i + 1) % n,
                        (Some(i), false) => (i + n - 1) % n,
                        (None, true) => marks.iter().position(|&r| r > cur).unwrap_or(0),
                        (None, false) => marks.iter().rposition(|&r| r < cur).unwrap_or(n - 1),
                    };
                    self.ticket_section = Some(at);
                    self.ticket_scroll = marks[at].min(max);
                }
            }
            Act::Edit => {
                self.mode = Mode::Form(TicketForm::edit(ticket).with_epics(self.epic_choices()));
                return Vec::new();
            }
            Act::Comment => {
                self.mode = Mode::Comment {
                    id,
                    text: String::new(),
                    files: String::new(),
                    field: 0,
                };
                return Vec::new();
            }
            Act::OpenImage => {
                let images = ticket_images(ticket);
                if let Some(at) = images
                    .len()
                    .checked_sub(1)
                    .map(|_| self.attach_cursor % images.len())
                {
                    let image = images[at].clone();
                    match self.attachments.as_ref().map(|s| s.0.path(&image.id)) {
                        Some(Ok(path)) => {
                            self.status = Some(format!(
                                "Opening {} ({}/{})",
                                image.name,
                                at + 1,
                                images.len()
                            ));
                            self.open_requests.push(path);
                            self.attach_cursor = at + 1;
                        }
                        Some(Err(err)) => self.status = Some(format!("Cannot open: {err}")),
                        None => self.status = Some("Attachments are not available".into()),
                    }
                }
            }
            Act::Assign => {
                let choice = ticket
                    .assignee
                    .as_deref()
                    .and_then(|a| self.state.employees.iter().position(|e| e.employee.id == a))
                    .map_or(0, |at| at + 1);
                self.mode = Mode::Assign { id, choice };
                return Vec::new();
            }
            Act::Blockers => {
                let text = ticket.blocked_by.join(" ");
                self.mode = Mode::Blockers { id, text };
                return Vec::new();
            }
            Act::Drop => {
                self.mode = Mode::Drop {
                    id,
                    reason: String::new(),
                    confirm: false,
                };
                return Vec::new();
            }
            Act::Prioritize => {
                commands.push(Command::PrioritizeTicket {
                    id: id.clone(),
                    urgent: !ticket.urgent,
                });
            }
            Act::MoveTicket => {
                if let KeyCode::Char(c) = key.code {
                    let moves = self.state.stakeholder_moves(ticket);
                    if let Some(to) = moves.get(c as usize - '1' as usize) {
                        commands.push(Command::TransitionTicket {
                            id: id.clone(),
                            to: (*to).to_string(),
                        });
                    }
                }
            }
            Act::Copy => {
                let text = self.ticket_text(&id);
                if let Some(text) = text {
                    self.copy_text(&text);
                }
            }
            Act::ToggleMouse => self.toggle_mouse(),
            _ => {}
        }
        self.mode = Mode::Ticket { id };
        commands
    }

    /// Typing a comment: Enter sends, Alt+Enter adds a line, Tab switches between the text
    /// and the image paths, Esc goes back to the ticket.
    fn comment_key(
        &mut self,
        key: KeyEvent,
        id: String,
        mut text: String,
        mut files: String,
        mut field: usize,
    ) -> Vec<Command> {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Ticket { id };
                return Vec::new();
            }
            KeyCode::Tab | KeyCode::BackTab => field = 1 - field,
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) && field == 0 => {
                text.push('\n')
            }
            KeyCode::Enter if !text.trim().is_empty() || !files.trim().is_empty() => {
                match self.import_files(&files) {
                    Ok(attachments) => {
                        self.mode = Mode::Ticket { id: id.clone() };
                        return vec![Command::CommentTicket {
                            id,
                            body: text.trim().to_string(),
                            external_id: None,
                            attachments,
                        }];
                    }
                    Err(why) => self.status = Some(why),
                }
            }
            KeyCode::Backspace => {
                if field == 0 { &mut text } else { &mut files }.pop();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if field == 0 { &mut text } else { &mut files }.push(c)
            }
            _ => {}
        }
        self.mode = Mode::Comment {
            id,
            text,
            files,
            field,
        };
        Vec::new()
    }

    /// Copy the named image files into the attachment store. Paths are comma separated;
    /// quotes, backslash escapes (terminal drag and drop) and `~` are understood.
    fn import_files(&self, files: &str) -> Result<Vec<Attachment>, String> {
        let paths: Vec<String> = files
            .split(',')
            .map(clean_path)
            .filter(|p| !p.is_empty())
            .collect();
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let store = self
            .attachments
            .as_ref()
            .ok_or("Attachments are not available")?;
        paths
            .iter()
            .map(|p| {
                store
                    .0
                    .import(&expand_home(p), None)
                    .map_err(|e| format!("{p}: {e}"))
            })
            .collect()
    }

    /// Files for the run loop to open in the system viewer.
    pub fn take_open_requests(&mut self) -> Vec<std::path::PathBuf> {
        std::mem::take(&mut self.open_requests)
    }

    /// Draw links as plain text (a terminal that prints OSC 8 sequences instead of
    /// hiding them).
    pub fn with_plain_links(mut self) -> Self {
        self.no_hyperlinks = true;
        self
    }

    pub fn with_attachments(mut self, store: Arc<dyn AttachmentStore>) -> Self {
        self.attachments = Some(StoreHandle(store));
        self
    }

    fn assign_key(&mut self, key: KeyEvent, id: String, mut choice: usize) -> Vec<Command> {
        let last = self.state.employees.len();
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Ticket { id };
                return Vec::new();
            }
            KeyCode::Up | KeyCode::Char('k') => choice = choice.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => choice = (choice + 1).min(last),
            KeyCode::Enter => {
                let assignee = choice
                    .checked_sub(1)
                    .and_then(|at| self.state.employees.get(at))
                    .map(|e| e.employee.id.clone());
                self.mode = Mode::Ticket { id: id.clone() };
                return vec![Command::AssignTicket { id, assignee }];
            }
            _ => {}
        }
        self.mode = Mode::Assign { id, choice };
        Vec::new()
    }

    fn blockers_key(&mut self, key: KeyEvent, id: String, mut text: String) -> Vec<Command> {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Ticket { id };
                return Vec::new();
            }
            KeyCode::Enter => {
                let blocked_by = text
                    .split([' ', ','])
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(|t| t.to_uppercase())
                    .collect();
                self.mode = Mode::Ticket { id: id.clone() };
                return vec![Command::SetBlockers { id, blocked_by }];
            }
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => text.push(c),
            _ => {}
        }
        self.mode = Mode::Blockers { id, text };
        Vec::new()
    }

    /// The drop dialog: a reason, then y to confirm. Anything else but y goes back.
    fn drop_key(
        &mut self,
        key: KeyEvent,
        id: String,
        mut reason: String,
        confirm: bool,
    ) -> Vec<Command> {
        if confirm {
            if matches!(key.code, KeyCode::Char('y' | 'Y')) {
                return vec![Command::DropTicket {
                    id,
                    reason: reason.trim().to_string(),
                }];
            }
            self.mode = Mode::Ticket { id };
            return Vec::new();
        }
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Ticket { id };
                return Vec::new();
            }
            KeyCode::Enter if !reason.trim().is_empty() => {
                self.mode = Mode::Drop {
                    id,
                    reason,
                    confirm: true,
                };
                return Vec::new();
            }
            KeyCode::Backspace => {
                reason.pop();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => reason.push(c),
            _ => {}
        }
        self.mode = Mode::Drop {
            id,
            reason,
            confirm: false,
        };
        Vec::new()
    }

    /// Keys in the request editor: Enter adds a line, Ctrl+S sends, Tab toggles
    /// "start now", Esc cancels.
    fn request_key(
        &mut self,
        key: KeyEvent,
        mut text: Editor,
        mut start_now: bool,
    ) -> Vec<Command> {
        match key.code {
            KeyCode::Esc => return Vec::new(),
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if text.is_blank() {
                    self.status = Some("Describe what you need first".into());
                } else {
                    return vec![Command::SubmitRequest {
                        text: text.text(),
                        start_now,
                    }];
                }
            }
            KeyCode::Tab | KeyCode::BackTab => start_now = !start_now,
            KeyCode::Enter => text.insert_char('\n'),
            _ => {
                text.handle_key(key);
            }
        }
        self.mode = Mode::Request { text, start_now };
        Vec::new()
    }

    fn plan_key(&mut self, key: KeyEvent, mut text: String) -> Vec<Command> {
        match key.code {
            KeyCode::Esc => return Vec::new(),
            KeyCode::Enter => {
                let note = Some(text.trim().to_string()).filter(|n| !n.is_empty());
                // The engine refuses while a sprint is in flight (it may have started since
                // the dialog opened); say why and stay where we are.
                if let Some(sp) = self.state.sprint_in_flight() {
                    self.status = Some(format!(
                        "Sprint {} is still running: finish or close it first",
                        sp.id
                    ));
                    return Vec::new();
                }
                // The conversation about the next release happens with the PM.
                match self.planner_index() {
                    Some(i) => {
                        self.selected = i;
                        self.switch_tab(Tab::Chat.index());
                    }
                    None => {
                        self.status = Some("Hire a Project Manager to plan the sprint".into());
                    }
                }
                return vec![Command::RequestSprintPlanning { note }];
            }
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => text.push(c),
            _ => {}
        }
        self.mode = Mode::PlanSprint { text };
        Vec::new()
    }

    /// Keys in the work item form. A saved edit returns to the ticket's panel.
    fn form_key(&mut self, key: KeyEvent, mut form: TicketForm) -> Vec<Command> {
        let back = match &form.target {
            FormTarget::Edit { id } => Mode::Ticket { id: id.clone() },
            FormTarget::Create => Mode::Normal,
        };
        match form.handle_key(key) {
            FormOutcome::Open => {
                self.mode = Mode::Form(form);
                Vec::new()
            }
            FormOutcome::Cancel => {
                self.mode = back;
                Vec::new()
            }
            FormOutcome::Save(command) => {
                self.mode = back;
                vec![command]
            }
        }
    }

    /// Furthest the log can scroll, as of the last draw.
    fn scroll_max(&self) -> u16 {
        self.scroll_max.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Text pasted into the terminal (bracketed paste): inserted verbatim into whatever is
    /// being typed. Line endings become `\n`; single-line fields get the lines joined by
    /// spaces so a paste never acts as Enter.
    pub fn handle_paste(&mut self, pasted: &str) {
        let text = pasted.replace("\r\n", "\n").replace('\r', "\n");
        let flat = || text.replace('\n', " ");
        match &mut self.mode {
            Mode::Compose => self.draft.insert_str(&text),
            Mode::Request { text: editor, .. } => editor.insert_str(&text),
            Mode::Comment {
                text: t, field: 0, ..
            }
            | Mode::PlanSprint { text: t }
            | Mode::RejectRelease { text: t, .. } => t.push_str(&text),
            Mode::Answer { text: t, .. }
            | Mode::Blockers { text: t, .. }
            | Mode::Comment {
                files: t, field: 1, ..
            } => t.push_str(&flat()),
            Mode::Drop {
                reason,
                confirm: false,
                ..
            } => reason.push_str(&flat()),
            Mode::Form(form) => form.paste(&text),
            _ => {}
        }
    }

    /// Where the terminal cursor belongs on a screen of `area`, if text is being typed
    /// into an editor.
    pub fn cursor_position(&self, area: Rect) -> Option<Position> {
        view::cursor_position(self, area)
    }

    fn selected_employee(&self) -> Option<&str> {
        self.state
            .employees
            .get(self.selected)
            .map(|e| e.employee.id.as_str())
    }

    /// Handle one key press; returns commands to send to the company.
    pub fn handle_key_event(&mut self, key: KeyEvent) -> Vec<Command> {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            // A second Ctrl-C while the prompt is open confirms the quit.
            if self.mode == Mode::ConfirmQuit {
                self.exit = true;
            } else {
                self.mode = Mode::ConfirmQuit;
            }
            return Vec::new();
        }
        match std::mem::take(&mut self.mode) {
            Mode::Normal => self.normal_key(key),
            Mode::Compose => self.compose_key(key),
            Mode::Hire(flow) => self.hire_key(key, flow),
            Mode::Answer { id, text } => self.answer_key(key, id, text),
            Mode::Ticket { id } => self.ticket_key(key, id),
            Mode::Form(form) => self.form_key(key, form),
            Mode::Settings(mut picker) => match picker.key(key, &mut self.recruit_requests) {
                Picked::Pending => {
                    self.mode = Mode::Settings(picker);
                    Vec::new()
                }
                Picked::Cancelled => Vec::new(),
                // Without an agent list there is nothing to change.
                Picked::Done { backend: None, .. } => Vec::new(),
                Picked::Done {
                    backend,
                    model,
                    effort,
                } => {
                    vec![Command::SetHiringDefaults {
                        backend,
                        model,
                        effort,
                    }]
                }
            },
            Mode::Projects(mut list) => match list.key(key) {
                ProjectPick::Pending => {
                    self.mode = Mode::Projects(list);
                    Vec::new()
                }
                ProjectPick::Cancelled => Vec::new(),
                ProjectPick::Open(url) => {
                    self.status = Some(format!("Opening {url}"));
                    self.open_requests.push(std::path::PathBuf::from(url));
                    Vec::new()
                }
            },
            Mode::EmployeeAgent {
                employee,
                mut picker,
            } => match picker.key(key, &mut self.recruit_requests) {
                Picked::Pending => {
                    self.mode = Mode::EmployeeAgent { employee, picker };
                    Vec::new()
                }
                Picked::Cancelled | Picked::Done { backend: None, .. } => Vec::new(),
                Picked::Done {
                    backend,
                    model,
                    effort,
                } => vec![Command::SetEmployeeAgent {
                    employee,
                    when_idle: true,
                    backend,
                    model,
                    effort,
                }],
            },
            Mode::ConfirmLetGo { employee } => {
                if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    vec![Command::LetGo { employee }]
                } else {
                    Vec::new()
                }
            }
            Mode::Comment {
                id,
                text,
                files,
                field,
            } => self.comment_key(key, id, text, files, field),
            Mode::Assign { id, choice } => self.assign_key(key, id, choice),
            Mode::Blockers { id, text } => self.blockers_key(key, id, text),
            Mode::Drop {
                id,
                reason,
                confirm,
            } => self.drop_key(key, id, reason, confirm),
            Mode::PlanSprint { text } => self.plan_key(key, text),
            Mode::Request { text, start_now } => self.request_key(key, text, start_now),
            Mode::Requests => {
                let max = self.requests_max.load(std::sync::atomic::Ordering::Relaxed);
                let page = 10;
                let scrolled = match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        Some(self.requests_scroll.saturating_sub(1))
                    }
                    KeyCode::Down | KeyCode::Char('j') => Some(self.requests_scroll + 1),
                    KeyCode::PageUp => Some(self.requests_scroll.saturating_sub(page)),
                    KeyCode::PageDown => Some(self.requests_scroll + page),
                    _ => None,
                };
                // Scrolling keeps the popup open; any other key closes it.
                if let Some(to) = scrolled {
                    if max > 0 {
                        self.requests_scroll = to.min(max);
                    }
                    self.mode = Mode::Requests;
                }
                Vec::new()
            }
            Mode::ConfirmClearChat { employee } => {
                if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    vec![Command::ClearChat { employee }]
                } else {
                    Vec::new()
                }
            }
            Mode::ConfirmCloseSprint { id } => {
                if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    vec![Command::CloseSprint { id }]
                } else {
                    Vec::new()
                }
            }
            Mode::Release(edit) => self.release_key(key, edit),
            Mode::RejectRelease { id, text } => self.reject_key(key, id, text),
            Mode::ConfirmQuit => {
                if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    self.exit = true;
                }
                Vec::new()
            }
            Mode::ConfirmForce => {
                if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    vec![Command::OpenReleaseGate { force: true }]
                } else {
                    Vec::new()
                }
            }
        }
    }

    fn normal_key(&mut self, key: KeyEvent) -> Vec<Command> {
        self.status = None;
        self.toast = None;
        let tables = [keys::tab_table(self), keys::global_table()];
        match keys::lookup(self, &tables, None, key.code) {
            Lookup::Run(act) => self.run_act(act, key.code),
            Lookup::Refused(why) => {
                if !why.is_empty() {
                    self.status = Some(why);
                }
                Vec::new()
            }
            Lookup::Unbound => Vec::new(),
        }
    }

    fn switch_tab(&mut self, index: usize) {
        self.tab = Tab::ALL[index];
        self.scroll = 0;
        self.board_focus = false;
        self.team_focus = false;
    }

    /// Do what a key table entry names. Availability was already checked by the lookup.
    fn run_act(&mut self, act: Act, code: KeyCode) -> Vec<Command> {
        let up = matches!(code, KeyCode::Up | KeyCode::Char('k'));
        let employees = self.state.employees.len();
        if self.tab == Tab::Inbox
            && !matches!(
                act,
                Act::Quit
                    | Act::NextTab
                    | Act::PrevTab
                    | Act::GotoTab
                    | Act::Pause
                    | Act::Resume
                    | Act::Projects
                    | Act::Copy
                    | Act::ToggleMouse
            )
        {
            return self.inbox_act(act, code);
        }
        match act {
            Act::Quit => self.mode = Mode::ConfirmQuit,
            Act::NextTab => self.switch_tab((self.tab.index() + 1) % Tab::ALL.len()),
            Act::PrevTab => {
                self.switch_tab((self.tab.index() + Tab::ALL.len() - 1) % Tab::ALL.len());
            }
            Act::GotoTab => {
                // `0` is the tenth tab; 1-9 are the first nine.
                let index = match code {
                    KeyCode::Char('0') => Some(9),
                    KeyCode::Char(c @ '1'..='9') => Some(c as usize - '1' as usize),
                    _ => None,
                };
                if let Some(i) = index.filter(|i| *i < Tab::ALL.len()) {
                    self.switch_tab(i);
                }
            }
            // The kill switch works from every view.
            Act::Pause => return vec![Command::PauseDispatch],
            Act::Resume => return vec![Command::ResumeDispatch],
            Act::Hire => self.open_hire(),
            Act::Copy => self.copy_focused(),
            Act::ToggleMouse => self.toggle_mouse(),
            Act::Projects => {
                self.projects_request = true;
                self.mode = Mode::Projects(ProjectList::new(self.project_root.clone()));
            }
            Act::ChangeAgent => {
                self.recruit_requests.push(RecruitRequest::Backends);
                self.mode = Mode::Settings(Box::new(AgentPicker::new(self.state.hiring.clone())));
            }
            Act::ClearActivity => return vec![Command::ClearActivity],
            // Refresh: close idle agents, forget waits and backoff, look again now.
            Act::Refresh => return vec![Command::Kick],
            Act::AgentsMore | Act::AgentsFewer => {
                if let Some(l) = self.state.agent_limit {
                    let n = if act == Act::AgentsMore {
                        l.limit + 1
                    } else {
                        l.limit.saturating_sub(1)
                    };
                    return vec![Command::SetMaxConcurrentAgents { n }];
                }
            }
            Act::Fire => {
                if let Some(id) = self.selected_employee() {
                    self.mode = Mode::ConfirmLetGo {
                        employee: id.to_string(),
                    };
                }
            }
            Act::Duplicate => {
                if let Some(view) = self.state.employees.get(self.selected) {
                    return vec![view.employee.duplicate_command()];
                }
            }
            Act::SetAgent => {
                if let Some(view) = self.state.employees.get(self.selected) {
                    let current = chillax_core::HiringDefaults {
                        backend: Some(view.employee.backend.clone()),
                        model: view.employee.model.clone(),
                        effort: view.employee.effort.clone(),
                    };
                    self.mode = Mode::EmployeeAgent {
                        employee: view.employee.id.clone(),
                        picker: Box::new(AgentPicker::new(current)),
                    };
                    self.recruit_requests.push(RecruitRequest::Backends);
                }
            }
            Act::Stop => {
                if let Some(id) = self.selected_employee() {
                    return vec![Command::CancelTurn {
                        employee: id.to_string(),
                    }];
                }
            }
            Act::ClearChat => {
                if let Some(id) = self.selected_employee() {
                    self.mode = Mode::ConfirmClearChat {
                        employee: id.to_string(),
                    };
                }
            }
            Act::OpenChat => self.switch_tab(Tab::Chat.index()),
            Act::Type => self.mode = Mode::Compose,
            Act::ToggleCompany => {
                self.company_view = !self.company_view;
                self.scroll = 0;
            }
            Act::UsageResetBaseline => {
                self.usage_base = Some((self.state.usage.total.clone(), chrono::Utc::now()));
            }
            Act::UsageBucket => self.usage_hourly = !self.usage_hourly,
            Act::UsageRefresh => return vec![Command::RefreshUsage],
            Act::UsageScroll => {
                self.scroll = if up {
                    self.scroll.saturating_sub(1)
                } else {
                    self.scroll.saturating_add(1).min(self.scroll_max())
                };
            }
            Act::Scroll => {
                self.scroll = if matches!(code, KeyCode::PageUp) {
                    self.scroll.saturating_add(10).min(self.scroll_max())
                } else {
                    self.scroll.saturating_sub(10)
                };
            }
            Act::Move if self.tab == Tab::Board => self.move_board_selection(!up),
            Act::NextSprintView => {
                let v = self.state.next_sprint_view(&self.sprint_view);
                self.set_board_view(v);
            }
            Act::PrevSprintView => {
                let v = self.state.prev_sprint_view(&self.sprint_view);
                self.set_board_view(v);
            }
            Act::FocusBoard => self.board_focus = true,
            Act::UnfocusBoard => self.board_focus = false,
            Act::FocusTeam => self.team_focus = true,
            Act::UnfocusTeam => self.team_focus = false,
            Act::MoveColumn => {
                self.move_board_column(matches!(code, KeyCode::Right | KeyCode::Char('l')))
            }
            Act::MoveEdge => {
                self.move_board_edge(matches!(code, KeyCode::End | KeyCode::Char('G')))
            }
            Act::MovePage => self.move_board_page(matches!(code, KeyCode::PageDown)),
            Act::MoveColumnEdge => self.jump_board_column(matches!(code, KeyCode::Char(']'))),
            Act::Move if self.tab == Tab::Team && self.grid => {
                let (cols, _) = view::grid_dims(employees);
                let next = if up {
                    self.selected.checked_sub(cols)
                } else {
                    Some(self.selected + cols).filter(|n| *n < employees)
                };
                self.selected = next.unwrap_or(self.selected);
                self.scroll = 0;
            }
            Act::MoveSide => {
                let right = code == KeyCode::Right;
                self.selected = if right {
                    (self.selected + 1).min(employees.saturating_sub(1))
                } else {
                    self.selected.saturating_sub(1)
                };
                self.scroll = 0;
            }
            Act::ToggleGrid => {
                self.grid = !self.grid;
                self.team_focus = false;
            }
            Act::OpenLive => self.switch_tab(Tab::Live.index()),
            Act::ToggleOutput => self.expanded = !self.expanded,
            Act::Move => {
                self.selected = if up {
                    self.selected.saturating_sub(1)
                } else {
                    (self.selected + 1).min(employees.saturating_sub(1))
                };
                self.scroll = 0;
            }
            Act::Prioritize => {
                if let Some(t) = self.board_ticket() {
                    return vec![Command::PrioritizeTicket {
                        id: t.id.clone(),
                        urgent: !t.urgent,
                    }];
                }
            }
            Act::OpenTicket => {
                if let Some(ticket) = self.board_ticket() {
                    let id = ticket.id.clone();
                    self.board_selected = Some(id.clone());
                    self.ticket_scroll = 0;
                    self.ticket_section = None;
                    self.mode = Mode::Ticket { id };
                }
            }
            Act::NewItem => {
                self.mode = Mode::Form(TicketForm::create().with_epics(self.epic_choices()));
            }
            Act::NewRequest => {
                self.mode = Mode::Request {
                    text: Editor::new(),
                    start_now: false,
                };
            }
            Act::Requests => {
                self.requests_scroll = 0;
                self.mode = Mode::Requests;
            }
            Act::PlanNext => {
                self.mode = Mode::PlanSprint {
                    text: String::new(),
                };
            }
            Act::CloseSprint => {
                if let Some(sp) = self.closable_sprint() {
                    self.mode = Mode::ConfirmCloseSprint { id: sp.id.clone() };
                }
            }
            Act::ApproveSprint => return self.gate(true),
            Act::RejectSprint => return self.gate(false),
            Act::Promote => return vec![Command::PromoteToQa],
            Act::PrepareRelease => return vec![Command::OpenReleaseGate { force: false }],
            Act::ForceRelease => self.mode = Mode::ConfirmForce,
            Act::ApproveRelease => {
                if let Some(id) = self.gated_release() {
                    self.open_release_editor(&id);
                }
            }
            Act::RejectRelease => {
                if let Some(id) = self.rejectable_release() {
                    self.mode = Mode::RejectRelease {
                        id,
                        text: String::new(),
                    };
                }
            }
            Act::Retry => return self.retry_command().into_iter().collect(),
            Act::SelectApp => {
                let last = self.state.apps.len().saturating_sub(1);
                self.app_selected = if up {
                    self.app_selected.saturating_sub(1)
                } else {
                    (self.app_selected + 1).min(last)
                };
            }
            Act::StartApp | Act::StopApp | Act::RestartApp => {
                if let Some(app) = self.selected_app() {
                    let env = app.env.clone();
                    return vec![match act {
                        Act::StartApp => Command::StartApp { env },
                        Act::StopApp => Command::StopApp { env },
                        _ => Command::RestartApp { env },
                    }];
                }
            }
            Act::OpenDashApp => {
                if let Some(url) = self.first_running_url() {
                    self.status = Some(format!("Opening {url}"));
                    self.open_requests.push(std::path::PathBuf::from(url));
                }
            }
            Act::OpenApp => self.request_open_selected_app(),
            _ => {}
        }
        Vec::new()
    }

    /// Queue the selected app's address for the browser when it is running. Shared by the
    /// `o`/Enter key and mouse clicks; nothing is opened here, the run loop does that.
    pub(crate) fn request_open_selected_app(&mut self) {
        if let Some(url) = self
            .selected_app()
            .filter(|app| app.is_running())
            .map(|app| app.url.clone())
        {
            self.status = Some(format!("Opening {url}"));
            self.open_requests.push(std::path::PathBuf::from(url));
        }
    }

    /// Approve or reject the sprint waiting at the gate.
    fn gate(&mut self, approve: bool) -> Vec<Command> {
        let Some(sprint) = self.proposed_sprint() else {
            self.status = Some("No sprint is waiting for approval".into());
            return Vec::new();
        };
        let id = sprint.id.clone();
        if approve {
            vec![Command::ApproveSprint { id }]
        } else {
            vec![Command::RejectSprint { id, reason: None }]
        }
    }

    /// Inbox keys act on the selected item; the table already limited them to its type.
    fn inbox_act(&mut self, act: Act, code: KeyCode) -> Vec<Command> {
        if act == Act::Move {
            self.inbox_selected = if matches!(code, KeyCode::Up | KeyCode::Char('k')) {
                self.inbox_selected.saturating_sub(1)
            } else {
                (self.inbox_selected + 1).min(self.state.inbox.len().saturating_sub(1))
            };
            return Vec::new();
        }
        let Some(item) = self.state.inbox.get(self.inbox_selected).cloned() else {
            return Vec::new();
        };
        match (item, act) {
            (InboxItem::Permission { id, choices, .. }, Act::Choose) => {
                let at = match code {
                    KeyCode::Char(c) => c as usize - '1' as usize,
                    _ => return Vec::new(),
                };
                match choices.get(at) {
                    Some(choice) => vec![Command::ResolvePermission {
                        id,
                        choice: Some(choice.id.clone()),
                    }],
                    None => {
                        self.status = Some("No such choice".into());
                        Vec::new()
                    }
                }
            }
            (InboxItem::Permission { id, .. }, Act::CancelPermission) => {
                vec![Command::ResolvePermission { id, choice: None }]
            }
            (InboxItem::Question { id, .. }, Act::Answer) => {
                self.mode = Mode::Answer {
                    id,
                    text: String::new(),
                };
                Vec::new()
            }
            (InboxItem::ReleaseGate { release, .. }, Act::ApproveRelease) => {
                self.open_release_editor(&release);
                Vec::new()
            }
            (InboxItem::ReleaseGate { release, .. }, Act::RejectRelease) => {
                self.mode = Mode::RejectRelease {
                    id: release,
                    text: String::new(),
                };
                Vec::new()
            }
            (InboxItem::EnvAlert { env, step, .. }, Act::Retry) => {
                vec![Command::RetryEnv { env, step }]
            }
            (InboxItem::Alert { id, .. }, Act::Dismiss) => vec![Command::DismissAlert { id }],
            (InboxItem::SprintGate { sprint, .. }, Act::ApproveSprint) => {
                vec![Command::ApproveSprint { id: sprint }]
            }
            (InboxItem::SprintGate { sprint, .. }, Act::RejectSprint) => {
                vec![Command::RejectSprint {
                    id: sprint,
                    reason: None,
                }]
            }
            _ => Vec::new(),
        }
    }

    /// A release the stakeholder can reject: one at the gate, or an approved one that
    /// Chillax could not make.
    fn rejectable_release(&self) -> Option<String> {
        self.state
            .releases
            .iter()
            .find(|r| matches!(r.status, ReleaseStatus::Gated | ReleaseStatus::Approved))
            .map(|r| r.id.clone())
    }

    /// The release waiting at the `release` gate.
    fn gated_release(&self) -> Option<String> {
        self.state
            .releases
            .iter()
            .find(|r| r.status == ReleaseStatus::Gated)
            .map(|r| r.id.clone())
    }

    fn open_release_editor(&mut self, release: &str) {
        match self.state.release(release) {
            Some(r) => {
                self.mode = Mode::Release(Box::new(ReleaseEdit {
                    id: r.id.clone(),
                    version: r.version.clone(),
                    notes: r.notes.clone(),
                    field: 0,
                }));
            }
            None => self.status = Some(format!("Unknown release {release}")),
        }
    }

    /// The environment step to run again: the first failed alert in the inbox, else a
    /// failed deploy.
    fn retry_command(&self) -> Option<Command> {
        let alert = self.state.inbox.iter().find_map(|item| match item {
            InboxItem::EnvAlert { env, step, .. } => Some(Command::RetryEnv {
                env: env.clone(),
                step: *step,
            }),
            _ => None,
        });
        alert.or_else(|| {
            self.state
                .releases
                .last()
                .filter(|r| {
                    matches!(
                        r.status,
                        ReleaseStatus::DeployFailed | ReleaseStatus::Approved
                    )
                })
                .map(|_| Command::RetryEnv {
                    env: "prod".into(),
                    step: EnvStep::Deploy,
                })
        })
    }

    fn release_key(&mut self, key: KeyEvent, mut edit: Box<ReleaseEdit>) -> Vec<Command> {
        match key.code {
            KeyCode::Esc => return Vec::new(),
            KeyCode::Tab | KeyCode::BackTab => edit.field = 1 - edit.field,
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => {
                if edit.field == 1 {
                    edit.notes.push('\n');
                }
            }
            KeyCode::Enter => {
                if !edit.version.trim().is_empty() && !edit.notes.trim().is_empty() {
                    return vec![Command::ApproveRelease {
                        id: edit.id,
                        version: edit.version.trim().to_string(),
                        notes: edit.notes,
                    }];
                }
            }
            KeyCode::Backspace => {
                if edit.field == 0 {
                    edit.version.pop();
                } else {
                    edit.notes.pop();
                }
            }
            KeyCode::Char(c) => {
                if edit.field == 0 {
                    edit.version.push(c);
                } else {
                    edit.notes.push(c);
                }
            }
            _ => {}
        }
        self.mode = Mode::Release(edit);
        Vec::new()
    }

    fn reject_key(&mut self, key: KeyEvent, id: String, mut text: String) -> Vec<Command> {
        match key.code {
            KeyCode::Esc => return Vec::new(),
            KeyCode::Enter => {
                return vec![Command::RejectRelease { id, reason: text }];
            }
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Char(c) => text.push(c),
            _ => {}
        }
        self.mode = Mode::RejectRelease { id, text };
        Vec::new()
    }

    fn compose_key(&mut self, key: KeyEvent) -> Vec<Command> {
        if key.code == KeyCode::Esc {
            return Vec::new();
        }
        self.mode = Mode::Compose;
        if key.code == KeyCode::Enter && !key.modifiers.contains(KeyModifiers::ALT) {
            let text = self.draft.take();
            self.scroll = 0;
            if self.company_view {
                if !text.trim().is_empty() {
                    return vec![Command::PostCompanyChat { text, to: None }];
                }
            } else if let (Some(to), false) = (self.selected_employee(), text.trim().is_empty()) {
                return vec![Command::SendChat {
                    to: to.to_string(),
                    text,
                }];
            }
        } else {
            self.draft.handle_key(key);
        }
        Vec::new()
    }

    fn answer_key(&mut self, key: KeyEvent, id: String, mut text: String) -> Vec<Command> {
        match key.code {
            KeyCode::Esc => return Vec::new(),
            KeyCode::Enter if !text.trim().is_empty() => {
                return vec![Command::AnswerQuestion { id, answer: text }];
            }
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Char(c) => text.push(c),
            _ => {}
        }
        self.mode = Mode::Answer { id, text };
        Vec::new()
    }

    /// Run until the user quits: draw, then wait for a key, a company event, or a
    /// recruiter reply. `recruiter` is `None` when the persona catalog is disabled;
    /// `agents` lists the backends and models to hire on.
    pub async fn run(
        mut self,
        terminal: &mut DefaultTerminal,
        company: Arc<dyn CompanyHandle>,
        recruiter: Option<Arc<dyn Recruiter>>,
        agents: Option<Arc<dyn AgentDirectory>>,
    ) -> io::Result<()> {
        let mut bus = company.subscribe();
        if let Ok(recent) = company
            .events_since(self.state.last_seq.saturating_sub(200))
            .await
        {
            recent.into_iter().for_each(|e| self.push_event(e));
        }
        let mut keys = EventStream::new();
        let (projects_tx, mut project_replies) = mpsc::unbounded_channel();
        let (reply_tx, mut replies) = mpsc::unbounded_channel();
        let mut in_flight: Vec<JoinHandle<()>> = Vec::new();
        let mut dirty = true;
        let mut spin = tokio::time::interval(std::time::Duration::from_millis(100));
        spin.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        while !self.exit {
            self.take_open_requests()
                .into_iter()
                .for_each(open_in_viewer);
            if let Some(seq) = self.take_clipboard() {
                use std::io::Write;
                let backend = terminal.backend_mut();
                // A terminal that ignores OSC 52 is not an error; `m` is the fallback.
                let _ = backend
                    .write_all(seq.as_bytes())
                    .and_then(|()| backend.flush());
                dirty = true;
            }
            if let Some(on) = self.take_mouse_request() {
                let _ = if on {
                    crossterm::execute!(terminal.backend_mut(), EnableMouseCapture)
                } else {
                    crossterm::execute!(terminal.backend_mut(), DisableMouseCapture)
                };
                dirty = true;
            }
            if let Some(source) = self.take_projects_request() {
                let tx = projects_tx.clone();
                in_flight.push(tokio::spawn(async move {
                    let _ = tx.send(source.list().await);
                }));
            }
            for request in self.take_recruit_requests() {
                in_flight.retain(|task| !task.is_finished());
                if matches!(request, RecruitRequest::Cancel { .. }) {
                    // Dropping the editor's future ends its agent session.
                    in_flight.drain(..).for_each(|task| task.abort());
                }
                in_flight.push(tokio::spawn(hire::execute(
                    recruiter.clone(),
                    agents.clone(),
                    request,
                    reply_tx.clone(),
                )));
            }
            if dirty {
                self.set_state(company.snapshot().await);
                self.roles = company.roles();
                terminal.draw(|frame| {
                    frame.render_widget(&self, frame.area());
                    if let Some(pos) = self.cursor_position(frame.area()) {
                        frame.set_cursor_position(pos);
                    }
                })?;
                dirty = false;
            }
            tokio::select! {
                key = keys.next() => match key {
                    // Crossterm also emits release and repeat events. Acting only on
                    // press avoids handling one keystroke more than once.
                    Some(Ok(TermEvent::Key(key))) if key.kind == KeyEventKind::Press => {
                        let cmds = self.handle_key_event(key);
                        self.send_all(&company, cmds).await;
                        dirty = true;
                    }
                    // Act only on clicks and the wheel; plain motion is reported too.
                    Some(Ok(TermEvent::Mouse(mouse))) => {
                        let area = Rect::from((Position::ORIGIN, terminal.size()?));
                        self.handle_mouse_event(mouse, area);
                        dirty = matches!(
                            mouse.kind,
                            MouseEventKind::Down(MouseButton::Left)
                                | MouseEventKind::ScrollUp
                                | MouseEventKind::ScrollDown
                        );
                    }
                    Some(Ok(TermEvent::Paste(text))) => {
                        self.handle_paste(&text);
                        dirty = true;
                    }
                    Some(Ok(TermEvent::Resize(..))) => dirty = true,
                    Some(Err(err)) => return Err(err),
                    None => break,
                    _ => {}
                },
                event = bus.recv() => match event {
                    Ok(event) => {
                        self.push_event(event);
                        // Coalesce bursts (streamed chunks) into one redraw.
                        while let Ok(more) = bus.try_recv() {
                            self.push_event(more);
                        }
                        dirty = true;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => dirty = true,
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                _ = spin.tick(), if self.is_busy() => {
                    self.tick = self.tick.wrapping_add(1);
                    dirty = true;
                }
                Some(items) = project_replies.recv() => {
                    self.on_projects(items);
                    dirty = true;
                }
                Some(reply) = replies.recv() => {
                    let cmds = self.on_recruit(reply);
                    self.send_all(&company, cmds).await;
                    dirty = true;
                }
            }
        }
        in_flight.iter().for_each(|task| task.abort());
        Ok(())
    }

    async fn send_all(&mut self, company: &Arc<dyn CompanyHandle>, cmds: Vec<Command>) {
        for cmd in cmds {
            if let Err(err) = company.send(Actor::Stakeholder, cmd).await {
                self.status = Some(err.to_string());
            }
        }
    }
}

#[cfg(test)]
mod tests;
