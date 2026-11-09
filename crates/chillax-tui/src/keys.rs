//! The key tables: every key a view offers is declared once, with the
//! condition under which its action exists. The footer prints only the available entries
//! and the key handler acts only through the same entries, so a hint can never promise
//! something that would be refused, and a hidden key cannot act.

use chillax_core::{InboxItem, SprintStatus};
use crossterm::event::KeyCode;

use crate::{App, Tab};

/// What a key does. Handlers match on this, never on the raw key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Act {
    Quit,
    NextTab,
    PrevTab,
    GotoTab,
    /// Usage: reset the "since you opened this view" baseline.
    UsageResetBaseline,
    /// Usage: record what live sessions have not recorded yet and refresh the numbers.
    UsageRefresh,
    /// Usage: switch the token chart between per hour and per day.
    UsageBucket,
    /// Usage: scroll the panels.
    UsageScroll,
    Pause,
    Resume,
    Hire,
    Projects,
    /// Copy the focused item to the clipboard (OSC 52).
    Copy,
    /// Release or retake the mouse so the terminal can select text.
    ToggleMouse,
    ClearActivity,
    ClearChat,
    /// Chat tab: switch between one person's chat and the company chat feed.
    ToggleCompany,
    /// Stop the selected person's current turn, keeping their session.
    Stop,
    Refresh,
    Move,
    /// Grid only: one tile left or right.
    MoveSide,
    /// Board: the previous or next column that has tickets, keeping the row.
    MoveColumn,
    /// Board: the first or last ticket of the column.
    MoveEdge,
    /// Board: a page up or down within the column.
    MovePage,
    /// Board: the first or last column that has tickets.
    MoveColumnEdge,
    /// Board: enter the board, so the arrows move the selection.
    FocusBoard,
    /// Board: leave it, so the arrows change view again.
    UnfocusBoard,
    /// Team grid: enter the grid, so the arrows walk the tiles.
    FocusTeam,
    /// Team grid: leave it, so the arrows change view again.
    UnfocusTeam,
    ToggleGrid,
    OpenLive,
    ToggleOutput,
    Scroll,
    /// Ticket panel: jump to the next or previous section.
    NextSection,
    PrevSection,
    OpenChat,
    Type,
    Fire,
    /// Team: hire another employee just like the selected one.
    Duplicate,
    SetAgent,
    OpenTicket,
    NewItem,
    NewRequest,
    Requests,
    ApproveSprint,
    RejectSprint,
    PlanNext,
    CloseSprint,
    Promote,
    PrepareRelease,
    ForceRelease,
    ApproveRelease,
    RejectRelease,
    Retry,
    /// Environments: pick the app the app keys act on.
    SelectApp,
    StartApp,
    StopApp,
    RestartApp,
    OpenApp,
    /// Dashboard: open the first running app (supervised dev first, else a reported one).
    OpenDashApp,
    ChangeAgent,
    Choose,
    CancelPermission,
    Answer,
    Dismiss,
    MoveTicket,
    Comment,
    OpenImage,
    Assign,
    Blockers,
    Edit,
    Drop,
    Prioritize,
    /// Board: show the next / previous sprint view (active, past sprints, backlog).
    NextSprintView,
    PrevSprintView,
    CloseTicket,
    /// Team: allow one more agent at a time / one fewer.
    AgentsMore,
    AgentsFewer,
}

/// Why an action is unavailable; empty means "say nothing".
pub(crate) type Avail = Result<(), String>;
type When = fn(&App, Option<&str>) -> Avail;

pub(crate) struct KeyDef {
    /// Shown in the footer; empty hides the entry (it still acts).
    pub hint: &'static str,
    pub label: &'static str,
    pub codes: &'static [KeyCode],
    pub act: Act,
    pub when: When,
}

const fn k(
    hint: &'static str,
    label: &'static str,
    codes: &'static [KeyCode],
    act: Act,
    when: When,
) -> KeyDef {
    KeyDef {
        hint,
        label,
        codes,
        act,
        when,
    }
}

fn no(reason: &str) -> Avail {
    Err(reason.to_string())
}

fn when(ok: bool, reason: &str) -> Avail {
    if ok { Ok(()) } else { no(reason) }
}

