//! A small multi-line text editor shared by the Chat composer and the request editor.
//!
//! The text is a `Vec<char>` and the cursor a char index, so every edit is unicode-safe.
//! Wrapping is done here (not by `Paragraph`) so the cursor maths and the drawn text always
//! agree; drawing and the terminal cursor both come from [`Editor::layout`].

use std::sync::atomic::{AtomicUsize, Ordering};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};
use unicode_width::UnicodeWidthChar;

/// Numbers the renderer learns about (box width, first visible row). Drawing takes `&self`,
/// so they sit in atomics; they never change what the text is, so they are ignored by
/// `Clone`'s meaning and by equality.
#[derive(Debug, Default)]
struct Seen {
    width: AtomicUsize,
    top: AtomicUsize,
}

impl Clone for Seen {
    fn clone(&self) -> Self {
        Self {
            width: AtomicUsize::new(self.width.load(Ordering::Relaxed)),
            top: AtomicUsize::new(self.top.load(Ordering::Relaxed)),
        }
    }
}

impl PartialEq for Seen {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}
impl Eq for Seen {}

/// Width used for Up/Down before anything was drawn.
const DEFAULT_WIDTH: usize = 80;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Editor {
    chars: Vec<char>,
    /// Char index in `0..=chars.len()`.
    cursor: usize,
    /// The display column Up/Down try to keep while moving across short lines.
    goal: Option<usize>,
    seen: Seen,
}

/// One visual row: the chars `start..end` of the text (a hard newline is not part of it).
type Row = (usize, usize);

fn cw(c: char) -> usize {
    c.width().unwrap_or(0)
}

impl Editor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_text(text: &str) -> Self {
        let mut e = Self::new();
        e.insert_str(text);
        e
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    /// True when there is nothing but whitespace (nothing worth sending).
    pub fn is_blank(&self) -> bool {
        self.chars.iter().all(|c| c.is_whitespace())
    }

    /// Hand back the text and start over.
    pub fn take(&mut self) -> String {
        let text = self.text();
        self.chars.clear();
        self.cursor = 0;
        self.goal = None;
        self.seen.top.store(0, Ordering::Relaxed);
        text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Tell the editor how wide its box is, for Up/Down (drawing does this too).
    pub fn set_width(&self, width: u16) {
        self.seen.width.store(width as usize, Ordering::Relaxed);
    }

    fn width(&self) -> usize {
        match self.seen.width.load(Ordering::Relaxed) {
            0 => DEFAULT_WIDTH,
            w => w,
        }
    }

    // ---- editing -------------------------------------------------------------------

    pub fn insert_char(&mut self, c: char) {
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
        self.goal = None;
    }

    /// Insert text verbatim, turning CRLF and lone CR into `\n`.
    pub fn insert_str(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        for c in text.chars() {
            self.insert_char(c);
        }
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
        self.goal = None;
    }

    pub fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
        self.goal = None;
    }

