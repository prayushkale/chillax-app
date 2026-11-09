//! Clickable links in the terminal (OSC 8). Ratatui has no hyperlink cell, so the escape
//! sequences go into one cell's symbol and that cell is given the width of the visible
//! text (`CellDiffOption::ForcedWidth`), which keeps the diff from counting the escape
//! characters. With links off the text is drawn plainly.

use std::num::NonZeroU16;

use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::style::Style;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// The OSC 8 sequence that makes `text` open `url`.
pub(crate) fn osc8(url: &str, text: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
}

/// Whether `url` is safe to put inside an escape sequence: no control characters.
fn embeddable(url: &str) -> bool {
    !url.chars().any(char::is_control)
}

/// `text` cut to at most `width` columns.
fn fit(text: &str, width: u16) -> String {
    let mut used = 0usize;
    let mut out = String::new();
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > usize::from(width) {
            break;
        }
        used += w;
        out.push(c);
    }
    out
}

/// Draw `text` at (`x`, `y`) in at most `max` columns, linking to `url` when `links` is on.
/// Returns the columns used.
pub(crate) fn put_link(
    buf: &mut Buffer,
    (x, y): (u16, u16),
    max: u16,
    text: &str,
    url: &str,
    style: Style,
    links: bool,
) -> u16 {
    let shown = fit(text, max);
    let width = u16::try_from(shown.width()).unwrap_or(0);
    let Some(forced) = NonZeroU16::new(width) else {
        return 0;
    };
    if !links || !embeddable(url) || width == 1 {
        buf.set_stringn(x, y, &shown, usize::from(max), style);
        return width;
    }
    for dx in 1..width {
        if let Some(cell) = buf.cell_mut((x + dx, y)) {
            cell.set_symbol(" ").set_style(style);
        }
    }
    if let Some(cell) = buf.cell_mut((x, y)) {
        cell.set_symbol(&osc8(url, &shown))
            .set_style(style)
            .set_diff_option(CellDiffOption::ForcedWidth(forced));
    }
    width
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    #[test]
    fn a_link_is_one_cell_that_spans_the_text() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 12, 1));
        let used = put_link(
            &mut buf,
            (1, 0),
            10,
            "localhost",
            "http://localhost:3000",
            Style::new().cyan(),
            true,
        );
        assert_eq!(used, 9);
        assert_eq!(
            buf[(1, 0)].symbol(),
            "\x1b]8;;http://localhost:3000\x1b\\localhost\x1b]8;;\x1b\\"
        );
        assert_eq!(
            buf[(1, 0)].diff_option,
            CellDiffOption::ForcedWidth(NonZeroU16::new(9).expect("nonzero"))
        );
        assert_eq!(buf[(2, 0)].symbol(), " ");
        assert_eq!(buf[(1, 0)].fg, ratatui::style::Color::Cyan);
        assert_eq!(buf[(2, 0)].fg, ratatui::style::Color::Cyan);
        // The link does not make the diff skip or double-write neighbours: only the link
        // cell is sent, and the cells after it are covered by its forced width.
        let blank = Buffer::empty(Rect::new(0, 0, 12, 1));
        let sent: Vec<u16> = blank.diff(&buf).iter().map(|(x, _, _)| *x).collect();
        assert_eq!(sent, vec![1]);
    }

    #[test]
    fn without_links_or_with_an_unsafe_url_the_text_is_plain() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 12, 1));
        put_link(
            &mut buf,
            (0, 0),
            12,
            "localhost",
            "http://x",
            Style::new(),
            false,
        );
        assert_eq!(buf, Buffer::with_lines(["localhost   "]));
        let mut buf = Buffer::empty(Rect::new(0, 0, 12, 1));
        put_link(
            &mut buf,
            (0, 0),
            12,
            "localhost",
            "http://x\x1b]0;evil\x07",
            Style::new(),
            true,
        );
        assert_eq!(buf, Buffer::with_lines(["localhost   "]));
    }

    #[test]
    fn long_text_is_cut_to_the_room_there_is() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 6, 1));
        let used = put_link(
            &mut buf,
            (0, 0),
            6,
            "http://localhost:3000",
            "http://localhost:3000",
            Style::new(),
            true,
        );
        assert_eq!(used, 6);
        assert!(buf[(0, 0)].symbol().ends_with("http:/\x1b]8;;\x1b\\"));
    }
}
