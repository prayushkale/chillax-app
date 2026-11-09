//! Rendering. Reads [`App`]; never writes it.

use chillax_core::engine::EmployeeView;
use chillax_core::notify;
use chillax_core::{EmployeeStatus, HireState, InboxItem, Speaker, SprintStatus, SprintView};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::symbols::border;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Paragraph, StatefulWidget, Tabs, Widget, Wrap,
};
use unicode_width::UnicodeWidthStr;

use crate::editor::{Editor, EditorView};
use crate::form::{Field, FormTarget, TicketForm};
use crate::keys;
use crate::{AgentPicker, App, HireFlow, HireStep, Mode, PickStage, Tab};

impl Widget for &App {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let [header, body, footer] = screen(area);
        self.render_header(header, buf);
        match self.tab {
            Tab::Dashboard => self.render_dashboard(body, buf),
            Tab::Team => self.render_team(body, buf),
            Tab::Chat => self.render_chat(body, buf),
            Tab::Board => self.render_board(body, buf),
            Tab::Live => self.render_live(body, buf),
            Tab::Environments => self.render_environments(body, buf),
            Tab::Inbox => self.render_inbox(body, buf),
            Tab::Events => self.render_events(body, buf),
            Tab::Settings => self.render_settings(body, buf),
            Tab::Usage => self.render_usage(body, buf),
        }
        self.render_footer(footer, buf);
        match &self.mode {
            Mode::Hire(flow) => self.render_hire(area, buf, flow),
            Mode::Answer { id, text } => render_prompt(area, buf, &format!(" Answer {id} "), text),
            Mode::ConfirmQuit => {
                render_prompt(area, buf, " Quit Chillax? ", "Agents will be stopped. y/n");
            }
            Mode::ConfirmLetGo { employee } => render_prompt(
                area,
                buf,
                " Fire? ",
                &format!(
                    "Fire (let go) {}? Their session ends and their tickets return to the \
                     backlog. y/n",
                    self.state
                        .employee(employee)
                        .map_or(employee.as_str(), |e| e.employee.name.as_str())
                ),
            ),
            Mode::ConfirmClearChat { employee } => render_prompt(
                area,
                buf,
                " Clear chat? ",
                &format!(
                    "Clear the chat with {}? Their chat session ends and the next message \
                     starts fresh. y/n",
                    self.state
                        .employee(employee)
                        .map_or(employee.as_str(), |e| e.employee.name.as_str())
                ),
            ),
            Mode::Ticket { id } => self.render_ticket(area, buf, id),
            Mode::Form(form) => render_form(area, buf, form),
            Mode::Settings(picker) | Mode::EmployeeAgent { picker, .. } => {
                let title = match &self.mode {
                    Mode::EmployeeAgent { employee, .. } => {
                        let view = self.state.employee(employee);
                        format!(
                            " Change agent: {}{} ",
                            view.map_or(employee.as_str(), |e| e.employee.name.as_str()),
                            if view.is_some_and(|e| e.turn_open) {
                                " (applies after the current turn)"
                            } else {
                                ""
                            }
                        )
                    }
                    _ => " Hiring defaults ".to_string(),
                };
                let popup = centered(
                    area,
                    area.width.saturating_sub(4).min(90),
                    area.height.saturating_sub(4).min(24),
                );
                Clear.render(popup, buf);
                let block = Block::bordered()
                    .title(title)
                    .border_set(border::ROUNDED)
                    .border_style(Style::new().cyan());
                let inner = block.inner(popup);
                block.render(popup, buf);
                picker.render(inner, buf, self.tick);
            }
            Mode::Projects(list) => {
                let popup = centered(
                    area,
                    area.width.saturating_sub(4).min(90),
                    area.height.saturating_sub(4).min(20),
                );
                Clear.render(popup, buf);
                let block = Block::bordered()
                    .title(" Running projects ")
                    .border_set(border::ROUNDED)
                    .border_style(Style::new().cyan());
                let inner = block.inner(popup);
                block.render(popup, buf);
                list.render(inner, buf, !self.no_hyperlinks);
            }
            Mode::Release(edit) => self.render_release_editor(area, buf, edit),
            Mode::RejectRelease { id, text } => {
                let version = self
                    .state
                    .release(id)
                    .map_or(id.as_str(), |r| r.version.as_str());
                render_prompt(area, buf, &format!(" Reject {version}? Why "), text);
            }
            Mode::ConfirmForce => render_prompt(
                area,
                buf,
                " Release with known issues? ",
                "Prepare the release even though some tickets failed QA? They ship as known \
                 issues. y/n",
            ),
            Mode::Comment {
                id,
                text,
                files,
                field,
            } => render_comment(area, buf, id, text, files, *field),
            Mode::Assign { id, choice } => self.render_assign(area, buf, id, *choice),
            Mode::Blockers { id, text } => render_prompt(
                area,
                buf,
                &format!(" What blocks {id}? Ticket ids, e.g. T-2 T-5 "),
                text,
            ),
            Mode::Drop {
                id,
                reason,
                confirm: false,
            } => render_prompt(area, buf, &format!(" Drop {id}? Why "), reason),
            Mode::Drop {
                id,
                reason,
                confirm: true,
            } => render_prompt(
                area,
                buf,
                &format!(" Drop {id}? "),
                &format!(
                    "{reason}\n\nIt leaves the board and its sprint, and its agents stop. y/n"
                ),
            ),
            Mode::PlanSprint { text } => render_prompt(
                area,
                buf,
                " Plan the next sprint — anything to focus on? (optional) ",
                text,
            ),
            Mode::ConfirmCloseSprint { id } => render_prompt(
                area,
                buf,
                &format!(" Close {id}? "),
                &format!(
                    "Close sprint {id}? Unfinished tickets leave it and can be planned \
                     again; planned ones return to the backlog. y/n"
                ),
            ),
            Mode::Request { text, start_now } => render_request_editor(area, buf, text, *start_now),
            Mode::Requests => self.render_requests(area, buf),
            Mode::Normal | Mode::Compose => {}
        }
    }
}

/// Header, body and footer rows of the screen.
pub(crate) fn screen(area: Rect) -> [Rect; 3] {
    Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area)
}

/// Narrowest hires strip worth drawing; tabs keep their width first.
const STRIP_MIN: u16 = 24;

/// Brand, tabs and the right-aligned hires strip. The tabs take what they need before the
/// strip gets the remainder, and the strip is dropped when that is under [`STRIP_MIN`].
fn header_areas(app: &App, header: Rect) -> [Rect; 3] {
    let [brand, rest] =
        Layout::horizontal([Constraint::Length(brand_width(app)), Constraint::Min(0)])
            .areas(header);
    let lines = app.tab_lines();
    let needed = lines.iter().map(|l| l.width() as u16 + 2).sum::<u16>()
        + (lines.len() as u16).saturating_sub(1);
    let tabs_width = needed.min(rest.width);
    // One cell of air between the tabs and the strip.
    let left = rest.width.saturating_sub(tabs_width).saturating_sub(1);
    let strip_width = if left >= STRIP_MIN && !app.state.employees.is_empty() {
        left
    } else {
        0
    };
    let [tabs, strip] = Layout::horizontal([
        Constraint::Length(rest.width - strip_width),
        Constraint::Length(strip_width),
    ])
    .areas(rest);
    [brand, tabs, strip]
}

/// The hires as `●name state T-n` chips that fit in `width`, `+N` for the rest. Names are
/// cut to eight cells; ticket titles never appear.
fn hire_strip(app: &App, width: u16) -> Line<'static> {
    let hires = notify::hire_summaries(&app.state);
    let chips: Vec<(Vec<Span<'static>>, usize)> = hires
        .iter()
        .map(|h| {
            let colour = match h.state {
                HireState::Working => Color::Green,
                HireState::Blocked => Color::Yellow,
                HireState::Failed => Color::Red,
                _ => Color::DarkGray,
            };
            let name = if h.name.chars().count() > 8 {
                format!("{}…", h.name.chars().take(7).collect::<String>())
            } else {
                h.name.clone()
            };
            let mut text = format!("{name} {}", h.state.label());
            if let Some(t) = &h.ticket {
                text.push(' ');
                text.push_str(t);
            }
            let text_style = if colour == Color::DarkGray {
                Style::new().fg(Color::DarkGray)
            } else {
                Style::new()
            };
            let w = 1 + text.width();
            (
                vec![
                    Span::styled("●", Style::new().fg(colour)),
                    Span::styled(text, text_style),
                ],
                w,
            )
        })
        .collect();
    let avail = width.saturating_sub(1) as usize;
    let more = |n: usize| format!("+{n}").width() + 2;
    let mut keep = chips.len();
    loop {
        let used: usize = chips[..keep].iter().map(|c| c.1).sum::<usize>()
            + keep.saturating_sub(1) * 2
            + if keep < chips.len() {
                more(chips.len() - keep)
            } else {
                0
            };
        if used <= avail || keep == 0 {
            break;
        }
        keep -= 1;
    }
    let mut spans = Vec::new();
    for (i, (chip, _)) in chips[..keep].iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.extend(chip.clone());
    }
    if keep < chips.len() {
        if keep > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            format!("+{}", chips.len() - keep),
            Style::new().fg(Color::DarkGray),
        ));
    }
    spans.push(Span::raw(" "));
    Line::from(spans).right_aligned()
}

