//! The Agent live view: what one employee's agent is doing right now —
//! the ticket and branch they are on, their plan, and the session as a transcript (messages
//! and tool calls with their output folded under them).

use chillax_core::engine::EmployeeView;
use chillax_core::{PlanEntry, Ticket};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
    StatefulWidget, Widget,
};

use chillax_core::EmployeeStatus;

use crate::App;
use crate::transcript::{EXPANDED_LINES, FOLDED_LINES, Look, transcript_lines};
use crate::view::{panel, status_style};

impl App {
    /// The ticket an employee is building or reviewing right now.
    pub(crate) fn current_ticket(&self, employee: &str) -> Option<&Ticket> {
        self.state.current_ticket(employee)
    }

    pub(crate) fn render_live(&self, area: Rect, buf: &mut Buffer) {
        let [people, main] =
            Layout::horizontal([Constraint::Length(30), Constraint::Min(0)]).areas(area);
        let items: Vec<ListItem> = self
            .state
            .employees
            .iter()
            .map(|e| {
                let on = match self.current_ticket(&e.employee.id) {
                    Some(t) => format!("  {} {}", t.id, t.branch.as_deref().unwrap_or("")),
                    None => format!("  {}", e.status.label()),
                };
                ListItem::new(vec![
                    Line::from(vec![
                        Span::styled("● ", status_style(e.status)),
                        Span::raw(e.employee.name.clone()),
                    ]),
                    Line::from(on).dark_gray(),
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

        let Some(employee) = self.state.employees.get(self.selected) else {
            Paragraph::new("Nobody to watch yet. Hire someone first (Team tab, h).")
                .dark_gray()
                .block(panel("Live"))
                .render(main, buf);
            return;
        };
        let live = self
            .state
            .live
            .get(&employee.employee.id)
            .cloned()
            .unwrap_or_default();
        let plan_height = (live.plan.len().max(1) + 2).min(PLAN_ROWS as usize + 2) as u16;
        let [head, plan, session] = Layout::vertical([
            // Agent, ticket, its facts and the workflow strip, plus the border.
            Constraint::Length(6),
            Constraint::Length(plan_height),
            Constraint::Min(0),
        ])
        .areas(main);

        Paragraph::new(self.live_header(employee))
            .block(panel(&format!(
                "{} · {}",
                employee.employee.name, employee.employee.role_title
            )))
            .render(head, buf);

        Paragraph::new(plan_lines(&live.plan))
            .block(panel("Plan"))
            .render(plan, buf);

        let items = self.state.transcript(&employee.employee.id);
        let block = panel("Session");
        let inner = block.inner(session);
        let width = inner.width.saturating_sub(2).max(10) as usize;
        let lines = if items.is_empty() {
            vec![Line::from(empty_transcript_text(employee)).dark_gray()]
        } else {
            transcript_lines(
                &items,
                width,
                Look {
                    output_lines: if self.expanded {
                        EXPANDED_LINES
                    } else {
                        FOLDED_LINES
                    },
                    compact: false,
                },
            )
        };
        let height = inner.height as usize;
        let scroll = self.note_scroll_max(lines.len().saturating_sub(height)) as usize;
        let bottom = lines.len().saturating_sub(scroll);
        let top = bottom.saturating_sub(height);
        let position = if scroll == 0 {
            " following ".to_string()
        } else {
            format!(" ↑ {top} more · ↓ {scroll} newer ")
        };
        block
            .title_bottom(Line::from(position).dark_gray().right_aligned())
            .render(session, buf);
        let [text, bar] =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(1)]).areas(inner);
        Paragraph::new(lines[top..bottom].to_vec()).render(text, buf);
        if lines.len() > height {
            let mut state = ScrollbarState::new(lines.len() - height).position(top);
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .render(bar, buf, &mut state);
        }
    }

    fn live_header(&self, employee: &EmployeeView) -> Vec<Line<'static>> {
        let status = Span::styled(employee.status.label(), status_style(employee.status));
        let total = self.state.employees.len();
        let who = Line::from(vec![
            Span::raw(format!("agent {} of {total} · ", self.selected + 1)),
            status,
        ]);
        let Some(ticket) = self.current_ticket(&employee.employee.id) else {
            return vec![who, Line::from("Not on a ticket.").dark_gray()];
        };
        let mut facts = Vec::new();
        if let Some(branch) = &ticket.branch {
            facts.push(branch.clone());
        }
        if let (Some(number), Some(url)) = (ticket.pr_number, &ticket.pr_url) {
            facts.push(format!("PR #{number} {url}"));
        }
        if ticket.checks.contains("tests_pass") {
            facts.push("tests ✓".into());
        }
        if let Some(reviewer) = &ticket.reviewer {
            facts.push(format!("reviewer {reviewer}"));
        }
        let mut strip: Vec<Span> = Vec::new();
        for (i, key) in self.state.workflow_states.iter().enumerate() {
            if i > 0 {
                strip.push(Span::styled(" › ", Style::new().dark_gray()));
            }
            let label = key.replace('_', " ");
            strip.push(if *key == ticket.state {
                Span::styled(format!("[{label}]"), Style::new().bold().cyan())
            } else {
                Span::styled(label, Style::new().dark_gray())
            });
        }
        vec![
            who,
            Line::from(vec![
                Span::styled(format!("{} ", ticket.id), Style::new().bold()),
                Span::raw(ticket.title.clone()),
            ]),
            Line::from(facts.join(" · ")).dark_gray(),
            Line::from(strip),
        ]
    }
}

/// Rows the Plan panel shows.
const PLAN_ROWS: u16 = 7;

/// The plan, at most `PLAN_ROWS` lines: the window follows the current step and a last
/// line says how many steps are not shown.
fn plan_lines(plan: &[PlanEntry]) -> Vec<Line<'static>> {
    if plan.is_empty() {
        return vec![Line::from("No plan reported yet.").dark_gray()];
    }
    let rows = PLAN_ROWS as usize;
    if plan.len() <= rows {
        return plan.iter().map(plan_line).collect();
    }
    let shown = rows - 1;
    let active = plan
        .iter()
        .position(|p| p.status == "in_progress")
        .or_else(|| plan.iter().position(|p| p.status != "completed"))
        .unwrap_or(0);
    let start = active.saturating_sub(1).min(plan.len() - shown);
    let mut lines: Vec<_> = plan[start..start + shown].iter().map(plan_line).collect();
    lines.push(Line::from(format!("+{} more", plan.len() - shown)).dark_gray());
    lines
}

fn plan_line(entry: &PlanEntry) -> Line<'static> {
    let (glyph, style) = match entry.status.as_str() {
        "completed" => ("✓", Style::new().green()),
        "in_progress" => ("▶", Style::new().yellow()),
        _ => ("·", Style::new().dark_gray()),
    };
    Line::from(vec![
        Span::styled(format!("{glyph} "), style),
        Span::raw(entry.content.clone()),
    ])
}

/// What an empty transcript says. An employee who has reported progress and is no longer
/// working had their context cleared when the work ended (ADR 0008); anyone else has
/// simply not started.
pub(crate) fn empty_transcript_text(e: &EmployeeView) -> &'static str {
    if e.status != EmployeeStatus::Working && e.last_progress.is_some() {
        "Context cleared — idle"
    } else {
        "Nothing yet."
    }
}