    // ---- moving --------------------------------------------------------------------

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
        self.goal = None;
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.chars.len());
        self.goal = None;
    }

    pub fn word_left(&mut self) {
        let mut i = self.cursor;
        while i > 0 && self.chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !self.chars[i - 1].is_whitespace() {
            i -= 1;
        }
        self.cursor = i;
        self.goal = None;
    }

    pub fn word_right(&mut self) {
        let n = self.chars.len();
        let mut i = self.cursor;
        while i < n && !self.chars[i].is_whitespace() {
            i += 1;
        }
        while i < n && self.chars[i].is_whitespace() {
            i += 1;
        }
        self.cursor = i;
        self.goal = None;
    }

    pub fn home(&mut self) {
        let rows = self.layout(self.width());
        self.cursor = rows[self.row_of(&rows)].0;
        self.goal = None;
    }

    pub fn end(&mut self) {
        let rows = self.layout(self.width());
        let at = self.row_of(&rows);
        self.cursor = Self::last_index(&rows, at);
        self.goal = None;
    }

    /// Up one visual row; on the first row, to the start of the text.
    pub fn up(&mut self) {
        self.vertical(false);
    }

    /// Down one visual row; on the last row, to the end of the text.
    pub fn down(&mut self) {
        self.vertical(true);
    }

    fn vertical(&mut self, down: bool) {
        let rows = self.layout(self.width());
        let at = self.row_of(&rows);
        let goal = self.goal.unwrap_or_else(|| self.col_in(rows[at]));
        let target = if down {
            if at + 1 >= rows.len() {
                self.cursor = self.chars.len();
                self.goal = Some(goal);
                return;
            }
            at + 1
        } else {
            if at == 0 {
                self.cursor = 0;
                self.goal = Some(goal);
                return;
            }
            at - 1
        };
        self.cursor = self.index_at(&rows, target, goal);
        self.goal = Some(goal);
    }

    /// Apply an editing key. Returns false for keys it does not own (Enter without Alt,
    /// Esc, Tab, Ctrl combinations other than A and E).
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Enter if alt => self.insert_char('\n'),
            KeyCode::Left if ctrl || alt => self.word_left(),
            KeyCode::Right if ctrl || alt => self.word_right(),
            KeyCode::Char('b') if alt => self.word_left(),
            KeyCode::Char('f') if alt => self.word_right(),
            KeyCode::Left => self.left(),
            KeyCode::Right => self.right(),
            KeyCode::Up => self.up(),
            KeyCode::Down => self.down(),
            KeyCode::Home => self.home(),
            KeyCode::End => self.end(),
            KeyCode::Char('a') if ctrl => self.home(),
            KeyCode::Char('e') if ctrl => self.end(),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Char(c) if !ctrl && !alt => self.insert_char(c),
            _ => return false,
        }
        true
    }

    // ---- layout --------------------------------------------------------------------

    /// The visual rows when wrapped to `width` cells. One cell is kept free so the cursor
    /// always has room at the end of a full row. There is always at least one row.
    pub fn layout(&self, width: usize) -> Vec<Row> {
        let wrap = width.saturating_sub(1).max(1);
        let n = self.chars.len();
        let mut rows = Vec::new();
        let mut s = 0;
        loop {
            let e = self.chars[s..]
                .iter()
                .position(|c| *c == '\n')
                .map_or(n, |p| s + p);
            self.wrap_segment(s, e, wrap, &mut rows);
            if e >= n {
                break;
            }
            s = e + 1;
        }
        rows
    }

    /// Greedy word wrap of one hard line; spaces stay at the end of the row they follow.
    fn wrap_segment(&self, s: usize, e: usize, wrap: usize, rows: &mut Vec<Row>) {
        let mut row_start = s;
        let mut col = 0;
        let mut i = s;
        while i < e {
            let mut j = i;
            while j < e && self.chars[j] != ' ' {
                j += 1;
            }
            let mut k = j;
            while k < e && self.chars[k] == ' ' {
                k += 1;
            }
            let word: usize = self.chars[i..j].iter().map(|c| cw(*c)).sum();
            if col > 0 && col + word > wrap {
                rows.push((row_start, i));
                row_start = i;
                col = 0;
            }
            if word > wrap {
                for idx in i..j {
                    let w = cw(self.chars[idx]);
                    if col > 0 && col + w > wrap {
                        rows.push((row_start, idx));
                        row_start = idx;
                        col = 0;
                    }
                    col += w;
                }
            } else {
                col += word;
            }
            col += k - j;
            i = k;
        }
        rows.push((row_start, e));
    }

    /// The row holding the cursor: the last one starting at or before it.
    fn row_of(&self, rows: &[Row]) -> usize {
        rows.iter().rposition(|r| r.0 <= self.cursor).unwrap_or(0)
    }

    fn col_in(&self, row: Row) -> usize {
        self.chars[row.0..self.cursor.clamp(row.0, row.1)]
            .iter()
            .map(|c| cw(*c))
            .sum()
    }

    /// The last cursor index that is drawn on row `at` (not the start of the next row).
    fn last_index(rows: &[Row], at: usize) -> usize {
        let (start, end) = rows[at];
        match rows.get(at + 1) {
            Some(next) if next.0 == end => end.saturating_sub(1).max(start),
            _ => end,
        }
    }

    /// The char index on row `at` nearest to, but not past, display column `col`.
    fn index_at(&self, rows: &[Row], at: usize, col: usize) -> usize {
        let (start, _) = rows[at];
        let last = Self::last_index(rows, at);
        let mut x = 0;
        let mut i = start;
        while i < last {
            let w = cw(self.chars[i]);
            if x + w > col {
                break;
            }
            x += w;
            i += 1;
        }
        i
    }

    /// Keep the cursor row inside a window of `height` rows; returns the first row shown.
    fn reveal(&self, rows: &[Row], height: usize) -> usize {
        let at = self.row_of(rows);
        let height = height.max(1);
        let mut top = self.seen.top.load(Ordering::Relaxed);
        top = top.min(rows.len().saturating_sub(height));
        if at < top {
            top = at;
        } else if at >= top + height {
            top = at + 1 - height;
        }
        self.seen.top.store(top, Ordering::Relaxed);
        top
    }

    /// Rows hidden above and below a window of `height` rows, after [`Editor::reveal`].
    pub fn hidden(&self, width: u16, height: u16) -> (usize, usize) {
        let rows = self.layout(width as usize);
        let top = self.reveal(&rows, height as usize);
        let shown = top + height as usize;
        (top, rows.len().saturating_sub(shown))
    }

    /// How many rows the text takes when wrapped to `width`.
    pub fn rows(&self, width: u16) -> usize {
        self.layout(width as usize).len()
    }

    /// Where the terminal cursor goes when the text is drawn in `area`.
    pub fn cursor_position(&self, area: Rect) -> Position {
        let rows = self.layout(area.width as usize);
        let top = self.reveal(&rows, area.height as usize);
        let at = self.row_of(&rows);
        let col = self
            .col_in(rows[at])
            .min(area.width.saturating_sub(1) as usize);
        Position::new(
            area.x + col as u16,
            area.y
                + at.saturating_sub(top)
                    .min(area.height.saturating_sub(1) as usize) as u16,
        )
    }

    /// Put the cursor where a click at `pos` landed inside `area`.
    pub fn click(&mut self, area: Rect, pos: Position) {
        let rows = self.layout(area.width as usize);
        let top = self.reveal(&rows, area.height as usize);
        let at = (top + pos.y.saturating_sub(area.y) as usize).min(rows.len() - 1);
        self.cursor = self.index_at(&rows, at, pos.x.saturating_sub(area.x) as usize);
        self.goal = None;
    }
}