/// Where each tab label (with its one-cell padding) is drawn in the header. Mirrors the
/// `Tabs` widget: padding, title, padding, then a one-cell divider.
pub(crate) fn tab_rects(app: &App, header: Rect) -> Vec<Rect> {
    let [_, tabs, _] = header_areas(app, header);
    let mut x = tabs.x;
    let end = tabs.right();
    let mut rects = Vec::new();
    for line in app.tab_lines() {
        let width = line.width() as u16 + 2;
        let rect = Rect::new(x, tabs.y, width, 1).intersection(tabs);
        rects.push(rect);
        x = x.saturating_add(width + 1).min(end);
    }
    rects
}

/// Left (people) and right (conversation) halves of the Chat tab.
pub(crate) fn chat_columns(body: Rect) -> [Rect; 2] {
    Layout::horizontal([Constraint::Length(24), Constraint::Min(0)]).areas(body)
}

/// Message log and input box inside the Chat conversation area.
pub(crate) fn chat_main(app: &App, main: Rect) -> [Rect; 2] {
    // The box grows with the wrapped draft, up to a third of the conversation area.
    let rows = app.draft.rows(main.width.saturating_sub(2)) as u16;
    let cap = (main.height / 3).max(3);
    let input_height = rows.saturating_add(2).clamp(3, cap);
    Layout::vertical([Constraint::Min(0), Constraint::Length(input_height)]).areas(main)
}

/// List and detail panes of the Inbox tab.
pub(crate) fn inbox_areas(body: Rect) -> [Rect; 2] {
    Layout::vertical([Constraint::Percentage(40), Constraint::Percentage(60)]).areas(body)
}

/// The Dashboard's panels: team, sprint, running apps, inbox, and the bottom pane.
/// Shared by drawing and by clicks on the running-app rows.
pub(crate) fn dashboard_areas(app: &App, area: Rect) -> [Rect; 5] {
    let [top, bottom] =
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);
    let [team, side] =
        Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(top);
    let app_rows = app.state.running_apps().len();
    // Rows plus the border, at least 3 (the empty text), at most 6.
    let apps_height = u16::try_from(app_rows + 2).unwrap_or(6).clamp(3, 6);
    let [sprint, apps, inbox] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(apps_height),
        Constraint::Fill(1),
    ])
    .areas(side);
    [team, sprint, apps, inbox, bottom]
}

/// Sprint banner row and column area of the Board tab.
pub(crate) fn board_areas(app: &App, body: Rect) -> [Rect; 2] {
    let rows = u16::from(app.board_banner().is_some()) + u16::from(app.request_banner().is_some());
    Layout::vertical([Constraint::Length(rows), Constraint::Min(0)]).areas(body)
}

pub(crate) fn board_columns(app: &App, board: Rect) -> Vec<Rect> {
    let states = &app.state.workflow_states;
    if states.is_empty() {
        return Vec::new();
    }
    Layout::horizontal(
        states
            .iter()
            .map(|_| Constraint::Ratio(1, states.len() as u32)),
    )
    .split(board)
    .to_vec()
}

/// Interior of a bordered panel.
pub(crate) fn panel_inner(area: Rect) -> Rect {
    panel("").inner(area)
}

/// The header text of tab `i`: its key and title. The tenth tab is on `0`.
pub(crate) fn tab_label(i: usize, tab: Tab) -> String {
    format!("{} {}", (i + 1) % 10, tab.title())
}