fn always(_: &App, _: Option<&str>) -> Avail {
    Ok(())
}

const C_UPDOWN: &[KeyCode] = &[
    KeyCode::Up,
    KeyCode::Down,
    KeyCode::Char('k'),
    KeyCode::Char('j'),
];
const C_LEFTRIGHT: &[KeyCode] = &[
    KeyCode::Left,
    KeyCode::Right,
    KeyCode::Char('h'),
    KeyCode::Char('l'),
];
const C_EDGE: &[KeyCode] = &[
    KeyCode::Home,
    KeyCode::End,
    KeyCode::Char('g'),
    KeyCode::Char('G'),
];
const C_COLUMN_EDGE: &[KeyCode] = &[KeyCode::Char('['), KeyCode::Char(']')];
const C_PAGE: &[KeyCode] = &[KeyCode::PageUp, KeyCode::PageDown];
const C_Q: &[KeyCode] = &[KeyCode::Char('q'), KeyCode::Esc];
const C_DIGITS: &[KeyCode] = &[
    KeyCode::Char('1'),
    KeyCode::Char('2'),
    KeyCode::Char('3'),
    KeyCode::Char('4'),
    KeyCode::Char('5'),
    KeyCode::Char('6'),
    KeyCode::Char('7'),
    KeyCode::Char('8'),
    KeyCode::Char('9'),
];
/// Digits that jump to a tab: `C_DIGITS` plus `0` for the tenth. `0` is kept out of
/// `C_DIGITS` because choosing and moving tickets use that and count from 1.
const C_TAB_DIGITS: &[KeyCode] = &[
    KeyCode::Char('1'),
    KeyCode::Char('2'),
    KeyCode::Char('3'),
    KeyCode::Char('4'),
    KeyCode::Char('5'),
    KeyCode::Char('6'),
    KeyCode::Char('7'),
    KeyCode::Char('8'),
    KeyCode::Char('9'),
    KeyCode::Char('0'),
];

impl App {
    fn staff(&self) -> Avail {
        when(!self.state.employees.is_empty(), "Nobody is hired yet")
    }

    /// The selected person has a chat to clear and is not replying right now.
    fn can_clear_chat(&self) -> Avail {
        let Some(view) = self.state.employees.get(self.selected) else {
            return when(false, "Nobody is hired yet");
        };
        if view.turn_open {
            return when(false, "They are replying; try again when they finish");
        }
        let has_lines = self
            .state
            .chats
            .get(&view.employee.id)
            .is_some_and(|c| !c.is_empty());
        when(has_lines, "No chat to clear")
    }

    /// The selected person is working, or a reply to the stakeholder is on its way.
    fn can_stop(&self) -> Avail {
        let Some(view) = self.state.employees.get(self.selected) else {
            return when(false, "Nobody is hired yet");
        };
        when(
            view.turn_open
                || view.reply_pending
                || view.status == chillax_core::EmployeeStatus::Working,
            "They are not working",
        )
    }

    fn crowd(&self) -> Avail {
        when(self.state.employees.len() > 1, "")
    }

    /// The app the app keys act on: the selected one, else the only one.
    pub(crate) fn selected_app(&self) -> Option<&chillax_core::RunningApp> {
        self.state.apps.get(
            self.app_selected
                .min(self.state.apps.len().saturating_sub(1)),
        )
    }

    /// Availability of an app key: there is a selected app and it is in the right state.
    fn app_key(&self, ok: fn(&chillax_core::RunningApp) -> bool, verb: &str) -> Avail {
        let Some(app) = self.selected_app() else {
            return no("No app is configured: set `run` and `port` under [env.*] in chillax.toml");
        };
        when(
            ok(app),
            &format!("The {} app cannot be {verb}ed now", app.env),
        )
    }

    fn has_activity(&self) -> bool {
        self.events
            .iter()
            .any(|e| e.seq > self.state.activity_cleared_seq)
    }

    pub(crate) fn proposed_sprint(&self) -> Option<&chillax_core::Sprint> {
        self.state
            .sprints
            .iter()
            .find(|s| s.status == SprintStatus::Proposed)
    }