/// Draws the visible rows of the editor, scrolled to keep the cursor in view.
pub struct EditorView<'a>(pub &'a Editor);

impl Widget for EditorView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let e = self.0;
        e.set_width(area.width);
        let rows = e.layout(area.width as usize);
        let top = e.reveal(&rows, area.height as usize);
        let lines: Vec<Line> = rows
            .iter()
            .skip(top)
            .take(area.height as usize)
            .map(|(s, t)| Line::from(e.chars[*s..*t].iter().collect::<String>()))
            .collect();
        Paragraph::new(lines).render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ed(text: &str, width: u16) -> Editor {
        let e = Editor::from_text(text);
        e.set_width(width);
        e
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn rows_of(e: &Editor, width: usize) -> Vec<String> {
        e.layout(width)
            .iter()
            .map(|(s, t)| e.chars[*s..*t].iter().collect())
            .collect()
    }

    #[test]
    fn inserts_and_deletes_at_the_cursor() {
        let mut e = ed("helo", 20);
        e.left();
        e.insert_char('l');
        assert_eq!(e.text(), "hello");
        e.home();
        e.delete();
        assert_eq!(e.text(), "ello");
        e.end();
        e.backspace();
        assert_eq!(e.text(), "ell");
        e.backspace();
        e.backspace();
        e.backspace();
        e.backspace();
        assert!(e.is_empty());
    }

    #[test]
    fn unicode_is_edited_by_char() {
        let mut e = ed("a日é🙂", 20);
        e.backspace();
        assert_eq!(e.text(), "a日é");
        e.left();
        e.insert_char('x');
        assert_eq!(e.text(), "a日xé");
        // A wide char takes two columns.
        let e2 = ed("日本", 20);
        let area = Rect::new(0, 0, 20, 3);
        assert_eq!(e2.cursor_position(area), Position::new(4, 0));
    }

    #[test]
    fn wraps_by_word_and_splits_long_words() {
        let e = ed("aaa bbb ccc", 8);
        assert_eq!(rows_of(&e, 8), vec!["aaa bbb ", "ccc"]);
        let e = ed("abcdefghij", 5);
        assert_eq!(rows_of(&e, 5), vec!["abcd", "efgh", "ij"]);
        let e = ed("one\n\ntwo\n", 20);
        assert_eq!(rows_of(&e, 20), vec!["one", "", "two", ""]);
        assert_eq!(rows_of(&Editor::new(), 20), vec![""]);
    }

    #[test]
    fn up_and_down_walk_wrapped_rows_and_keep_the_column() {
        let mut e = ed("aaa bbb ccc ddd", 8);
        // rows: "aaa bbb " / "ccc ddd"
        e.cursor = 0;
        e.up();
        assert_eq!(e.cursor(), 0, "first row: up goes to the start");
        e.down();
        assert_eq!(e.cursor(), 8, "column 0 of the next row");
        e.right();
        e.right();
        assert_eq!(e.cursor(), 10);
        e.up();
        assert_eq!(e.cursor(), 2);
        e.down();
        assert_eq!(e.cursor(), 10);
        e.down();
        assert_eq!(e.cursor(), 15, "last row: down goes to the end");
    }

    #[test]
    fn goal_column_survives_a_short_row() {
        let mut e = ed("abcdef\nx\nabcdef", 20);
        e.cursor = 5;
        e.down();
        assert_eq!(e.cursor(), 8, "end of the short row");
        e.down();
        assert_eq!(e.cursor(), 14, "back to column 5");
        e.up();
        e.up();
        assert_eq!(e.cursor(), 5);
    }

    #[test]
    fn home_and_end_are_per_visual_row() {
        let mut e = ed("aaa bbb ccc", 8);
        e.cursor = 9;
        e.home();
        assert_eq!(e.cursor(), 8);
        e.cursor = 2;
        e.end();
        assert_eq!(e.cursor(), 7, "end of a wrapped row stays on that row");
        e.cursor = 3;
        assert!(e.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)));
        assert_eq!(e.cursor(), 0);
        assert!(e.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL)));
        assert_eq!(e.cursor(), 7);
    }

    #[test]
    fn word_jumps() {
        let mut e = ed("one two  three", 40);
        e.word_left();
        assert_eq!(e.cursor(), 9);
        e.word_left();
        assert_eq!(e.cursor(), 4);
        e.word_right();
        assert_eq!(e.cursor(), 9);
        e.word_right();
        assert_eq!(e.cursor(), 14);
    }

    #[test]
    fn paste_normalises_line_endings() {
        let mut e = ed("a", 40);
        e.insert_str("x\r\ny\rz");
        assert_eq!(e.text(), "ax\ny\nz");
    }

    #[test]
    fn keys_belong_to_the_editor_or_not() {
        let mut e = ed("", 40);
        assert!(e.handle_key(key(KeyCode::Char('h'))));
        assert!(e.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)));
        assert!(!e.handle_key(key(KeyCode::Enter)));
        assert!(!e.handle_key(key(KeyCode::Esc)));
        assert!(!e.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)));
        assert_eq!(e.text(), "h\n");
    }

    #[test]
    fn window_follows_the_cursor() {
        let mut e = ed("1\n2\n3\n4\n5\n6", 10);
        assert_eq!(e.hidden(10, 3), (3, 0), "cursor at the end shows the tail");
        e.cursor = 0;
        assert_eq!(e.hidden(10, 3), (0, 3));
        e.cursor = 6; // row 3 ("4")
        assert_eq!(e.hidden(10, 3), (1, 2), "scrolls just far enough");
        let area = Rect::new(2, 5, 10, 3);
        assert_eq!(e.cursor_position(area), Position::new(2, 7));
    }

    #[test]
    fn click_places_the_cursor() {
        let mut e = ed("hello\nworld", 20);
        let area = Rect::new(1, 1, 20, 4);
        e.click(area, Position::new(4, 2));
        assert_eq!(e.cursor(), 9);
        e.click(area, Position::new(19, 1));
        assert_eq!(e.cursor(), 5);
        e.click(area, Position::new(5, 4));
        assert_eq!(
            e.cursor(),
            10,
            "below the text: the last row, at that column"
        );
    }
}