impl App {
    fn tab_lines(&self) -> Vec<Line<'static>> {
        Tab::ALL
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let mut spans = vec![Span::raw(tab_label(i, *t))];
                if *t == Tab::Inbox && !self.state.inbox.is_empty() {
                    spans.push(Span::styled(
                        format!(" ({})", self.state.inbox.len()),
                        Style::new().red().bold(),
                    ));
                }
                Line::from(spans)
            })
            .collect()
    }

    fn render_header(&self, area: Rect, buf: &mut Buffer) {
        let titles = self.tab_lines();
        let [brand, tabs, strip] = header_areas(self, area);
        let mut brand_line = vec![" ◐ ".cyan().bold(), self.company().bold(), " ".into()];
        if self.state.dispatch_paused {
            brand_line.push("⏸ paused ".yellow().bold());
        }
        Line::from(brand_line).render(brand, buf);
        Tabs::new(titles)
            .select(self.tab.index())
            .highlight_style(Style::new().reversed())
            .divider("│")
            .render(tabs, buf);
        if strip.width > 0 {
            hire_strip(self, strip.width).render(strip, buf);
        }
    }

    fn company(&self) -> String {
        if self.state.company.is_empty() {
            "Chillax".into()
        } else {
            self.state.company.clone()
        }
    }

    pub(crate) fn render_footer(&self, area: Rect, buf: &mut Buffer) {
        let mut lead = Vec::new();
        if self.mouse_released {
            lead.push(Span::styled(
                " SELECT MODE (m: mouse on) ",
                Style::new().yellow().bold(),
            ));
        }
        if let Some(status) = &self.status {
            lead.push(Span::styled(format!(" {status}"), Style::new().red()));
            Line::from(lead).render(area, buf);
            return;
        }
        let owned: Vec<(&str, &str)>;
        let hints: &[(&str, &str)] = match (&self.mode, self.tab) {
            (Mode::Compose, _) => {
                owned = keys::gated(&[
                    ("Enter", "send", !self.draft.is_blank()),
                    ("Alt+Enter", "newline", true),
                    ("↑↓←→", "move", true),
                    ("Esc", "stop typing", true),
                ]);
                &owned
            }
            (Mode::Hire(flow), _) => hire_hints(flow),
            (Mode::Answer { text, .. }, _) => {
                owned = keys::gated(&[
                    ("Enter", "send", !text.trim().is_empty()),
                    ("Esc", "cancel", true),
                ]);
                &owned
            }
            (
                Mode::ConfirmLetGo { .. }
                | Mode::ConfirmClearChat { .. }
                | Mode::ConfirmForce
                | Mode::ConfirmQuit,
                _,
            ) => &[("y", "yes"), ("n", "no")],
            (Mode::Release(edit), _) => {
                owned = keys::gated(&[
                    (
                        "Enter",
                        "approve",
                        !edit.version.trim().is_empty() && !edit.notes.trim().is_empty(),
                    ),
                    ("Tab", "version/notes", true),
                    ("Alt+Enter", "newline", edit.field == 1),
                    ("Esc", "cancel", true),
                ]);
                &owned
            }
            (Mode::Ticket { id }, _) => {
                owned = keys::hints(self, keys::ticket_table(), Some(id));
                &owned
            }
            (
                Mode::Comment {
                    text, files, field, ..
                },
                _,
            ) => {
                owned = keys::gated(&[
                    (
                        "Enter",
                        "send",
                        !text.trim().is_empty() || !files.trim().is_empty(),
                    ),
                    ("Alt+Enter", "newline", *field == 0),
                    ("Tab", "text/images", true),
                    ("Esc", "back", true),
                ]);
                &owned
            }
            (Mode::Assign { .. }, _) => &[("↑↓", "choose"), ("Enter", "assign"), ("Esc", "back")],
            (Mode::Blockers { .. }, _) => &[("Enter", "save"), ("Esc", "back")],
            (
                Mode::Drop {
                    confirm: false,
                    reason,
                    ..
                },
                _,
            ) => {
                owned = keys::gated(&[
                    ("Enter", "continue", !reason.trim().is_empty()),
                    ("Esc", "back", true),
                ]);
                &owned
            }
            (Mode::Drop { confirm: true, .. }, _) | (Mode::ConfirmCloseSprint { .. }, _) => {
                &[("y", "yes"), ("n", "no")]
            }
            (Mode::PlanSprint { .. }, _) => &[("Enter", "ask the PM"), ("Esc", "cancel")],
            (Mode::Request { text, .. }, _) => {
                owned = keys::gated(&[
                    ("Enter", "new line", true),
                    ("Tab", "start now", true),
                    ("Ctrl+S", "send", !text.is_blank()),
                    ("Esc", "cancel", true),
                ]);
                &owned
            }
            (Mode::Requests, _) => {
                let scrolls = self.requests_max.load(std::sync::atomic::Ordering::Relaxed) > 0;
                owned = keys::gated(&[
                    ("↑↓", "scroll", scrolls),
                    ("any other key", "close", scrolls),
                    ("any key", "close", !scrolls),
                ]);
                &owned
            }
            (Mode::Projects(list), _) => {
                owned = keys::gated(&[
                    ("↑↓", "select", list.can_open()),
                    ("Enter", "open dashboard", list.can_open()),
                    ("Esc", "close", true),
                ]);
                &owned
            }
            (Mode::Form(_), _) => &[("Tab", "next field"), ("Ctrl+S", "save"), ("Esc", "cancel")],
            (Mode::RejectRelease { .. }, _) => &[("Enter", "reject"), ("Esc", "cancel")],
            (Mode::Settings(picker) | Mode::EmployeeAgent { picker, .. }, _) => {
                picker_hints(picker)
            }
            (_, tab) => {
                debug_assert_eq!(tab, self.tab);
                let mut all = keys::hints(self, keys::tab_table(self), None);
                // The global `m` entries are the only global ones with a hint.
                all.extend(keys::hints(self, keys::global_table(), None));
                owned = all;
                &owned
            }
        };
        let mut spans = lead;
        if let Some(toast) = &self.toast {
            spans.push(Span::styled(format!(" {toast} "), Style::new().green()));
        }
        for (key, what) in hints {
            spans.push(format!(" {key} ").blue().bold());
            spans.push(Span::raw(format!("{what} ")));
        }
        if let Some(url) = &self.web_url {
            spans.push(Span::styled(
                format!(" web: {url}"),
                Style::new().dark_gray(),
            ));
        }
        Line::from(spans).render(area, buf);
    }

    fn render_dashboard(&self, area: Rect, buf: &mut Buffer) {
        let [team, sprint, apps, inbox, bottom] = dashboard_areas(self, area);

        let team_lines: Vec<Line> = if self.state.employees.is_empty() {
            vec![Line::from("Nobody yet. Press h to hire a Project Manager.").dark_gray()]
        } else {
            self.state
                .employees
                .iter()
                .map(|e| {
                    let on = (e.status == EmployeeStatus::Working)
                        .then(|| self.state.working_on(&e.employee.id));
                    employee_line(e, on)
                })
                .collect()
        };
        Paragraph::new(team_lines)
            .block(panel(&self.team_title()))
            .render(team, buf);

        let sprint_text =
            match self
                .state
                .sprints
                .iter()
                .rev()
                .find(|s| !matches!(s.status, SprintStatus::Rejected | SprintStatus::Closed))
            {
                Some(s) => {
                    let live: Vec<_> = self.state.sprint_tickets(s).collect();
                    let done = live.iter().filter(|t| t.state == "done").count();
                    let status = match s.status {
                        SprintStatus::Closed => "closed".dark_gray(),
                        SprintStatus::Proposed => "awaiting your approval".yellow(),
                        SprintStatus::Active => "active".green(),
                        SprintStatus::Rejected => "rejected".red(),
                        SprintStatus::Released => "released".cyan(),
                    };
                    let mut head = vec![format!("{} {} · ", s.id, s.name).bold(), status];
                    if s.status == SprintStatus::Released
                        && let Some(r) =
                            self.state.releases.iter().rev().find(|r| {
                                r.sprint.as_deref() == Some(s.id.as_str()) && r.tag.is_some()
                            })
                    {
                        head.push(format!(" · {}", r.version).cyan());
                    }
                    vec![
                        Line::from(head),
                        Line::from(s.goal.clone()).dark_gray(),
                        Line::from(format!("{done} / {} done", live.len())),
                    ]
                }
                None => vec![
                    Line::from(format!(
                        "No sprint running · {} tickets in backlog",
                        self.state
                            .tickets
                            .iter()
                            .filter(|t| t.dropped.is_none() && t.state != "done")
                            .count()
                    ))
                    .dark_gray(),
                ],
            };
        let mut sprint_text = sprint_text;
        if self.state.sprint_in_flight().is_none()
            && self.state.planning.is_none()
            && self.can_start_sprint()
        {
            sprint_text.push(Line::from(vec![
                "N".blue().bold(),
                "  start a new sprint: talk to the PM about the next release".into(),
            ]));
        }
        Paragraph::new(sprint_text)
            .wrap(Wrap { trim: true })
            .block(panel("Sprint"))
            .render(sprint, buf);

        self.render_running_apps(apps, buf);

        let inbox_lines: Vec<Line> = if self.state.inbox.is_empty() {
            vec![Line::from("Nothing needs you.").dark_gray()]
        } else {
            self.state
                .inbox
                .iter()
                .map(|i| Line::from(inbox_title(self, i)))
                .collect()
        };
        Paragraph::new(inbox_lines)
            .block(panel("Needs you"))
            .render(inbox, buf);

        let height = bottom.height.saturating_sub(2) as usize;
        let recent: Vec<Line> = self
            .events
            .iter()
            .rev()
            .take_while(|e| e.seq > self.state.activity_cleared_seq)
            .filter(|e| !matches!(e.kind, chillax_core::EventKind::AgentMessageChunk { .. }))
            .take(height)
            .map(event_line)
            .collect();
        Paragraph::new(recent)
            .block(panel("Recent activity"))
            .render(bottom, buf);
    }

    /// The Team panel title, with the agent limit once it is known.
    fn team_title(&self) -> String {
        match self.state.agent_limit {
            Some(l) => format!(
                "Team · Agents: {}/{} (max {})",
                self.state.agents_running(),
                l.limit,
                l.hardware_max
            ),
            None => "Team".to_string(),
        }
    }

    pub(crate) fn render_team(&self, area: Rect, buf: &mut Buffer) {
        if self.state.employees.is_empty() {
            Paragraph::new("Nobody on the team yet. Press h to hire.")
                .dark_gray()
                .block(panel(&self.team_title()))
                .render(area, buf);
            return;
        }
        if self.grid {
            self.render_grid(area, buf);
            return;
        }
        let items: Vec<ListItem> = self
            .state
            .employees
            .iter()
            .map(|e| {
                let assigned: Vec<_> = self
                    .state
                    .assignments(&e.employee.id)
                    .iter()
                    .map(|t| t.id.clone())
                    .collect();
                let mut lines = vec![employee_line(e, None)];
                lines.push(
                    Line::from(format!(
                        "    {} · {}{}{}",
                        e.employee.backend,
                        if assigned.is_empty() {
                            "no tickets".to_string()
                        } else {
                            assigned.join(", ")
                        },
                        self.working_on_suffix(e),
                        e.pending_agent.as_ref().map_or(String::new(), |p| format!(
                            " · next: {}",
                            chillax_core::event::agent_label(&p.backend, &p.model, &p.effort)
                        ))
                    ))
                    .dark_gray(),
                );
                ListItem::new(lines)
            })
            .collect();
        let mut list_state = ListState::default().with_selected(Some(self.selected));
        StatefulWidget::render(
            List::new(items)
                .block(panel(&self.team_title()))
                .highlight_symbol("▶ ")
                .highlight_style(Style::new().bold()),
            area,
            buf,
            &mut list_state,
        );
    }

    fn render_chat(&self, area: Rect, buf: &mut Buffer) {
        let [people, main] = chat_columns(area);
        let items: Vec<ListItem> = self
            .state
            .employees
            .iter()
            .map(|e| {
                ListItem::new(vec![
                    Line::from(vec![
                        Span::raw(e.employee.name.clone()),
                        Span::styled(
                            format!(" · {}", e.employee.role_title),
                            Style::new().dark_gray(),
                        ),
                    ]),
                    Line::from(format!("  {}", status_label(e))).style(status_style(e.status)),
                ])
            })
            .collect();
        let mut list_state = ListState::default().with_selected(Some(self.selected));
        StatefulWidget::render(
            List::new(items)
                .block(panel("People"))
                .highlight_symbol("▶ ")
                .highlight_style(Style::new().bold()),
            people,
            buf,
            &mut list_state,
        );

        let [log, input] = chat_main(self, main);
        if self.company_view {
            self.render_company_log(log, buf);
            self.render_chat_input(input, buf, " Post to everyone (Enter to send) ");
            return;
        }
        let Some(employee) = self.state.employees.get(self.selected) else {
            Paragraph::new("Hire someone first (Team tab, h).")
                .dark_gray()
                .block(panel("Chat"))
                .render(main, buf);
            return;
        };
        let title = format!(
            "{} · {}",
            employee.employee.name, employee.employee.role_title
        );
        let block = panel(&title);
        let inner = block.inner(log);
        block.render(log, buf);

        let lines = self.chat_lines(employee, inner.width);
        let height = inner.height as usize;
        let scroll = self.note_scroll_max(lines.len().saturating_sub(height));
        let bottom = lines.len().saturating_sub(scroll as usize);
        let top = bottom.saturating_sub(height);
        Paragraph::new(lines[top..bottom].to_vec()).render(inner, buf);

        self.render_chat_input(input, buf, " Message (Enter to send) ");
    }

    /// The message box under the Chat log; `composing_title` is shown while typing.
    fn render_chat_input(&self, input: Rect, buf: &mut Buffer, composing_title: &'static str) {
        let composing = self.mode == Mode::Compose;
        let input_block = Block::bordered()
            .title(if composing {
                composing_title
            } else {
                " Press i to type "
            })
            .border_style(if composing {
                Style::new().cyan()
            } else {
                Style::new().dark_gray()
            });
        let (above, below) = self.draft.hidden(
            input.width.saturating_sub(2),
            input.height.saturating_sub(2),
        );
        let input_block = if above > 0 {
            input_block.title_bottom(
                Line::from(format!(" ↑ {above} more "))
                    .dark_gray()
                    .right_aligned(),
            )
        } else {
            input_block
        };
        let input_block = if below > 0 {
            input_block.title_bottom(Line::from(format!(" ↓ {below} more ")).dark_gray())
        } else {
            input_block
        };
        let inner = input_block.inner(input);
        input_block.render(input, buf);
        EditorView(&self.draft).render(inner, buf);
    }

    /// The company chat feed in the Chat tab's log area.
    fn render_company_log(&self, log: Rect, buf: &mut Buffer) {
        let block = panel("Company chat");
        let inner = block.inner(log);
        block.render(log, buf);
        let lines = self.company_lines(inner.width);
        let height = inner.height as usize;
        let scroll = self.note_scroll_max(lines.len().saturating_sub(height));
        let bottom = lines.len().saturating_sub(scroll as usize);
        let top = bottom.saturating_sub(height);
        Paragraph::new(lines[top..bottom].to_vec()).render(inner, buf);
    }

    /// The company chat feed as `HH:MM from → to: text` lines, wrapped; `@mentions` of
    /// people on the team are bold.
    pub(crate) fn company_lines(&self, width: u16) -> Vec<Line<'static>> {
        let width = width.saturating_sub(1).max(10) as usize;
        if self.state.company_chat.is_empty() {
            return vec![
                Line::from("Nothing yet. Press i and write to everyone; @name wakes that person.")
                    .dark_gray(),
            ];
        }
        let mut lines = Vec::new();
        for entry in &self.state.company_chat {
            let time = entry.ts.with_timezone(&chrono::Local).format("%H:%M");
            let to = match &entry.to {
                None => "all".to_string(),
                Some(id) => self
                    .state
                    .employee(id)
                    .map_or_else(|| id.clone(), |e| e.employee.name.clone()),
            };
            let from_style = if entry.from.is_none() {
                Style::new().green().bold()
            } else {
                Style::new().magenta().bold()
            };
            let prefix_width = format!("{time} {} → {to}: ", entry.from_name).width();
            for (i, wrapped) in wrap(&entry.text, width.saturating_sub(prefix_width).max(8))
                .into_iter()
                .enumerate()
            {
                let mut spans = if i == 0 {
                    vec![
                        Span::styled(format!("{time} "), Style::new().dark_gray()),
                        Span::styled(entry.from_name.clone(), from_style),
                        Span::styled(format!(" → {to}: "), Style::new().dark_gray()),
                    ]
                } else {
                    vec![Span::raw(" ".repeat(prefix_width))]
                };
                spans.extend(self.mention_spans(&wrapped));
                lines.push(Line::from(spans));
            }
        }
        lines
    }

    /// `text` split so `@word`s that name someone on the team are bold.
    fn mention_spans(&self, text: &str) -> Vec<Span<'static>> {
        let mut spans = Vec::new();
        let mut plain = String::new();
        for piece in text.split_inclusive(' ') {
            let word = piece.trim_end();
            let is_mention = word.starts_with('@')
                && !chillax_core::engine::resolve_mentions(&self.state, word).is_empty();
            if is_mention {
                if !plain.is_empty() {
                    spans.push(Span::raw(std::mem::take(&mut plain)));
                }
                spans.push(Span::styled(word.to_string(), Style::new().bold()));
                plain.push_str(&piece[word.len()..]);
            } else {
                plain.push_str(piece);
            }
        }
        if !plain.is_empty() {
            spans.push(Span::raw(plain));
        }
        spans
    }

    /// Remember the size of a Board column's interior, so keys can scroll to the selection.
    fn note_board_view(&self, inner: Rect) {
        self.board_view.store(
            (u32::from(inner.width) << 16) | u32::from(inner.height),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// Record how far the log being drawn can scroll, and return the offset to draw at: the
    /// stored one, held inside the log.
    pub(crate) fn note_scroll_max(&self, max: usize) -> u16 {
        let max = max.min(u16::MAX as usize) as u16;
        self.scroll_max
            .store(max, std::sync::atomic::Ordering::Relaxed);
        self.scroll.min(max)
    }

    /// Record how far the ticket panel can scroll and how tall its window is, and return the
    /// offset to draw at: the stored one, held inside the panel.
    pub(crate) fn note_ticket_extent(&self, max: usize, height: u16) -> u16 {
        use std::sync::atomic::Ordering::Relaxed;
        let max = max.min(u16::MAX as usize) as u16;
        self.ticket_max.store(max, Relaxed);
        self.ticket_view.store(height, Relaxed);
        self.ticket_scroll.min(max)
    }

    /// The wrapped conversation with `employee`, as drawn in the Chat log.
    pub(crate) fn chat_lines(&self, employee: &EmployeeView, log_width: u16) -> Vec<Line<'static>> {
        let width = log_width.saturating_sub(1).max(10) as usize;
        let mut lines: Vec<Line> = Vec::new();
        let chat = self
            .state
            .chats
            .get(&employee.employee.id)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        if chat.is_empty() {
            lines.push(
                Line::from(format!(
                    "Say hi to {}. Their agent session starts on your first message.",
                    employee.employee.name
                ))
                .dark_gray(),
            );
        }
        for line in chat {
            let (label, style) = match &line.speaker {
                Speaker::Stakeholder => ("You".to_string(), Style::new().green().bold()),
                Speaker::Agent => (employee.employee.name.clone(), Style::new().cyan().bold()),
                Speaker::Teammate(name) => (name.clone(), Style::new().magenta().bold()),
                Speaker::Tool => ("⚙".to_string(), Style::new().dark_gray()),
                Speaker::System => ("•".to_string(), Style::new().yellow()),
            };
            let body_style = match line.speaker {
                Speaker::Tool => Style::new().dark_gray(),
                Speaker::System => Style::new().yellow(),
                _ => Style::new(),
            };
            let prefix = format!("{label}: ");
            for (i, wrapped) in wrap(&line.text, width.saturating_sub(prefix.width()).max(8))
                .into_iter()
                .enumerate()
            {
                let head = if i == 0 {
                    Span::styled(prefix.clone(), style)
                } else {
                    Span::raw(" ".repeat(prefix.width()))
                };
                lines.push(Line::from(vec![head, Span::styled(wrapped, body_style)]));
            }
        }
        if matches!(
            employee.status,
            EmployeeStatus::Working | EmployeeStatus::Starting
        ) {
            lines.push(
                Line::from(format!(
                    "{} {} is working…",
                    crate::spinner::frame(self.tick),
                    employee.employee.name
                ))
                .dark_gray()
                .italic(),
            );
        }
        lines
    }

    /// The line above the Board: the sprint waiting for approval, or what to do when no
    /// sprint is planned.
    pub(crate) fn board_banner(&self) -> Option<Line<'static>> {
        match &self.sprint_view {
            SprintView::Active => {}
            SprintView::Sprint(id) => {
                let s = self.state.sprint(id)?;
                let status = match s.status {
                    SprintStatus::Released => "Released",
                    SprintStatus::Closed => "Closed",
                    _ => "Past",
                };
                let goal = if s.goal.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", s.goal)
                };
                return Some(
                    Line::from(format!(
                        " {} {} · {status}{goal} (read-only) — < > sprint",
                        s.id, s.name
                    ))
                    .cyan(),
                );
            }
            SprintView::Backlog => {
                let n = self.state.board_tickets(&SprintView::Backlog).len();
                return Some(
                    Line::from(format!(
                        " Backlog · {n} unsprinted ticket{} (read-only) — < > sprint",
                        if n == 1 { "" } else { "s" }
                    ))
                    .cyan(),
                );
            }
        }
        if let Some(s) = self
            .state
            .sprints
            .iter()
            .find(|s| s.status == SprintStatus::Proposed)
        {
            return Some(Line::from(vec![
                format!(" {} {} is waiting for your approval — ", s.id, s.name)
                    .yellow()
                    .bold(),
                "a".blue().bold(),
                " approve  ".into(),
                "r".blue().bold(),
                " reject".into(),
            ]));
        }
        if self.state.sprint_in_flight().is_some() {
            return None;
        }
        let mut line = match &self.state.planning {
            Some(p) if !p.briefed => {
                Line::from(" Waiting for the project manager to take the planning request").yellow()
            }
            Some(_) => Line::from(" The project manager is planning the next sprint").yellow(),
            None if chillax_core::planning::candidates(&self.state).is_empty() => Line::default(),
            None => Line::from(vec![
                " No sprint is planned — ".yellow().bold(),
                "N".blue().bold(),
                " plan the next one".into(),
            ]),
        };
        // Tickets outside a sprint are off the Board, so say where they went.
        let mut extra = Vec::new();
        if !self.state.past_sprints().is_empty() {
            extra.push("Past sprints: < >".to_string());
        }
        let backlog = self.state.board_tickets(&SprintView::Backlog).len();
        if backlog > 0 {
            extra.push(format!("{backlog} in Backlog"));
        }
        if extra.is_empty() && line.spans.is_empty() {
            return None;
        }
        if !extra.is_empty() {
            let sep = if line.spans.is_empty() { " " } else { " · " };
            line.spans
                .push(format!("{sep}{}", extra.join(" · ")).dark_gray());
        }
        Some(line)
    }

    /// Why a request is not moving, from the roles this app knows.
    fn request_wait(&self, r: &chillax_core::Request) -> Option<String> {
        let first = self.state.workflow_states.first()?;
        chillax_core::intake::wait_reason(&self.state, r, |role| {
            self.roles
                .iter()
                .any(|x| x.id == role && x.owns_states.contains(first))
        })
    }

    /// A line about the requests the project manager has not finished, if any.
    pub(crate) fn request_banner(&self) -> Option<Line<'static>> {
        let open: Vec<_> = self
            .state
            .requests
            .iter()
            .filter(|r| r.status.is_open())
            .collect();
        let first = open.first()?;
        let mut text = format!(
            " {} request{} with the project manager",
            open.len(),
            if open.len() == 1 { "" } else { "s" }
        );
        if let Some(why) = self.request_wait(first) {
            text = format!(" {} queued: {why}", first.id);
        }
        Some(Line::from(vec![
            text.yellow(),
            "  I".blue().bold(),
            " list".into(),
        ]))
    }

    fn render_requests(&self, area: Rect, buf: &mut Buffer) {
        let popup = centered(
            area,
            area.width.saturating_sub(4).min(90),
            area.height.saturating_sub(4).min(24),
        );
        Clear.render(popup, buf);
        let inner_width = popup.width.saturating_sub(4) as usize;
        let mut lines: Vec<Line<'static>> = Vec::new();
        for r in self.state.requests.iter().rev() {
            let mut head = vec![
                Span::styled(r.id.clone(), Style::new().bold()),
                Span::raw(format!(" [{}]", r.status.label())),
            ];
            if r.start_now {
                head.push(Span::raw(" start now"));
            }
            if let Some(who) = r.from.as_deref().and_then(|id| self.state.employee(id)) {
                let kind = r.kind.map_or("suggestion", |k| k.label());
                head.push(Span::styled(
                    format!(" {kind} from {}", who.employee.name),
                    Style::new().dark_gray(),
                ));
            }
            lines.push(Line::from(head));
            for l in wrap(r.text.trim(), inner_width).into_iter().take(2) {
                lines.push(Line::from(format!("  {l}")));
            }
            if let Some(why) = self.request_wait(r) {
                lines.push(Line::from(format!("  {why}")).yellow());
            }
            for id in &r.tickets {
                if let Some(t) = self.state.ticket(id) {
                    let who = t
                        .assignee
                        .as_deref()
                        .and_then(|a| self.state.employee(a))
                        .map_or("unassigned", |e| e.employee.name.as_str());
                    lines.push(Line::from(format!("  {} {} → {who}", t.id, t.title)));
                }
            }
            lines.push(Line::from(""));
        }
        let max = lines
            .len()
            .saturating_sub(popup.height.saturating_sub(2) as usize)
            .min(u16::MAX as usize) as u16;
        self.requests_max
            .store(max, std::sync::atomic::Ordering::Relaxed);
        let offset = self.requests_scroll.min(max);
        let mut block = Block::bordered()
            .title(" Requests ")
            .border_set(border::ROUNDED)
            .border_style(Style::new().cyan());
        if offset > 0 {
            block = block.title_top(Line::from(format!(" ↑ {offset} more ")).dark_gray());
        }
        if offset < max {
            block =
                block.title_bottom(Line::from(format!(" ↓ {} more ", max - offset)).dark_gray());
        }
        Paragraph::new(lines)
            .scroll((offset, 0))
            .block(block)
            .render(popup, buf);
    }

    fn render_board(&self, area: Rect, buf: &mut Buffer) {
        let [banner, board] = board_areas(self, area);
        let lines: Vec<Line<'static>> = [self.board_banner(), self.request_banner()]
            .into_iter()
            .flatten()
            .collect();
        Paragraph::new(lines).render(banner, buf);
        let columns = board_columns(self, board);
        for (i, (key, col)) in self.state.workflow_states.iter().zip(columns).enumerate() {
            let count = self
                .state
                .board_tickets(&self.sprint_view)
                .iter()
                .filter(|t| &t.state == key)
                .count();
            let block = panel(&format!("{} ({count})", key.replace('_', " ")));
            let inner = block.inner(col);
            block.render(col, buf);
            if i == 0 {
                self.note_board_view(inner);
            }
            let lines = self.board_column_lines(key, inner.width);
            let max = lines.len().saturating_sub(inner.height as usize) as u16;
            let offset = self.board_scroll.get(i).copied().unwrap_or(0).min(max);
            Paragraph::new(lines).scroll((offset, 0)).render(inner, buf);
        }
    }

    /// The tickets in one Board column, wrapped to the column's interior width.
    pub(crate) fn board_column_lines(&self, state: &str, inner_width: u16) -> Vec<Line<'static>> {
        self.board_column(state, inner_width).0
    }

    /// The column's lines and the line range of the highlighted ticket's card.
    pub(crate) fn board_column(
        &self,
        state: &str,
        inner_width: u16,
    ) -> (Vec<Line<'static>>, Option<(usize, usize)>) {
        let (lines, cards) = self.board_cards(state, inner_width);
        let selected = self.board_ticket().map(|t| t.id.as_str());
        let card = cards
            .into_iter()
            .find(|(id, ..)| Some(id.as_str()) == selected)
            .map(|(_, start, end)| (start, end));
        (lines, card)
    }

    /// The column's lines and each ticket's `(id, first line, end line)`.
    pub(crate) fn board_cards(
        &self,
        state: &str,
        inner_width: u16,
    ) -> (Vec<Line<'static>>, Vec<(String, usize, usize)>) {
        let width = inner_width.max(4) as usize;
        let mut lines = Vec::new();
        let mut cards = Vec::new();
        let selected = self.board_ticket().map(|t| t.id.as_str());
        for t in self
            .state
            .board_tickets(&self.sprint_view)
            .into_iter()
            .filter(|t| t.state == state)
        {
            let start = lines.len();
            let mut first = true;
            // An urgent ticket carries a red `!` ahead of its id.
            let mark = if t.urgent { 2 } else { 0 };
            for part in wrap(&format!("{} {}", t.id, t.title), width.saturating_sub(mark)) {
                lines.push(if first {
                    let head = if t.urgent {
                        Line::from(vec![
                            Span::styled("! ", Style::new().red().bold()),
                            Span::raw(part),
                        ])
                    } else {
                        Line::from(part)
                    };
                    if Some(t.id.as_str()) != selected {
                        head.bold()
                    } else if self.board_focus {
                        head.bold().reversed()
                    } else {
                        // Remembered, but the board is not entered: no highlight.
                        head.bold().underlined()
                    }
                } else {
                    Line::from(part)
                });
                first = false;
            }
            let mut meta = vec![t.kind.label().to_string()];
            if let Some(e) = t.estimate {
                meta.push(format!("{e}pt"));
            }
            if let Some(a) = &t.assignee {
                meta.push(format!("@{a}"));
            }
            lines.push(Line::from(meta.join(" · ")).dark_gray());
            let badges = self.ticket_badges(t);
            if !badges.is_empty() {
                lines.push(Line::from(badges.join(" · ")).cyan());
            }
            cards.push((t.id.clone(), start, lines.len()));
            lines.push(Line::from(""));
        }
        (lines, cards)
    }

    /// Dev-loop facts about a ticket for its board card.
    fn ticket_badges(&self, t: &chillax_core::Ticket) -> Vec<String> {
        let mut badges = Vec::new();
        if !t.blocked_by.is_empty() && !self.state.blockers_clear(t) {
            badges.push(format!("blocked by {}", t.blocked_by.join(",")));
        }
        if t.review_requested {
            badges.push("testing".into());
        }
        if t.checks.contains("tests_pass") {
            badges.push("tests ✓".into());
        }
        if let (Some(reviewer), "in_review") = (&t.reviewer, t.state.as_str()) {
            badges.push(format!("review: {reviewer}"));
        }
        if t.checks.contains("review_approved") {
            badges.push("approved ✓".into());
        }
        if t.checks.contains("merged_to_dev") {
            badges.push("merged ✓".into());
        }
        if t.needs_rebase {
            badges.push("behind dev".into());
        }
        // The developer is resolving a conflict with dev, even while the ticket sits in
        // review.
        if let Some(c) = &t.conflict {
            badges.push(format!(
                "conflict {}/{}",
                c.attempts.min(chillax_core::engine::MAX_CONFLICT_ATTEMPTS),
                chillax_core::engine::MAX_CONFLICT_ATTEMPTS
            ));
        }
        if t.checks.contains("qa_passed") {
            badges.push("QA ✓".into());
        }
        if t.checks.contains("qa_failed") {
            badges.push("QA ✗".into());
            // Every bug QA filed for it is on dev: the next round retests it.
            let bugs: Vec<_> = self
                .state
                .tickets
                .iter()
                .filter(|b| b.found_in.as_deref() == Some(t.id.as_str()))
                .collect();
            if !bugs.is_empty()
                && bugs
                    .iter()
                    .all(|b| b.state == "done" || b.checks.contains("merged_to_dev"))
            {
                badges.push("retest".into());
            }
        }
        if let Some(story) = &t.found_in {
            badges.push(format!("bug of {story}"));
        }
        badges
    }

    fn render_inbox(&self, area: Rect, buf: &mut Buffer) {
        if self.state.inbox.is_empty() {
            Paragraph::new("Inbox zero. Chill.")
                .dark_gray()
                .block(panel("Inbox"))
                .render(area, buf);
            return;
        }
        let [list_area, detail] = inbox_areas(area);
        let items: Vec<ListItem> = self
            .state
            .inbox
            .iter()
            .map(|i| ListItem::new(inbox_title(self, i)))
            .collect();
        let mut list_state = ListState::default().with_selected(Some(self.inbox_selected));
        StatefulWidget::render(
            List::new(items)
                .block(panel("Inbox"))
                .highlight_symbol("▶ ")
                .highlight_style(Style::new().bold()),
            list_area,
            buf,
            &mut list_state,
        );
        let Some(item) = self.state.inbox.get(self.inbox_selected) else {
            return;
        };
        let mut lines: Vec<Line> = Vec::new();
        match item {
            InboxItem::Permission {
                employee,
                title,
                choices,
                ..
            } => {
                lines.push(Line::from(format!("{} wants to: {title}", self.name(employee))).bold());
                lines.push(Line::from(""));
                for (i, c) in choices.iter().enumerate() {
                    lines.push(Line::from(vec![
                        format!(" {} ", i + 1).blue().bold(),
                        Span::raw(c.name.clone()),
                    ]));
                }
                lines.push(Line::from(vec![" c ".blue().bold(), "cancel".into()]));
            }
            InboxItem::Question {
                employee, question, ..
            } => {
                lines.push(Line::from(format!("{} asks:", self.name(employee))).bold());
                lines.extend(question.lines().map(|l| Line::from(l.to_string())));
                lines.push(Line::from(""));
                lines.push(Line::from(vec![" Enter ".blue().bold(), "answer".into()]));
            }
            InboxItem::EnvAlert {
                env,
                step,
                output_tail,
                ..
            } => {
                lines.push(Line::from(format!("The {env} environment's {step:?} failed:")).bold());
                lines.extend(output_tail.lines().map(|l| Line::from(l.to_string())));
                lines.push(Line::from(""));
                lines.push(Line::from(vec![" r ".blue().bold(), "retry".into()]));
            }
            InboxItem::Alert { title, detail, .. } => {
                lines.push(Line::from(title.clone()).bold());
                lines.extend(detail.lines().map(|l| Line::from(l.to_string())));
                lines.push(Line::from(""));
                lines.push(Line::from(vec![" d ".blue().bold(), "dismiss".into()]));
            }
            InboxItem::ReleaseGate { release, .. } => {
                if let Some(r) = self.state.release(release) {
                    lines.push(
                        Line::from(format!(
                            "Release {} ({} tickets)",
                            r.version,
                            r.tickets.len()
                        ))
                        .bold(),
                    );
                    if !r.known_issues.is_empty() {
                        lines.push(
                            Line::from(format!("known issues: {}", r.known_issues.join(", ")))
                                .yellow(),
                        );
                    }
                    lines.extend(r.notes.lines().map(|l| Line::from(l.to_string())));
                    lines.push(Line::from(""));
                    lines.push(Line::from(vec![
                        " a ".blue().bold(),
                        "approve (edit version and notes)  ".into(),
                        " x ".blue().bold(),
                        "reject".into(),
                    ]));
                }
            }
            InboxItem::SprintGate { sprint, .. } => {
                if let Some(s) = self.state.sprint(sprint) {
                    lines.push(Line::from(format!("{} {}", s.id, s.name)).bold());
                    if !s.goal.is_empty() {
                        lines.push(Line::from(s.goal.clone()).dark_gray());
                    }
                    let mut points = 0;
                    for id in &s.tickets {
                        if let Some(t) = self.state.ticket(id) {
                            points += t.estimate.unwrap_or(0);
                            lines.push(Line::from(format!(
                                "  {} {} ({})",
                                t.id,
                                t.title,
                                t.kind.label()
                            )));
                            for ac in &t.acceptance_criteria {
                                lines.push(Line::from(format!("      ✓ {ac}")).dark_gray());
                            }
                        }
                    }
                    lines.push(Line::from(format!(
                        "{} tickets · {points} points",
                        s.tickets.len()
                    )));
                }
                lines.push(Line::from(vec![
                    " a ".blue().bold(),
                    "approve  ".into(),
                    " r ".blue().bold(),
                    "reject".into(),
                ]));
            }
        }
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(panel("Details"))
            .render(detail, buf);
    }

    fn render_settings(&self, area: Rect, buf: &mut Buffer) {
        let hiring = &self.state.hiring;
        let agent = match &hiring.backend {
            Some(b) => format!("{} ({b})", chillax_core::agents::backend_title(b)),
            None => "each role's own (claude unless the role says otherwise)".into(),
        };
        let model = hiring
            .model
            .clone()
            .unwrap_or_else(|| "the agent's default".into());
        let mut lines = vec![
            Line::from("Hiring defaults").bold(),
            Line::from(vec!["  Coding agent: ".bold(), Span::raw(agent)]),
            Line::from(vec!["  Model:        ".bold(), Span::raw(model)]),
        ];
        if let Some(effort) = &hiring.effort {
            lines.push(Line::from(vec![
                "  Effort:       ".bold(),
                Span::raw(effort.clone()),
            ]));
        }
        lines.extend([
            Line::from(""),
            Line::from(
                "New hires and persona revisions use these unless you pick others while \
                 hiring; what you pick there becomes the new default.",
            )
            .dark_gray(),
            Line::from("Press Enter to change them.").dark_gray(),
        ]);
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(panel("Settings"))
            .render(area, buf);
    }

    fn render_events(&self, area: Rect, buf: &mut Buffer) {
        let block = panel("Event log");
        let inner = block.inner(area);
        block.render(area, buf);
        let height = inner.height as usize;
        let scroll = self.note_scroll_max(self.events.len().saturating_sub(height));
        let end = self.events.len().saturating_sub(scroll as usize);
        let start = end.saturating_sub(height);
        let lines: Vec<Line> = self.events.range(start..end).map(event_line).collect();
        Paragraph::new(lines).render(inner, buf);
    }

    fn render_ticket(&self, area: Rect, buf: &mut Buffer, id: &str) {
        let Some(t) = self.state.ticket(id) else {
            return;
        };
        let none = || Span::styled("none", Style::new().dark_gray());
        let field = |label: &str, value: Span<'static>| {
            Line::from(vec![
                Span::styled(format!("{label}: "), Style::new().bold()),
                value,
            ])
        };
        let epic = t
            .epic
            .as_ref()
            .map(|e| match self.state.epics.iter().find(|x| &x.id == e) {
                Some(epic) => Span::raw(format!("{e} {}", epic.title)),
                None => Span::raw(e.clone()),
            })
            .unwrap_or_else(none);
        let mut lines: Vec<Line<'static>> = Vec::new();
        if let Some(reason) = &t.dropped {
            lines.push(Line::from(format!("Dropped: {reason}")).red().bold());
        }
        // Line index where each section starts, for Tab.
        let mut marks = vec![0usize];
        lines.extend([
            field("State", Span::raw(t.state.replace('_', " "))),
            field(
                "Assignee",
                t.assignee
                    .as_ref()
                    .map(|a| Span::raw(format!("{} (@{a})", self.name(a))))
                    .unwrap_or_else(none),
            ),
            field("Epic", epic),
            field(
                "Blockers",
                if t.blocked_by.is_empty() {
                    none()
                } else {
                    Span::raw(t.blocked_by.join(", "))
                },
            ),
        ]);
        if let Some(url) = &t.pr_url {
            lines.push(field("Pull request", Span::raw(url.clone())));
        }
        lines.push(Line::from(""));
        marks.push(lines.len());
        lines.push(Line::from("Description").bold());
        if t.description.trim().is_empty() {
            lines.push(Line::from("none").dark_gray());
        } else {
            lines.extend(t.description.lines().map(|l| Line::from(l.to_string())));
        }
        lines.push(Line::from(""));
        marks.push(lines.len());
        lines.push(Line::from("Acceptance criteria").bold());
        if t.acceptance_criteria.is_empty() {
            lines.push(Line::from("none").dark_gray());
        }
        lines.extend(
            t.acceptance_criteria
                .iter()
                .map(|c| Line::from(format!("☐ {c}"))),
        );
        lines.push(Line::from(""));
        marks.push(lines.len());
        lines.push(Line::from(format!("Comments ({})", t.comments.len())).bold());
        if t.comments.is_empty() {
            lines.push(Line::from("none").dark_gray());
        }
        let mut image_no = 0;
        let next = self.attach_cursor % crate::ticket_images(t).len().max(1);
        for c in &t.comments {
            lines.push(Line::from(c.author.clone()).cyan().bold());
            for part in wrap(&c.body, 68) {
                lines.push(Line::from(format!("  {part}")));
            }
            for a in &c.attachments {
                // The marker shows which image `o` opens next.
                let mark = if image_no == next { "▶" } else { " " };
                image_no += 1;
                lines.push(Line::from(vec![
                    format!("  {mark}[img {image_no}] ").magenta(),
                    Span::raw(format!("{} ({})", a.name, a.size_label())),
                ]));
            }
        }
        lines.push(Line::from(""));
        marks.push(lines.len());
        lines.push(Line::from("Move to").bold());
        let moves = self.state.stakeholder_moves(t);
        if moves.is_empty() {
            lines.push(Line::from("No manual moves from this state").dark_gray());
        }
        for (i, to) in moves.iter().enumerate() {
            lines.push(Line::from(vec![
                format!("{} ", i + 1).blue().bold(),
                Span::raw(to.replace('_', " ")),
            ]));
        }
        // Wrapped lines make the content taller than `lines`; count the rows it fills.
        let inner_w = area.width.min(72).saturating_sub(2);
        let rows_of = |l: &[Line<'static>]| {
            Paragraph::new(l.to_vec())
                .wrap(Wrap { trim: false })
                .line_count(inner_w.max(1))
        };
        let rows = rows_of(&lines);
        let height = (rows.min(u16::MAX as usize - 2) as u16 + 2).min(area.height);
        let popup = centered(area, 72, height);
        Clear.render(popup, buf);
        let block = Block::bordered()
            .title(if t.urgent {
                Line::from(vec![
                    Span::raw(" "),
                    Span::styled("!", Style::new().red().bold()),
                    Span::raw(format!(" {} {} ", t.id, t.title)),
                ])
            } else {
                Line::from(format!(" {} {} ", t.id, t.title))
            })
            .border_set(border::ROUNDED)
            .border_style(Style::new().cyan());
        let inner = block.inner(popup);
        block.render(popup, buf);
        let offsets: Vec<u16> = marks
            .iter()
            .map(|&i| rows_of(&lines[..i]).min(u16::MAX as usize) as u16)
            .collect();
        if let Ok(mut m) = self.ticket_marks.lock() {
            *m = offsets;
        }
        let scroll =
            self.note_ticket_extent(rows.saturating_sub(inner.height as usize), inner.height);
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0))
            .render(inner, buf);
    }

    fn render_assign(&self, area: Rect, buf: &mut Buffer, id: &str, choice: usize) {
        let mut items = vec![ListItem::new("Unassigned")];
        items.extend(
            self.state
                .employees
                .iter()
                .map(|e| ListItem::new(format!("{} · {}", e.employee.name, e.employee.role_title))),
        );
        let height = (items.len() as u16 + 2).min(area.height);
        let popup = centered(area, 50, height);
        Clear.render(popup, buf);
        let mut list_state = ListState::default().with_selected(Some(choice));
        StatefulWidget::render(
            List::new(items)
                .block(
                    Block::bordered()
                        .title(format!(" Assign {id} to "))
                        .border_set(border::ROUNDED)
                        .border_style(Style::new().cyan()),
                )
                .highlight_symbol("▶ ")
                .highlight_style(Style::new().bold()),
            popup,
            buf,
            &mut list_state,
        );
    }

    fn name(&self, id: &str) -> String {
        self.state
            .employee(id)
            .map(|e| e.employee.name.clone())
            .unwrap_or_else(|| id.to_string())
    }
}