    fn can_promote(&self) -> Avail {
        let s = &self.state;
        if s.open_round().is_some() {
            no("A QA round is open")
        } else if s.qa_requested {
            no("Promotion to qa is already requested")
        } else if s.open_release().is_some() {
            no("A release is in flight")
        } else {
            when(!s.waiting_for_qa().is_empty(), "Nothing is waiting for QA")
        }
    }

    fn can_prepare(&self) -> Avail {
        if self.state.open_round().is_some() {
            no("A QA round is open: prepare the release once it closes")
        } else {
            when(
                self.state.open_release().is_none(),
                "A release is already in flight",
            )
        }
    }

    fn selected_inbox(&self) -> Option<&InboxItem> {
        self.state.inbox.get(self.inbox_selected)
    }
}

static GLOBAL: &[KeyDef] = &[
    k("", "", C_Q, Act::Quit, always),
    k(
        "",
        "",
        &[KeyCode::Tab, KeyCode::Right],
        Act::NextTab,
        always,
    ),
    k(
        "",
        "",
        &[KeyCode::BackTab, KeyCode::Left],
        Act::PrevTab,
        always,
    ),
    k("", "", C_TAB_DIGITS, Act::GotoTab, |app, _| {
        // Digits choose in the Inbox; beyond the last tab they do nothing.
        if app.tab == Tab::Inbox {
            no("")
        } else {
            Ok(())
        }
    }),
    k("", "", &[KeyCode::Char('P')], Act::Pause, |a, _| {
        when(!a.state.dispatch_paused, "")
    }),
    k("", "", &[KeyCode::Char('P')], Act::Resume, |a, _| {
        when(a.state.dispatch_paused, "")
    }),
    SELECT_ON,
    SELECT_OFF,
    PROJECTS,
];

/// `y` copies the focused item; offered only where there is something to copy.
const COPY: KeyDef = k("y", "copy", &[KeyCode::Char('y')], Act::Copy, |a, t| {
    let ok = match t {
        Some(id) => a.ticket_text(id).is_some(),
        None => a.focused_text().is_some(),
    };
    when(ok, "")
});
/// `m` frees the mouse for native drag-select, and takes it back.
const SELECT_ON: KeyDef = k(
    "m",
    "select text",
    &[KeyCode::Char('m')],
    Act::ToggleMouse,
    |a, _| when(!a.mouse_released, ""),
);
const SELECT_OFF: KeyDef = k(
    "m",
    "mouse on",
    &[KeyCode::Char('m')],
    Act::ToggleMouse,
    |a, _| when(a.mouse_released, ""),
);
const PAUSE: KeyDef = k("P", "pause", &[KeyCode::Char('P')], Act::Pause, |a, _| {
    when(!a.state.dispatch_paused, "")
});
const RESUME: KeyDef = k("P", "resume", &[KeyCode::Char('P')], Act::Resume, |a, _| {
    when(a.state.dispatch_paused, "")
});
/// Switch to another running project (`P` is taken by pause). Offered only when the
/// run loop gave the app an instance registry.
const PROJECTS: KeyDef = k(
    "O",
    "other projects",
    &[KeyCode::Char('O')],
    Act::Projects,
    |a, _| when(a.instances.is_some(), ""),
);
const QUIT: KeyDef = k("q", "quit", C_Q, Act::Quit, always);

/// Start a new sprint: the project manager is briefed and the chat opens. On the Dashboard
/// it is offered only once there is something to start from (a release or a backlog).
const NEW_SPRINT: KeyDef = k(
    "N",
    "new sprint",
    &[KeyCode::Char('N')],
    Act::PlanNext,
    |a, _| {
        if !a.can_plan() {
            return Err(match a.state.sprint_in_flight() {
                Some(sp) => format!("Sprint {} is not finished yet", sp.id),
                None => "The project manager is already planning".into(),
            });
        }
        when(
            a.tab != Tab::Dashboard || a.can_start_sprint(),
            "No release yet: hire a project manager and chat about the first sprint",
        )
    },
);

