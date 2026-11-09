//! Copying out of the TUI: OSC 52 clipboard sequences and the text of the focused item.
//! Mouse capture blocks the terminal's own selection, so `m` (see `keys.rs`) also releases it.

use chillax_core::InboxItem;

use crate::{App, Mode, Tab};

/// Most base64 characters sent in one sequence; terminals and tmux drop larger ones.
const MAX_B64: usize = 74_000;

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 (RFC 4648) with padding; kept here to avoid a dependency for one use.
pub(crate) fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The OSC 52 sequence that sets the clipboard to `text`, and how many chars it carries
/// (fewer than the input when it had to be truncated). Wrapped for tmux when `$TMUX` is set.
pub(crate) fn osc52(text: &str) -> (String, usize, bool) {
    osc52_for(text, std::env::var_os("TMUX").is_some())
}

pub(crate) fn osc52_for(text: &str, tmux: bool) -> (String, usize, bool) {
    let max_bytes = MAX_B64 / 4 * 3;
    let mut end = text.len().min(max_bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let kept = &text[..end];
    let seq = format!("\x1b]52;c;{}\x07", base64(kept.as_bytes()));
    let seq = if tmux {
        format!("\x1bPtmux;{}\x1b\\", seq.replace('\x1b', "\x1b\x1b"))
    } else {
        seq
    };
    (seq, kept.chars().count(), end < text.len())
}

impl App {
    /// The text `y` copies from the view in front of the user, if there is any.
    pub(crate) fn focused_text(&self) -> Option<String> {
        if let Mode::Ticket { id } = &self.mode {
            return self.ticket_text(id);
        }
        match self.tab {
            Tab::Inbox => self.selected_inbox_text(),
            Tab::Board => self.board_ticket().and_then(|t| self.ticket_text(&t.id)),
            Tab::Chat => {
                let id = self.state.employees.get(self.selected)?.employee.id.clone();
                let line = self.state.chats.get(&id)?.last()?;
                Some(line.text.clone())
            }
            Tab::Events => self
                .events
                .iter()
                .rev()
                .nth(self.scroll as usize)
                .map(|e| format!("{} {} {}", e.seq, e.kind.name(), e.kind.summary())),
            _ => None,
        }
    }

    pub(crate) fn ticket_text(&self, id: &str) -> Option<String> {
        let t = self.state.ticket(id)?;
        let mut out = format!("{} {}\n\n{}", t.id, t.title, t.description);
        if !t.acceptance_criteria.is_empty() {
            out.push_str("\n\nAcceptance criteria:");
            for c in &t.acceptance_criteria {
                out.push_str("\n- ");
                out.push_str(c);
            }
        }
        Some(out)
    }

    fn selected_inbox_text(&self) -> Option<String> {
        Some(match self.state.inbox.get(self.inbox_selected)? {
            InboxItem::Permission { title, choices, .. } => {
                let mut out = title.clone();
                for (i, c) in choices.iter().enumerate() {
                    out.push_str(&format!("\n{}. {}", i + 1, c.name));
                }
                out
            }
            InboxItem::Question { question, .. } => question.clone(),
            InboxItem::EnvAlert {
                env,
                step,
                output_tail,
                ..
            } => format!("{env} {step:?}\n\n{output_tail}"),
            InboxItem::Alert { title, detail, .. } => format!("{title}\n\n{detail}"),
            InboxItem::SprintGate { id, sprint } => {
                let goal = self
                    .state
                    .sprints
                    .iter()
                    .find(|s| &s.id == sprint)
                    .map(|s| format!("{} {}\n\n{}", s.id, s.name, s.goal));
                goal.unwrap_or_else(|| format!("{id} sprint {sprint}"))
            }
            InboxItem::ReleaseGate { id, release } => format!("{id} release {release}"),
        })
    }

    /// Queue the focused text for the run loop to send as OSC 52.
    pub(crate) fn copy_focused(&mut self) {
        match self.focused_text() {
            Some(text) => self.copy_text(&text),
            None => self.status = Some("Nothing to copy".into()),
        }
    }

    pub(crate) fn copy_text(&mut self, text: &str) {
        let (seq, chars, truncated) = osc52(text);
        self.pending_clipboard = Some(seq);
        self.toast = Some(if truncated {
            format!("Copied {chars} chars (truncated)")
        } else {
            format!("Copied {chars} chars")
        });
    }

    /// The escape sequence for the run loop to write, once.
    pub fn take_clipboard(&mut self) -> Option<String> {
        self.pending_clipboard.take()
    }

    /// `Some(true)` to turn mouse capture on, `Some(false)` to turn it off.
    pub fn take_mouse_request(&mut self) -> Option<bool> {
        self.pending_mouse.take()
    }

    pub(crate) fn toggle_mouse(&mut self) {
        self.mouse_released = !self.mouse_released;
        self.pending_mouse = Some(!self.mouse_released);
    }
}