fn picker_hints(picker: &AgentPicker) -> &'static [(&'static str, &'static str)] {
    match picker.stage {
        PickStage::Backend => &[("↑↓", "agent"), ("Enter", "choose"), ("Esc", "back")],
        PickStage::Model => &[
            ("↑↓", "model"),
            ("type", "model id"),
            ("Enter", "use it"),
            ("Esc", "back"),
        ],
        PickStage::Effort => &[("↑↓", "effort"), ("Enter", "use it"), ("Esc", "back")],
    }
}

fn hire_hints(flow: &HireFlow) -> &'static [(&'static str, &'static str)] {
    match &flow.step {
        HireStep::Pick => &[
            ("type", "search"),
            ("↑↓", "select"),
            ("Enter", "choose"),
            ("Ctrl+R", "refresh catalog"),
            ("Esc", "cancel"),
        ],
        HireStep::Reference { .. } => &[
            ("type", "search"),
            ("↑↓", "select"),
            ("Enter", "base on it"),
            ("Esc", "back"),
        ],
        HireStep::NewRole { .. } => &[
            ("type", "text"),
            ("Tab", "field"),
            ("Enter", "next"),
            ("Esc", "back"),
        ],
        HireStep::Name { .. } => &[("type", "name"), ("Enter", "next"), ("Esc", "back")],
        HireStep::Agent { picker, .. } => picker_hints(picker),
        HireStep::Preparing { .. } => &[("Esc", "back")],
        HireStep::Base { .. } => &[("↑↓", "base role"), ("Enter", "use it"), ("Esc", "back")],
        HireStep::Review(r) if r.input.is_some() => {
            &[("Enter", "ask the agent"), ("Esc", "stop typing")]
        }
        HireStep::Review(_) if flow.busy.is_some() => &[("Esc", "cancel")],
        HireStep::Review(_) => &[
            ("e", "suggest edit"),
            ("t", "tailor"),
            ("v", "view"),
            ("u", "undo"),
            ("b", "base role"),
            ("↑↓", "scroll"),
            ("a", "accept"),
            ("Esc", "discard"),
        ],
        HireStep::Confirm(_) => &[("Tab", "field"), ("Enter", "adopt & hire"), ("Esc", "back")],
    }
}