static DASHBOARD: &[KeyDef] = &[
    NEW_SPRINT,
    k("h", "hire", &[KeyCode::Char('h')], Act::Hire, always),
    PROJECTS,
    k(
        "o",
        "open app",
        &[KeyCode::Char('o')],
        Act::OpenDashApp,
        |a, _| when(a.first_running_url().is_some(), "No app is running"),
    ),
    k(
        "c",
        "clear activity",
        &[KeyCode::Char('c')],
        Act::ClearActivity,
        |a, _| when(a.has_activity(), "No activity to clear"),
    ),
    k(
        "R",
        "refresh agents",
        &[KeyCode::Char('R')],
        Act::Refresh,
        |a, _| a.staff(),
    ),
    k(
        "Tab",
        "next view",
        &[KeyCode::Tab, KeyCode::Right],
        Act::NextTab,
        always,
    ),
    QUIT,
];

/// The Team grid has been entered and has tiles, so the arrows walk them.
fn team_entered(a: &App) -> bool {
    a.grid && a.team_focus && a.crowd().is_ok()
}

static TEAM: &[KeyDef] = &[
    k("↑↓", "select", C_UPDOWN, Act::Move, |a, _| {
        when(!a.grid && a.state.employees.len() > 1, "")
    }),
    // In the grid the arrows walk the tiles; Tab still changes view.
    k("↑↓←→", "select", C_UPDOWN, Act::Move, |a, _| {
        when(team_entered(a), "")
    }),
    k(
        "",
        "",
        &[KeyCode::Left, KeyCode::Right],
        Act::MoveSide,
        |a, _| when(team_entered(a), ""),
    ),
    // Until the grid is entered the arrows change view (hint only; the lookup skips it).
    k("←→", "view", &[], Act::NextTab, |a, _| {
        when(a.grid && !a.team_focus, "")
    }),
    k(
        "Enter/↓",
        "grid",
        &[KeyCode::Enter, KeyCode::Down, KeyCode::Char('j')],
        Act::FocusTeam,
        |a, _| when(a.grid && !a.team_focus && a.crowd().is_ok(), ""),
    ),
    k(
        "Esc",
        "back to tabs",
        &[KeyCode::Esc],
        Act::UnfocusTeam,
        |a, _| when(a.grid && a.team_focus, ""),
    ),
    k("h", "hire", &[KeyCode::Char('h')], Act::Hire, always),
    k("Enter", "chat", &[KeyCode::Enter], Act::OpenChat, |a, _| {
        if a.grid { no("") } else { a.staff() }
    }),
    k("Enter", "live", &[KeyCode::Enter], Act::OpenLive, |a, _| {
        if a.grid && a.team_focus {
            a.staff()
        } else {
            no("")
        }
    }),
    k(
        "g",
        "grid",
        &[KeyCode::Char('g')],
        Act::ToggleGrid,
        |a, _| {
            if a.grid { no("") } else { a.staff() }
        },
    ),
    k(
        "g",
        "list",
        &[KeyCode::Char('g')],
        Act::ToggleGrid,
        |a, _| {
            if a.grid { a.staff() } else { no("") }
        },
    ),
    k(
        "a",
        "agent",
        &[KeyCode::Char('a')],
        Act::SetAgent,
        |a, _| a.staff(),
    ),
    k(
        "+",
        "more agents",
        &[KeyCode::Char('+'), KeyCode::Char('=')],
        Act::AgentsMore,
        |a, _| match a.state.agent_limit {
            None => no("The agent limit is not known yet"),
            Some(l) if l.limit >= l.hardware_max => no(&format!(
                "Already at this machine's maximum ({})",
                l.hardware_max
            )),
            Some(_) => Ok(()),
        },
    ),
    k(
        "-",
        "fewer agents",
        &[KeyCode::Char('-')],
        Act::AgentsFewer,
        |a, _| match a.state.agent_limit {
            None => no("The agent limit is not known yet"),
            Some(l) if l.limit <= 1 => no("At least one agent must be allowed"),
            Some(_) => Ok(()),
        },
    ),
    k("x", "fire", &[KeyCode::Char('x')], Act::Fire, |a, _| {
        a.staff()
    }),
    k(
        "d",
        "duplicate",
        &[KeyCode::Char('d')],
        Act::Duplicate,
        |a, _| a.staff(),
    ),
    k("s", "stop", &[KeyCode::Char('s')], Act::Stop, |a, _| {
        a.can_stop()
    }),
    QUIT,
];

