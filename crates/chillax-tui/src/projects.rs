//! The project switcher overlay: lists the Chillax instances running on this
//! machine and opens the chosen one's dashboard in the browser. The TUI stays attached to
//! its own project (ADR 0017). The list never blocks the render loop: the key handler
//! asks for a load, the run loop calls the [`chillax_core::InstanceSource`] in a task and
//! hands the answer to [`ProjectList::loaded`].

use chillax_core::InstanceInfo;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::link::put_link;

/// Rows (three lines each) shown at once.
const ROW_LINES: u16 = 3;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProjectList {
    /// `None` while loading.
    pub items: Option<Vec<InstanceInfo>>,
    pub selected: usize,
    /// Root of the project this TUI is attached to, marked in the list.
    pub current: Option<String>,
}

/// What a key press did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectPick {
    Pending,
    Cancelled,
    /// Open this dashboard URL.
    Open(String),
}

impl ProjectList {
    pub fn new(current: Option<String>) -> Self {
        Self {
            items: None,
            selected: 0,
            current,
        }
    }

    pub fn is_loading(&self) -> bool {
        self.items.is_none()
    }

    /// The registry answered. The cursor starts on the first project that is not this one.
    pub fn loaded(&mut self, items: Vec<InstanceInfo>) {
        self.selected = items
            .iter()
            .position(|i| Some(&i.root) != self.current.as_ref())
            .unwrap_or(0);
        self.items = Some(items);
    }

    pub fn key(&mut self, key: KeyEvent) -> ProjectPick {
        let len = self.items.as_ref().map_or(0, Vec::len);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return ProjectPick::Cancelled,
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(len.saturating_sub(1));
            }
            KeyCode::Enter if self.can_open() => {
                if let Some(item) = self.items.as_ref().and_then(|v| v.get(self.selected)) {
                    return ProjectPick::Open(item.url.clone());
                }
            }
            _ => {}
        }
        ProjectPick::Pending
    }

    /// Enter opens something only when a project is listed.
    pub fn can_open(&self) -> bool {
        self.others() > 0
    }

    /// Listed projects other than this TUI's own.
    fn others(&self) -> usize {
        self.items.as_ref().map_or(0, |items| {
            items
                .iter()
                .filter(|i| Some(&i.root) != self.current.as_ref())
                .count()
        })
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer, links: bool) {
        let Some(items) = &self.items else {
            Line::from("Looking for running projects…")
                .dark_gray()
                .render(area, buf);
            return;
        };
        // Only this project (or none) is running: nothing to switch to.
        if self.others() == 0 {
            Paragraph::new("No other projects running. Start one with `chillax` in its folder.")
                .dark_gray()
                .render(area, buf);
            return;
        }
        let visible = usize::from((area.height / ROW_LINES).max(1));
        let top = (self.selected + 1).saturating_sub(visible);
        for (n, item) in items.iter().enumerate().skip(top).take(visible) {
            let y = area.y + u16::try_from(n - top).unwrap_or(0) * ROW_LINES;
            if y + ROW_LINES > area.y + area.height {
                break;
            }
            let here = Some(&item.root) == self.current.as_ref();
            let picked = n == self.selected;
            let marker = if picked { "▶ " } else { "  " };
            let mut head = vec![Span::raw(marker), Span::raw(item.name.clone()).bold()];
            if here {
                head.push(Span::raw("  (this project)").green());
            }
            let head = Line::from(head);
            let head = if picked { head.cyan() } else { head };
            head.render(Rect::new(area.x, y, area.width, 1), buf);
            Line::from(format!("  {}", item.root))
                .dark_gray()
                .render(Rect::new(area.x, y + 1, area.width, 1), buf);
            put_link(
                buf,
                (area.x + 2, y + 2),
                area.width.saturating_sub(2),
                &item.url,
                &item.url,
                Style::new().blue().underlined(),
                links,
            );
        }
    }
}