fn brand_width(app: &App) -> u16 {
    let paused = if app.state.dispatch_paused { 9 } else { 0 };
    (app.company().width() + 4 + paused) as u16
}

pub(crate) fn panel(title: &str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(Style::new().dark_gray())
        .title(Line::from(format!(" {title} ")).bold())
}

pub(crate) fn status_style(status: EmployeeStatus) -> Style {
    match status {
        EmployeeStatus::Idle => Style::new().fg(Color::Green),
        EmployeeStatus::Working | EmployeeStatus::Starting => Style::new().fg(Color::Cyan),
        EmployeeStatus::WaitingOnStakeholder => Style::new().fg(Color::Yellow),
        EmployeeStatus::Failed => Style::new().fg(Color::Red),
        EmployeeStatus::Offline => Style::new().fg(Color::DarkGray),
    }
}

/// Tiles per row and rows for `n` people: about the square root each way.
pub(crate) fn grid_dims(n: usize) -> (usize, usize) {
    let n = n.max(1);
    let cols = (1..=n).find(|c| c * c >= n).unwrap_or(1);
    (cols, n.div_ceil(cols))
}

/// The tile of each of `n` people in `area`, row by row.
pub(crate) fn grid_rects(area: Rect, n: usize) -> Vec<Rect> {
    let (cols, rows) = grid_dims(n);
    let row_areas = Layout::vertical(vec![Constraint::Ratio(1, rows as u32); rows]).split(area);
    row_areas
        .iter()
        .flat_map(|row| {
            Layout::horizontal(vec![Constraint::Ratio(1, cols as u32); cols])
                .split(*row)
                .to_vec()
        })
        .take(n)
        .collect()
}