static CHAT: &[KeyDef] = &[
    k("↑↓", "person", C_UPDOWN, Act::Move, |a, _| a.crowd()),
    k(
        "i",
        "type",
        &[KeyCode::Char('i'), KeyCode::Enter],
        Act::Type,
        |a, _| if a.company_view { Ok(()) } else { a.staff() },
    ),
    k(
        "c",
        "company chat",
        &[KeyCode::Char('c')],
        Act::ToggleCompany,
        always,
    ),
    k("PgUp/PgDn", "scroll", C_PAGE, Act::Scroll, |a, _| {
        if a.company_view { Ok(()) } else { a.staff() }
    }),
    k(
        "a",
        "agent",
        &[KeyCode::Char('a')],
        Act::SetAgent,
        |a, _| a.staff(),
    ),
    k("x", "fire", &[KeyCode::Char('x')], Act::Fire, |a, _| {
        a.staff()
    }),
    k("s", "stop", &[KeyCode::Char('s')], Act::Stop, |a, _| {
        a.can_stop()
    }),
    k(
        "X",
        "clear chat",
        &[KeyCode::Char('X')],
        Act::ClearChat,
        |a, _| a.can_clear_chat(),
    ),
    COPY,
    QUIT,
];

/// The Board takes the arrows only once entered (Enter or ↓); until then they change view.
fn board_entered(a: &App) -> bool {
    a.board_focus && a.board_ticket().is_some()
}

static BOARD: &[KeyDef] = &[
    k("↑↓", "select", C_UPDOWN, Act::Move, |a, _| {
        when(board_entered(a), "")
    }),
    // Not entered yet: ↓ enters the board.
    k(
        "",
        "",
        &[KeyCode::Down, KeyCode::Char('j')],
        Act::FocusBoard,
        |a, _| when(!a.board_focus && a.board_ticket().is_some(), ""),
    ),
    // Unentered, or with no tickets, the arrows fall through to the global view switch.
    k("←→", "column", C_LEFTRIGHT, Act::MoveColumn, |a, _| {
        when(board_entered(a), "")
    }),
    // Hint only (no keys): the global table owns the arrows while the board is not entered.
    k("←→", "view", &[], Act::NextTab, |a, _| {
        when(!board_entered(a), "")
    }),
    k("g/G", "top/bottom", C_EDGE, Act::MoveEdge, |a, _| {
        when(board_entered(a), "")
    }),
    k("", "", C_PAGE, Act::MovePage, |a, _| {
        when(board_entered(a), "")
    }),
    k(
        "[ ]",
        "first/last column",
        C_COLUMN_EDGE,
        Act::MoveColumnEdge,
        |a, _| when(board_entered(a), ""),
    ),
    k(
        "< >",
        "sprint",
        &[KeyCode::Char('<')],
        Act::PrevSprintView,
        always,
    ),
    k("", "", &[KeyCode::Char('>')], Act::NextSprintView, always),
    // The arrows are taken above, so say how to change view.
    k("Tab", "next view", &[KeyCode::Tab], Act::NextTab, |a, _| {
        when(board_entered(a), "")
    }),
    k(
        "Enter/↓",
        "board",
        &[KeyCode::Enter],
        Act::FocusBoard,
        |a, _| {
            when(
                !a.board_focus && a.board_ticket().is_some(),
                "No tickets yet",
            )
        },
    ),
    k(
        "!",
        "urgent",
        &[KeyCode::Char('!')],
        Act::Prioritize,
        |a, _| {
            if a.sprint_view.is_read_only() {
                return no(a.read_only_reason());
            }
            when(
                board_entered(a) && a.board_ticket().is_some_and(|t| !a.state.is_settled(t)),
                "This ticket is finished or dropped",
            )
        },
    ),
    k(
        "Enter",
        "open",
        &[KeyCode::Enter],
        Act::OpenTicket,
        |a, _| when(board_entered(a), ""),
    ),
    k(
        "Esc",
        "back to tabs",
        &[KeyCode::Esc],
        Act::UnfocusBoard,
        |a, _| when(a.board_focus, ""),
    ),
    k("n", "new item", &[KeyCode::Char('n')], Act::NewItem, always),
    k(
        "i",
        "new request",
        &[KeyCode::Char('i')],
        Act::NewRequest,
        always,
    ),
    k(
        "I",
        "requests",
        &[KeyCode::Char('I')],
        Act::Requests,
        |a, _| {
            when(
                !a.state.requests.is_empty(),
                "No requests yet: press i to describe one",
            )
        },
    ),
    k(
        "a",
        "approve sprint",
        &[KeyCode::Char('a')],
        Act::ApproveSprint,
        |a, _| {
            when(
                a.proposed_sprint().is_some(),
                "No sprint is waiting for approval",
            )
        },
    ),
    k(
        "r",
        "reject sprint",
        &[KeyCode::Char('r')],
        Act::RejectSprint,
        |a, _| {
            when(
                a.proposed_sprint().is_some(),
                "No sprint is waiting for approval",
            )
        },
    ),
    NEW_SPRINT,
    k(
        "C",
        "close sprint",
        &[KeyCode::Char('C')],
        Act::CloseSprint,
        |a, _| when(a.closable_sprint().is_some(), "No sprint is running"),
    ),
    COPY,
    PAUSE,
    RESUME,
    QUIT,
];

