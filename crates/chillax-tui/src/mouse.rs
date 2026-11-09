//! Mouse handling. Hit-testing reuses the layout helpers from `view`, so a click lands on
//! what was drawn. Only left clicks and the wheel are handled.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};

use crate::env::{env_panel_split, env_panels};
use crate::view::{
    board_areas, board_columns, chat_columns, chat_main, dashboard_areas, grid_rects, inbox_areas,
    panel_inner, screen, tab_rects,
};
use crate::{App, Mode, Tab};

/// Lines moved per wheel notch in logs and board columns.
const WHEEL_LINES: u16 = 3;

/// A vertical list whose rows are `item_height` cells tall.
struct ListHit {
    inner: Rect,
    item_height: u16,
    selected: usize,
    len: usize,
}

impl ListHit {
    /// Row under `pos`. A fresh `ListState` scrolls just far enough to show the
    /// selection, which is how the views draw their lists.
    fn row_at(&self, pos: Position) -> Option<usize> {
        if !self.inner.contains(pos) {
            return None;
        }
        let visible = (self.inner.height / self.item_height).max(1) as usize;
        let offset = (self.selected + 1).saturating_sub(visible);
        let row = offset + ((pos.y - self.inner.y) / self.item_height) as usize;
        (row < self.len).then_some(row)
    }
}