impl App {
    /// ` · what they are doing`, only when there is something beyond the status.
    fn working_on_suffix(&self, e: &EmployeeView) -> String {
        let on = self.state.working_on(&e.employee.id);
        if on == e.status.label() {
            String::new()
        } else {
            format!(" · {on}")
        }
    }

    /// The Team tab as tiles: each hire's name, role, status, what they work on and the
    /// tail of their session.
    fn render_grid(&self, area: Rect, buf: &mut Buffer) {
        let rects = grid_rects(area, self.state.employees.len());
        for (i, (e, rect)) in self.state.employees.iter().zip(rects).enumerate() {
            let selected = i == self.selected;
            let block = Block::bordered()
                .border_set(border::ROUNDED)
                .border_style(if selected {
                    Style::new().cyan()
                } else {
                    Style::new().dark_gray()
                })
                .title(Line::from(vec![
                    Span::styled(" ● ", status_style(e.status)),
                    Span::styled(
                        format!("{} · {} ", e.employee.name, e.employee.role_title),
                        Style::new().add_modifier(Modifier::BOLD),
                    ),
                ]));
            let inner = block.inner(rect);
            block.render(rect, buf);
            if inner.height == 0 {
                continue;
            }
            let [head, tail] =
                Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
            Line::from(self.state.working_on(&e.employee.id))
                .yellow()
                .render(head, buf);
            let items = self.state.transcript(&e.employee.id);
            let mut lines = crate::transcript::transcript_lines(
                &items,
                tail.width.max(10) as usize,
                crate::transcript::Look {
                    output_lines: 2,
                    compact: true,
                },
            );
            if lines.is_empty() && e.last_progress.is_some() {
                lines.push(Line::from(crate::live::empty_transcript_text(e)).dark_gray());
            }
            let skip = lines.len().saturating_sub(tail.height as usize);
            lines.drain(..skip);
            Paragraph::new(lines).render(tail, buf);
        }
    }
}