static LIVE: &[KeyDef] = &[
    k("↑↓", "person", C_UPDOWN, Act::Move, |a, _| a.crowd()),
    k("PgUp/PgDn", "scroll", C_PAGE, Act::Scroll, |a, _| a.staff()),
    k(
        "e",
        "expand output",
        &[KeyCode::Char('e')],
        Act::ToggleOutput,
        |a, _| when(a.staff().is_ok() && !a.expanded, ""),
    ),
    k(
        "e",
        "fold output",
        &[KeyCode::Char('e')],
        Act::ToggleOutput,
        |a, _| when(a.staff().is_ok() && a.expanded, ""),
    ),
    PAUSE,
    RESUME,
    QUIT,
];

static ENVIRONMENTS: &[KeyDef] = &[
    k(
        "p",
        "promote",
        &[KeyCode::Char('p')],
        Act::Promote,
        |a, _| a.can_promote(),
    ),
    k(
        "g",
        "prepare release",
        &[KeyCode::Char('g')],
        Act::PrepareRelease,
        |a, _| a.can_prepare(),
    ),
    k(
        "f",
        "release with known issues",
        &[KeyCode::Char('f')],
        Act::ForceRelease,
        |a, _| a.can_prepare(),
    ),
    k(
        "a",
        "approve",
        &[KeyCode::Char('a')],
        Act::ApproveRelease,
        |a, _| {
            when(
                a.gated_release().is_some(),
                "No release is waiting for approval",
            )
        },
    ),
    k(
        "x",
        "reject",
        &[KeyCode::Char('x')],
        Act::RejectRelease,
        |a, _| {
            when(
                a.rejectable_release().is_some(),
                "No release is waiting for approval",
            )
        },
    ),
    k("r", "retry", &[KeyCode::Char('r')], Act::Retry, |a, _| {
        when(a.retry_command().is_some(), "Nothing to retry")
    }),
    k("↑↓", "app", C_UPDOWN, Act::SelectApp, |a, _| {
        when(a.state.apps.len() > 1, "")
    }),
    k(
        "s",
        "start app",
        &[KeyCode::Char('s')],
        Act::StartApp,
        |a, _| a.app_key(|app| app.can_start(), "start"),
    ),
    k(
        "t",
        "stop app",
        &[KeyCode::Char('t')],
        Act::StopApp,
        |a, _| a.app_key(|app| app.can_stop(), "stop"),
    ),
    k(
        "u",
        "restart app",
        &[KeyCode::Char('u')],
        Act::RestartApp,
        |a, _| a.app_key(|app| app.can_restart(), "restart"),
    ),
    k(
        "o/Enter",
        "open app",
        &[KeyCode::Char('o'), KeyCode::Enter],
        Act::OpenApp,
        |a, _| a.app_key(|app| app.is_running(), "open"),
    ),
    QUIT,
];

static EVENTS: &[KeyDef] = &[
    k("PgUp/PgDn", "scroll", C_PAGE, Act::Scroll, |a, _| {
        when(!a.events.is_empty(), "")
    }),
    COPY,
    QUIT,
];