impl App {
    /// Handle one mouse event on a screen of `area`. Mouse input never produces commands:
    /// it navigates, like the arrow keys and Tab, and a click on a running app only queues
    /// an open request for the browser.
    pub fn handle_mouse_event(&mut self, mouse: MouseEvent, area: Rect) {
        let pos = Position::new(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) if self.mode == Mode::Normal => {
                self.status = None;
                self.click(pos, area);
            }
            // While typing in Chat, a click inside the box moves the cursor.
            MouseEventKind::Down(MouseButton::Left)
                if self.mode == Mode::Compose && self.tab == Tab::Chat =>
            {
                let [_, body, _] = screen(area);
                let [_, main] = chat_columns(body);
                let [_, input] = chat_main(self, main);
                let inner = panel_inner(input);
                if inner.contains(pos) {
                    self.draft.click(inner, pos);
                }
            }
            MouseEventKind::ScrollUp if self.accepts_wheel() => self.wheel(pos, area, true),
            MouseEventKind::ScrollDown if self.accepts_wheel() => self.wheel(pos, area, false),
            _ => {}
        }
    }

    /// Popups cover the views, so only plain browsing and composing let the wheel through.
    fn accepts_wheel(&self) -> bool {
        matches!(self.mode, Mode::Normal | Mode::Compose)
    }

    /// Index of the tab drawn under `pos`, if any.
    pub fn tab_at(&self, pos: Position, area: Rect) -> Option<usize> {
        let [header, ..] = screen(area);
        tab_rects(self, header).iter().position(|r| r.contains(pos))
    }

    fn click(&mut self, pos: Position, area: Rect) {
        if let Some(index) = self.tab_at(pos, area) {
            self.switch_tab(index);
            return;
        }
        let [_, body, _] = screen(area);
        if self.tab == Tab::Team && self.grid {
            let tiles = grid_rects(body, self.state.employees.len());
            if let Some(i) = tiles.iter().position(|r| r.contains(pos)) {
                self.selected = i;
                self.switch_tab(Tab::Live.index());
            }
            return;
        }
        if self.tab == Tab::Board {
            self.click_board(pos, body);
            return;
        }
        if self.tab == Tab::Environments {
            self.click_environments(pos, body);
            return;
        }
        if self.tab == Tab::Dashboard {
            self.click_dashboard(pos, body);
            return;
        }
        if let Some(hit) = self.list_hit(pos, body)
            && let Some(row) = hit.row_at(pos)
        {
            self.select(row);
        }
    }

    /// A click on an app selects it; a click on its address row, or on the app that is
    /// already selected, opens it when it runs.
    fn click_environments(&mut self, pos: Position, body: Rect) {
        let panels = env_panels(body);
        for (env, panel) in ["dev", "qa", "prod"].into_iter().zip(panels) {
            let Some(index) = self.state.apps.iter().position(|a| a.env == env) else {
                continue;
            };
            let [_, area] = env_panel_split(panel_inner(panel), true);
            if !area.contains(pos) {
                continue;
            }
            let was_selected = self
                .selected_app()
                .is_some_and(|selected| selected.env == env);
            self.app_selected = index;
            if was_selected || pos.y > area.y {
                self.request_open_selected_app();
            }
            return;
        }
    }

    /// A click on a running app's row in the Dashboard opens its address.
    fn click_dashboard(&mut self, pos: Position, body: Rect) {
        let [_, _, apps, ..] = dashboard_areas(self, body);
        let inner = panel_inner(apps);
        if !inner.contains(pos) {
            return;
        }
        let Some(row) = self
            .state
            .running_apps()
            .into_iter()
            .nth(usize::from(pos.y - inner.y))
            .filter(|a| a.is_running())
        else {
            return;
        };
        if let Ok((url, _)) = chillax_core::apps::normalize_reported_url(&row.url) {
            self.status = Some(format!("Opening {url}"));
            self.open_requests.push(std::path::PathBuf::from(url));
        }
    }

    /// A click on a Board card selects it and enters the board.
    fn click_board(&mut self, pos: Position, body: Rect) {
        let [_, board] = board_areas(self, body);
        let columns = board_columns(self, board);
        for (i, (key, col)) in self.state.workflow_states.iter().zip(columns).enumerate() {
            let inner = panel_inner(col);
            if !inner.contains(pos) {
                continue;
            }
            let (lines, cards) = self.board_cards(key, inner.width);
            let max = lines.len().saturating_sub(inner.height as usize) as u16;
            let offset = self.board_scroll.get(i).copied().unwrap_or(0).min(max) as usize;
            let line = (pos.y - inner.y) as usize + offset;
            if let Some((id, ..)) = cards.into_iter().find(|(_, s, e)| (*s..*e).contains(&line)) {
                self.select_board(Some(id));
                self.board_focus = true;
            }
            return;
        }
    }

    /// The list under `pos` for the current tab, if it has one.
    fn list_hit(&self, pos: Position, body: Rect) -> Option<ListHit> {
        let employees = self.state.employees.len();
        let people = |inner, selected| ListHit {
            inner,
            item_height: 2,
            selected,
            len: employees,
        };
        match self.tab {
            Tab::Team if !self.grid => Some(people(panel_inner(body), self.selected)),
            Tab::Chat => {
                let [list, _] = chat_columns(body);
                Some(people(panel_inner(list), self.selected))
            }
            Tab::Inbox => {
                let [list, _] = inbox_areas(body);
                Some(ListHit {
                    inner: panel_inner(list),
                    item_height: 1,
                    selected: self.inbox_selected,
                    len: self.state.inbox.len(),
                })
            }
            _ => None,
        }
        .filter(|hit| hit.inner.contains(pos))
    }

    /// Select a row in the current tab's list, as the arrow keys do.
    fn select(&mut self, row: usize) {
        match self.tab {
            Tab::Team | Tab::Chat => {
                self.selected = row;
                self.scroll = 0;
            }
            Tab::Inbox => self.inbox_selected = row,
            _ => {}
        }
    }

    fn wheel(&mut self, pos: Position, area: Rect, up: bool) {
        let [_, body, _] = screen(area);
        // Lists move the selection one row, like Up/Down; logs and columns scroll.
        if self.list_hit(pos, body).is_some() {
            let (current, len) = match self.tab {
                Tab::Inbox => (self.inbox_selected, self.state.inbox.len()),
                _ => (self.selected, self.state.employees.len()),
            };
            let next = if up {
                current.saturating_sub(1)
            } else {
                (current + 1).min(len.saturating_sub(1))
            };
            self.select(next);
            return;
        }
        match self.tab {
            Tab::Chat => {
                let [_, main] = chat_columns(body);
                let [log, _] = chat_main(self, main);
                if !log.contains(pos) {
                    return;
                }
                let Some(employee) = self.state.employees.get(self.selected) else {
                    return;
                };
                let inner = panel_inner(log);
                let total = self.chat_lines(employee, inner.width).len();
                let max = total.saturating_sub(inner.height as usize) as u16;
                self.scroll = step(self.scroll, up, max);
            }
            Tab::Events => {
                if !body.contains(pos) {
                    return;
                }
                let max = self
                    .events
                    .len()
                    .saturating_sub(panel_inner(body).height as usize)
                    as u16;
                self.scroll = step(self.scroll, up, max);
            }
            Tab::Board => {
                let [_, board] = board_areas(self, body);
                let columns = board_columns(self, board);
                let Some(index) = columns.iter().position(|c| c.contains(pos)) else {
                    return;
                };
                let inner = panel_inner(columns[index]);
                let key = &self.state.workflow_states[index];
                let total = self.board_column_lines(key, inner.width).len();
                let max = total.saturating_sub(inner.height as usize) as u16;
                if self.board_scroll.len() <= index {
                    self.board_scroll.resize(index + 1, 0);
                }
                self.board_scroll[index] = step(self.board_scroll[index], !up, max);
            }
            _ => {}
        }
    }
}

/// Move a scroll offset by one wheel notch, within `0..=max`. For the chat and event
/// log the offset counts lines up from the bottom, so wheel up increases it.
fn step(offset: u16, up: bool, max: u16) -> u16 {
    if up {
        offset.saturating_add(WHEEL_LINES).min(max)
    } else {
        offset.saturating_sub(WHEEL_LINES)
    }
}