/// The status word to show: an idle person the stakeholder stopped reads "stopped".
fn status_label(e: &EmployeeView) -> &'static str {
    if e.stopped && e.status == EmployeeStatus::Idle {
        "stopped"
    } else {
        e.status.label()
    }
}

fn employee_line(e: &EmployeeView, on: Option<String>) -> Line<'static> {
    let on = on.map(|on| Span::styled(format!(" — {on}"), Style::new().dark_gray()));
    let mut spans = vec![
        Span::styled("● ", status_style(e.status)),
        Span::styled(
            e.employee.name.clone(),
            Style::new().add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(" · {} · ", e.employee.role_title)),
        Span::styled(status_label(e), status_style(e.status)),
    ];
    spans.extend(on);
    Line::from(spans)
}

fn inbox_title(app: &App, item: &InboxItem) -> String {
    match item {
        InboxItem::Permission {
            id,
            employee,
            title,
            ..
        } => format!("{id} 🔒 {} wants to: {title}", app.name(employee)),
        InboxItem::Question {
            id,
            employee,
            question,
            ..
        } => {
            format!(
                "{id} ❓ {} asks: {}",
                app.name(employee),
                question.lines().next().unwrap_or("")
            )
        }
        InboxItem::SprintGate { id, sprint } => format!("{id} 🏁 sprint {sprint} awaits approval"),
        InboxItem::Alert { id, title, .. } => format!("{id} ⚠ {title}"),
        InboxItem::ReleaseGate { id, release } => {
            format!("{id} 🚀 release {release} awaits approval")
        }
        InboxItem::EnvAlert { id, env, step, .. } => {
            format!("{id} ⚠ {env} {step:?} failed")
        }
    }
}