static USAGE: &[KeyDef] = &[
    k("↑↓", "scroll", C_UPDOWN, Act::UsageScroll, always),
    k(
        "h",
        "tokens/hour|day",
        &[KeyCode::Char('h')],
        Act::UsageBucket,
        always,
    ),
    k(
        "r",
        "reset since-opened",
        &[KeyCode::Char('r')],
        Act::UsageResetBaseline,
        always,
    ),
    k(
        "R",
        "refresh now",
        &[KeyCode::Char('R')],
        Act::UsageRefresh,
        always,
    ),
    QUIT,
];

static SETTINGS: &[KeyDef] = &[
    k(
        "Enter",
        "change agent & model",
        &[KeyCode::Enter],
        Act::ChangeAgent,
        always,
    ),
    QUIT,
];

const INBOX_MOVE: KeyDef = k("↑↓", "select", C_UPDOWN, Act::Move, |a, _| {
    when(a.state.inbox.len() > 1, "")
});

static INBOX_EMPTY: &[KeyDef] = &[INBOX_MOVE];
static INBOX_PERMISSION: &[KeyDef] = &[
    INBOX_MOVE,
    COPY,
    k("1-9", "choose", C_DIGITS, Act::Choose, always),
    k(
        "c",
        "cancel",
        &[KeyCode::Char('c')],
        Act::CancelPermission,
        always,
    ),
];
static INBOX_QUESTION: &[KeyDef] = &[
    INBOX_MOVE,
    COPY,
    k("Enter", "answer", &[KeyCode::Enter], Act::Answer, always),
];
static INBOX_SPRINT: &[KeyDef] = &[
    INBOX_MOVE,
    COPY,
    k(
        "a",
        "approve sprint",
        &[KeyCode::Char('a')],
        Act::ApproveSprint,
        always,
    ),
    k(
        "r",
        "reject sprint",
        &[KeyCode::Char('r')],
        Act::RejectSprint,
        always,
    ),
];
static INBOX_RELEASE: &[KeyDef] = &[
    INBOX_MOVE,
    COPY,
    k(
        "a",
        "approve release",
        &[KeyCode::Char('a')],
        Act::ApproveRelease,
        always,
    ),
    k(
        "x",
        "reject release",
        &[KeyCode::Char('x')],
        Act::RejectRelease,
        always,
    ),
];
static INBOX_ENV: &[KeyDef] = &[
    INBOX_MOVE,
    COPY,
    k("r", "retry", &[KeyCode::Char('r')], Act::Retry, always),
];
static INBOX_ALERT: &[KeyDef] = &[
    INBOX_MOVE,
    COPY,
    k("d", "dismiss", &[KeyCode::Char('d')], Act::Dismiss, always),
];

static TICKET: &[KeyDef] = &[
    COPY,
    SELECT_ON,
    SELECT_OFF,
    k("1-9", "move to state", C_DIGITS, Act::MoveTicket, |a, t| {
        let moves = t
            .and_then(|id| a.state.ticket(id))
            .is_some_and(|t| !a.state.stakeholder_moves(t).is_empty());
        when(moves, "This ticket has no moves left")
    }),
    k("c", "comment", &[KeyCode::Char('c')], Act::Comment, always),
    k(
        "o",
        "open image",
        &[KeyCode::Char('o')],
        Act::OpenImage,
        |a, t| {
            when(
                t.and_then(|id| a.state.ticket(id))
                    .is_some_and(|t| !crate::ticket_images(t).is_empty()),
                "This ticket has no images",
            )
        },
    ),
    k("A", "assign", &[KeyCode::Char('A')], Act::Assign, |a, _| {
        when(a.can_assign(), "Hire someone to assign work to")
    }),
    k(
        "b",
        "blockers",
        &[KeyCode::Char('b')],
        Act::Blockers,
        |a, t| {
            when(
                t.is_some_and(|id| a.can_block(id)),
                "No other ticket could block this one",
            )
        },
    ),
    k("e", "edit", &[KeyCode::Char('e')], Act::Edit, always),
    k(
        "!",
        "urgent",
        &[KeyCode::Char('!')],
        Act::Prioritize,
        |a, t| {
            when(
                t.and_then(|id| a.state.ticket(id))
                    .is_some_and(|t| !a.state.is_settled(t)),
                "This ticket is finished or dropped",
            )
        },
    ),
    k("d", "drop", &[KeyCode::Char('d')], Act::Drop, |a, t| {
        when(
            t.and_then(|id| a.state.ticket(id))
                .is_some_and(|t| a.state.droppable(t)),
            "This ticket can no longer be dropped",
        )
    }),
    k(
        "↑↓",
        "scroll",
        &[
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::Char('g'),
            KeyCode::Char('G'),
        ],
        Act::Scroll,
        always,
    ),
    k("Tab", "section", &[KeyCode::Tab], Act::NextSection, always),
    k("", "", &[KeyCode::BackTab], Act::PrevSection, always),
    k("Esc", "close", &[KeyCode::Esc], Act::CloseTicket, always),
];

