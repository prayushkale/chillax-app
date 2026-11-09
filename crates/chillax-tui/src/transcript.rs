//! The Claude-Code-style session view: messages, then each tool call inline
//! with its output folded under it. One renderer serves the Live view and the Team grid
//! tiles; the Chat tab keeps its own plain log.

use chillax_core::{Speaker, ToolContent, ToolStatus, TranscriptItem};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use similar::{ChangeTag, TextDiff};

use crate::view::wrap;

/// Output lines shown under a call when folded in the Live view.
pub(crate) const FOLDED_LINES: usize = 6;
/// Output lines shown under a call when expanded (anything past is "… +N lines").
pub(crate) const EXPANDED_LINES: usize = 60;

/// How to draw a transcript.
#[derive(Clone, Copy)]
pub(crate) struct Look {
    /// Output lines kept under a call; the rest is summarised as "… +N lines".
    pub output_lines: usize,
    /// No blank line between messages (grid tiles).
    pub compact: bool,
}

/// An icon for the kind of tool call.
pub(crate) fn kind_icon(kind: &str) -> &'static str {
    match kind {
        "read" => "◇",
        "edit" => "✎",
        "delete" => "✂",
        "move" => "⇄",
        "search" => "⌕",
        "execute" => "$",
        "think" => "∴",
        "fetch" => "↓",
        _ => "⚙",
    }
}

fn status_mark(status: ToolStatus) -> (&'static str, Style) {
    match status {
        ToolStatus::Ok => ("✓", Style::new().green()),
        ToolStatus::Failed => ("✗", Style::new().red()),
        ToolStatus::Running => ("⋯", Style::new().yellow()),
        ToolStatus::Pending => ("…", Style::new().dark_gray()),
    }
}

/// The transcript as styled lines wrapped to `width`.
pub(crate) fn transcript_lines(
    items: &[TranscriptItem],
    width: usize,
    style: Look,
) -> Vec<Line<'static>> {
    let mut out: Vec<Line> = Vec::new();
    for item in items {
        match item {
            TranscriptItem::Message { speaker, text } => {
                if !style.compact && !out.is_empty() {
                    out.push(Line::default());
                }
                let (mark, mark_style, body_style) = match speaker {
                    Speaker::Stakeholder => (
                        ">".to_string(),
                        Style::new().green().bold(),
                        Style::new().green(),
                    ),
                    Speaker::Agent => ("●".to_string(), Style::new().cyan(), Style::new()),
                    Speaker::Teammate(who) => (
                        format!("{who} ›"),
                        Style::new().magenta().bold(),
                        Style::new(),
                    ),
                    Speaker::System => (
                        "•".to_string(),
                        Style::new().yellow(),
                        Style::new().yellow(),
                    ),
                    Speaker::Tool => (
                        "⚙".to_string(),
                        Style::new().dark_gray(),
                        Style::new().dark_gray(),
                    ),
                };
                let indent = mark.chars().count() + 1;
                for (i, part) in wrap(text, width.saturating_sub(indent).max(8))
                    .into_iter()
                    .enumerate()
                {
                    let head = if i == 0 {
                        Span::styled(format!("{mark} "), mark_style)
                    } else {
                        Span::raw(" ".repeat(indent))
                    };
                    out.push(Line::from(vec![head, Span::styled(part, body_style)]));
                }
            }
            TranscriptItem::Tool {
                kind,
                title,
                status,
                content,
                ..
            } => {
                let (mark, mark_style) = status_mark(*status);
                out.push(Line::from(vec![
                    Span::styled(format!("{mark} "), mark_style),
                    Span::styled(format!("{} ", kind_icon(kind)), Style::new().dark_gray()),
                    Span::styled(title.clone(), Style::new().bold()),
                ]));
                out.extend(output_lines(content, width, style.output_lines));
            }
        }
    }
    out
}

/// A call's output, indented under it with `⎿`; `limit` lines, then "… +N lines".
fn output_lines(content: &[ToolContent], width: usize, limit: usize) -> Vec<Line<'static>> {
    let mut all: Vec<Line<'static>> = Vec::new();
    for c in content {
        match c {
            ToolContent::Diff { path, old, new } => {
                all.push(Line::from(path.clone()).cyan());
                let diff = TextDiff::from_lines(old.as_deref().unwrap_or(""), new);
                for change in diff.iter_all_changes() {
                    let (sign, style) = match change.tag() {
                        ChangeTag::Insert => ("+", Style::new().green()),
                        ChangeTag::Delete => ("-", Style::new().red()),
                        ChangeTag::Equal => continue,
                    };
                    let text = change.value().trim_end_matches('\n');
                    all.push(Line::from(Span::styled(format!("{sign} {text}"), style)));
                }
            }
            ToolContent::Text { text } | ToolContent::Terminal { output: text } => {
                for l in text
                    .lines()
                    .flat_map(|l| wrap(l, width.saturating_sub(5).max(8)))
                {
                    all.push(Line::from(l).dark_gray());
                }
            }
        }
    }
    let hidden = all.len().saturating_sub(limit);
    all.truncate(limit);
    if hidden > 0 {
        all.push(Line::from(format!("… +{hidden} lines")).dark_gray());
    }
    all.into_iter()
        .enumerate()
        .map(|(i, mut line)| {
            let lead = if i == 0 { "  ⎿ " } else { "    " };
            line.spans
                .insert(0, Span::styled(lead, Style::new().dark_gray()));
            line
        })
        .collect()
}