fn event_line(e: &chillax_core::Event) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{:>5} ", e.seq), Style::new().dark_gray()),
        Span::styled(
            format!("{} ", e.ts.format("%H:%M:%S")),
            Style::new().dark_gray(),
        ),
        Span::styled(format!("{:<24}", e.kind.name()), Style::new().cyan()),
        Span::raw(e.kind.summary()),
    ])
}

fn render_form(area: Rect, buf: &mut Buffer, form: &TicketForm) {
    let mut lines: Vec<Line> = Vec::new();
    let mut focus_rows = (0, 0);
    let text_width = (72u16.min(area.width).saturating_sub(2) as usize).max(10);
    for field in form.fields() {
        let focused = form.focus == field;
        let first_row = lines.len();
        let (label, value) = match field {
            Field::Kind => ("Kind", format!("◀ {} ▶", form.kind.label())),
            Field::Title => ("Title", form.title.clone()),
            Field::Description => ("Description", form.description.clone()),
            Field::Criteria => ("Acceptance criteria (one per line)", form.criteria.clone()),
            Field::Estimate => ("Estimate (points)", form.estimate.clone()),
            Field::Epic => (
                "Epic",
                format!(
                    "◀ {} ▶",
                    form.epic
                        .as_ref()
                        .map(|id| {
                            match form.epics.iter().find(|(e, _)| e == id) {
                                Some((_, title)) => format!("{id} {title}"),
                                None => id.clone(),
                            }
                        })
                        .unwrap_or_else(|| "none".into())
                ),
            ),
        };
        let label = Line::from(label);
        lines.push(if focused {
            label.cyan().bold()
        } else {
            label.bold()
        });
        let cursor = if focused && !matches!(field, Field::Kind | Field::Epic) {
            "▏"
        } else {
            ""
        };
        let value = format!("{value}{cursor}");
        lines.extend(wrap(&value, text_width).into_iter().map(Line::from));
        if focused {
            focus_rows = (first_row, lines.len());
        }
        lines.push(Line::from(""));
    }
    let title = match &form.target {
        FormTarget::Create => " New work item ".to_string(),
        FormTarget::Edit { id } => format!(" Edit {id} "),
    };
    // Border plus one reserved row for the error, so a long form clips its fields
    // rather than the validation message.
    let popup = centered(area, 72, (lines.len() as u16 + 3).min(area.height));
    Clear.render(popup, buf);
    let block = Block::bordered()
        .title(title)
        .border_set(border::ROUNDED)
        .border_style(Style::new().cyan());
    let inner = block.inner(popup);
    block.render(popup, buf);
    let [body, footer] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);
    // A form taller than the screen scrolls to keep the focused field in view.
    let height = body.height as usize;
    form.top
        .reveal(focus_rows.1.saturating_sub(1), lines.len(), height);
    let top = form.top.reveal(focus_rows.0, lines.len(), height);
    Paragraph::new(lines)
        .scroll((top as u16, 0))
        .render(body, buf);
    if let Some(error) = &form.error {
        Line::from(error.clone()).red().bold().render(footer, buf);
    }
}

impl App {
    fn render_release_editor(&self, area: Rect, buf: &mut Buffer, edit: &crate::ReleaseEdit) {
        let popup = centered(
            area,
            area.width.saturating_sub(4).min(90),
            area.height.saturating_sub(4).min(22),
        );
        Clear.render(popup, buf);
        let label = |name: &'static str, active: bool| {
            if active {
                Line::from(name).cyan().bold()
            } else {
                Line::from(name).dark_gray()
            }
        };
        let cursor = |field: usize| if edit.field == field { "▏" } else { "" };
        let mut lines = vec![
            label("Version", edit.field == 0),
            Line::from(format!("{}{}", edit.version, cursor(0))),
            Line::from(""),
            label(
                "Notes (prepended to CHANGELOG.md, used as the tag message)",
                edit.field == 1,
            ),
        ];
        let notes = format!("{}{}", edit.notes, cursor(1));
        lines.extend(notes.split('\n').map(|l| Line::from(l.to_string())));
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(
                Block::bordered()
                    .title(" Approve release ")
                    .border_set(border::ROUNDED)
                    .border_style(Style::new().cyan()),
            )
            .render(popup, buf);
    }
}

/// The request editor: free text, then the "start now" switch.
/// The request popup split into its title box, the editor area inside it, and the rows
/// above and below the editor.
fn request_regions(area: Rect) -> (Rect, Rect, Rect, Rect) {
    let popup = centered(
        area,
        area.width.saturating_sub(4).min(80),
        area.height.saturating_sub(4).min(16),
    );
    let inner = Block::bordered().inner(popup);
    let [head, edit, foot] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .areas(inner);
    (popup, head, edit, foot)
}

fn render_request_editor(area: Rect, buf: &mut Buffer, text: &Editor, start_now: bool) {
    let (popup, head, edit, foot) = request_regions(area);
    Clear.render(popup, buf);
    Block::bordered()
        .title(" New request ")
        .border_set(border::ROUNDED)
        .border_style(Style::new().cyan())
        .render(popup, buf);
    Paragraph::new(vec![
        Line::from("What do you need? The project manager files it and picks the owner.")
            .dark_gray(),
        Line::from(""),
    ])
    .wrap(Wrap { trim: false })
    .render(head, buf);
    EditorView(text).render(edit, buf);
    Paragraph::new(vec![
        Line::from(""),
        Line::from(format!(
            "[{}] Start now (joins the active sprint without waiting for approval)",
            if start_now { "x" } else { " " }
        )),
    ])
    .render(foot, buf);
}

/// Where the terminal cursor goes: inside the Chat input while composing, or in the
/// request popup.
pub(crate) fn cursor_position(app: &App, area: Rect) -> Option<ratatui::layout::Position> {
    match &app.mode {
        Mode::Compose if app.tab == Tab::Chat => {
            let [_, body, _] = screen(area);
            let [_, main] = chat_columns(body);
            let [_, input] = chat_main(app, main);
            app.state.employees.get(app.selected)?;
            Some(app.draft.cursor_position(Block::bordered().inner(input)))
        }
        Mode::Request { text, .. } => Some(text.cursor_position(request_regions(area).2)),
        _ => None,
    }
}

/// The comment box: the text, then the image paths (Tab switches).
fn render_comment(area: Rect, buf: &mut Buffer, id: &str, text: &str, files: &str, field: usize) {
    let popup = centered(area, 60, 11);
    Clear.render(popup, buf);
    let cursor = |active: bool| if active { "▏" } else { "" };
    let mut lines = vec![Line::from("Comment").bold()];
    lines.extend(
        format!("{text}{}", cursor(field == 0))
            .split('\n')
            .map(|l| Line::from(l.to_string())),
    );
    lines.push(Line::from(""));
    lines.push(Line::from("Images (file paths, comma separated)").bold());
    lines.push(Line::from(format!("{files}{}", cursor(field == 1))));
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(
            Block::bordered()
                .title(format!(" Comment on {id} "))
                .border_set(border::ROUNDED)
                .border_style(Style::new().cyan()),
        )
        .render(popup, buf);
}

fn render_prompt(area: Rect, buf: &mut Buffer, title: &str, text: &str) {
    let popup = centered(area, 60, 7);
    Clear.render(popup, buf);
    Paragraph::new(Text::from(format!("{text}▏")))
        .wrap(Wrap { trim: false })
        .block(
            Block::bordered()
                .title(title.to_string())
                .border_set(border::ROUNDED)
                .border_style(Style::new().cyan()),
        )
        .render(popup, buf);
}

pub(crate) fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// Greedy word wrap by display width; long words are split.
pub(crate) fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for raw in text.split('\n') {
        let mut line = String::new();
        for word in raw.split(' ') {
            let candidate = if line.is_empty() {
                word.to_string()
            } else {
                format!("{line} {word}")
            };
            if candidate.width() <= width {
                line = candidate;
                continue;
            }
            if !line.is_empty() {
                out.push(std::mem::take(&mut line));
            }
            let mut rest = word.to_string();
            while rest.width() > width {
                let mut cut = String::new();
                for c in rest.chars() {
                    if (cut.clone() + &c.to_string()).width() > width {
                        break;
                    }
                    cut.push(c);
                }
                if cut.is_empty() {
                    break;
                }
                rest = rest[cut.len()..].to_string();
                out.push(cut);
            }
            line = rest;
        }
        out.push(line);
    }
    out
}