/// The table behind the current view (the Inbox follows the selected item's type).
pub(crate) fn tab_table(app: &App) -> &'static [KeyDef] {
    match app.tab {
        Tab::Dashboard => DASHBOARD,
        Tab::Team => TEAM,
        Tab::Chat => CHAT,
        Tab::Board => BOARD,
        Tab::Live => LIVE,
        Tab::Environments => ENVIRONMENTS,
        Tab::Events => EVENTS,
        Tab::Settings => SETTINGS,
        Tab::Usage => USAGE,
        Tab::Inbox => match app.selected_inbox() {
            None => INBOX_EMPTY,
            Some(InboxItem::Permission { .. }) => INBOX_PERMISSION,
            Some(InboxItem::Question { .. }) => INBOX_QUESTION,
            Some(InboxItem::SprintGate { .. }) => INBOX_SPRINT,
            Some(InboxItem::ReleaseGate { .. }) => INBOX_RELEASE,
            Some(InboxItem::EnvAlert { .. }) => INBOX_ENV,
            Some(InboxItem::Alert { .. }) => INBOX_ALERT,
        },
    }
}

pub(crate) fn ticket_table() -> &'static [KeyDef] {
    TICKET
}

/// The footer hints of a table: only the keys whose action exists right now.
pub(crate) fn hints(
    app: &App,
    table: &[KeyDef],
    ticket: Option<&str>,
) -> Vec<(&'static str, &'static str)> {
    table
        .iter()
        .filter(|d| !d.hint.is_empty() && (d.when)(app, ticket).is_ok())
        .map(|d| (d.hint, d.label))
        .collect()
}

pub(crate) enum Lookup {
    Run(Act),
    /// The key is bound but its action does not exist now; the reason may be empty.
    Refused(String),
    Unbound,
}

fn flipped(code: KeyCode) -> Option<KeyCode> {
    match code {
        KeyCode::Char(c) if c.is_alphabetic() => {
            let other = if c.is_uppercase() {
                c.to_lowercase().next()?
            } else {
                c.to_uppercase().next()?
            };
            (other != c).then_some(KeyCode::Char(other))
        }
        _ => None,
    }
}

/// Resolve a key against `tables`: the exact key first, then the same letter in the other
/// case (a stakeholder pressing `A` for `a`). The first available entry wins.
pub(crate) fn lookup(
    app: &App,
    tables: &[&[KeyDef]],
    ticket: Option<&str>,
    code: KeyCode,
) -> Lookup {
    for candidate in [Some(code), flipped(code)].into_iter().flatten() {
        let mut refused = None;
        for def in tables.iter().flat_map(|t| t.iter()) {
            if !def.codes.contains(&candidate) {
                continue;
            }
            match (def.when)(app, ticket) {
                Ok(()) => return Lookup::Run(def.act),
                Err(why) => {
                    refused.get_or_insert(why);
                }
            }
        }
        if let Some(why) = refused {
            return Lookup::Refused(why);
        }
    }
    Lookup::Unbound
}

pub(crate) fn global_table() -> &'static [KeyDef] {
    GLOBAL
}

/// Hints of a text-entry mode: each is shown only while its key would act.
pub(crate) fn gated(
    items: &[(&'static str, &'static str, bool)],
) -> Vec<(&'static str, &'static str)> {
    items
        .iter()
        .filter(|(_, _, ok)| *ok)
        .map(|(k, l, _)| (*k, *l))
        .collect()
}
