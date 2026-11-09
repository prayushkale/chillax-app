//! The per-employee transcript: one ordered, Claude-Code-style stream built
//! from the chat log and the live tool calls, plus the one-line "working on" summary.
//! Pure functions over [`State`]; frontends only draw the result.

use crate::engine::State;
use crate::model::{EmployeeStatus, LiveTool, Speaker, Ticket, ToolContent};

/// Where a tool call stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Pending,
    Running,
    Ok,
    Failed,
}

impl ToolStatus {
    pub fn parse(status: &str) -> Self {
        match status {
            "completed" => Self::Ok,
            "failed" => Self::Failed,
            "pending" => Self::Pending,
            _ => Self::Running,
        }
    }
}

/// One entry of a transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptItem {
    /// A message: the stakeholder, a teammate, the assistant (`Speaker::Agent`) or the system.
    Message { speaker: Speaker, text: String },
    /// A tool call with its output (empty when it has none or it was trimmed).
    Tool {
        id: String,
        kind: String,
        title: String,
        status: ToolStatus,
        content: Vec<ToolContent>,
    },
}

/// `Title [status]` is how the chat log stores a tool line.
fn split_tool_text(text: &str) -> (&str, &str) {
    match text.strip_suffix(']').and_then(|t| t.rsplit_once(" [")) {
        Some((title, status)) => (title, status),
        None => (text, ""),
    }
}

impl State {
    /// The ticket an employee is building or reviewing right now.
    pub fn current_ticket(&self, employee: &str) -> Option<&Ticket> {
        self.tickets.iter().find(|t| {
            (t.assignee.as_deref() == Some(employee) && t.state == "in_progress")
                || (t.reviewer.as_deref() == Some(employee) && t.state == "in_review")
        })
    }

    /// The employee's session as one stream: chat lines in order, each tool line joined
    /// with its live record (kind, output, current status) through `tool_call_id`. Live
    /// calls the chat log has no line for are appended.
    pub fn transcript(&self, employee: &str) -> Vec<TranscriptItem> {
        let live: &[LiveTool] = self.live.get(employee).map_or(&[], |l| l.tools.as_slice());
        let mut used = vec![false; live.len()];
        let mut out = Vec::new();
        for line in self.chats.get(employee).map_or(&[][..], Vec::as_slice) {
            match (&line.speaker, &line.tool_call_id) {
                (Speaker::Tool, Some(id)) => {
                    let found = live.iter().rposition(|t| t.tool_call_id == *id);
                    if let Some(i) = found {
                        used[i] = true;
                    }
                    let (title, status) = split_tool_text(&line.text);
                    out.push(match found.map(|i| &live[i]) {
                        Some(t) => tool_item(t),
                        None => TranscriptItem::Tool {
                            id: id.clone(),
                            kind: String::new(),
                            title: title.to_string(),
                            status: ToolStatus::parse(status),
                            content: Vec::new(),
                        },
                    });
                }
                (Speaker::Tool, None) => {}
                (speaker, _) => out.push(TranscriptItem::Message {
                    speaker: speaker.clone(),
                    text: line.text.clone(),
                }),
            }
        }
        out.extend(
            live.iter()
                .zip(used)
                .filter(|(_, used)| !used)
                .map(|(t, _)| tool_item(t)),
        );
        out
    }

    /// A short line on what an employee is doing: the ticket (id and title), then the
    /// current plan step, else the last tool call while working, else the last progress
    /// report; the status when there is nothing else.
    pub fn working_on(&self, employee: &str) -> String {
        let Some(view) = self.employee(employee) else {
            return String::new();
        };
        let ticket = self
            .current_ticket(employee)
            .map(|t| format!("{} {}", t.id, t.title));
        let live = self.live.get(employee);
        let working = matches!(view.status, EmployeeStatus::Working) || view.turn_open;
        let step = live
            .and_then(|l| l.plan.iter().find(|p| p.status == "in_progress"))
            .map(|p| p.content.clone());
        let detail = if working {
            step.or_else(|| live.and_then(|l| l.tools.last()).map(|t| t.title.clone()))
        } else {
            None
        }
        .or_else(|| view.last_progress.clone());
        match (ticket, detail) {
            (Some(t), Some(d)) => format!("{t} — {d}"),
            (Some(t), None) => t,
            (None, Some(d)) => d,
            (None, None) => view.status.label().to_string(),
        }
    }
}

fn tool_item(t: &LiveTool) -> TranscriptItem {
    TranscriptItem::Tool {
        id: t.tool_call_id.clone(),
        kind: t.tool_kind.clone(),
        title: t.title.clone(),
        status: ToolStatus::parse(&t.status),
        content: t.content.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ChatLine, LiveView, PlanEntry};

    fn state() -> State {
        let mut s = State::default();
        s.employees.push(crate::engine::EmployeeView {
            employee: crate::model::Employee {
                id: "pat".into(),
                name: "Pat".into(),
                role: "dev".into(),
                role_title: "Dev".into(),
                backend: "claude".into(),
                model: None,
                effort: None,
            },
            status: EmployeeStatus::Idle,
            last_progress: None,
            turn_open: false,
            stopped: false,
            reply_pending: false,
            pending_agent: None,
        });
        s
    }

    fn line(speaker: Speaker, text: &str, id: Option<&str>) -> ChatLine {
        ChatLine {
            speaker,
            text: text.into(),
            tool_call_id: id.map(String::from),
        }
    }

    fn tool(id: &str, title: &str, status: &str) -> LiveTool {
        LiveTool {
            tool_call_id: id.into(),
            title: title.into(),
            tool_kind: "edit".into(),
            status: status.into(),
            content: vec![ToolContent::Text { text: "out".into() }],
        }
    }

    #[test]
    fn joins_tool_lines_with_live_records_in_chat_order() {
        let mut s = state();
        s.chats.insert(
            "pat".into(),
            vec![
                line(Speaker::Stakeholder, "do it", None),
                line(Speaker::Agent, "ok", None),
                line(Speaker::Tool, "Edit a.rs [pending]", Some("c1")),
                line(Speaker::Tool, "Read old [completed]", Some("gone")),
            ],
        );
        s.live.insert(
            "pat".into(),
            LiveView {
                plan: vec![],
                tools: vec![
                    tool("c1", "Edit a.rs", "completed"),
                    tool("c9", "Orphan", "failed"),
                ],
            },
        );
        let t = s.transcript("pat");
        assert_eq!(t.len(), 5);
        assert!(matches!(&t[0], TranscriptItem::Message { text, .. } if text == "do it"));
        match &t[2] {
            TranscriptItem::Tool {
                status,
                kind,
                content,
                ..
            } => {
                assert_eq!(
                    *status,
                    ToolStatus::Ok,
                    "live status wins over the chat text"
                );
                assert_eq!(kind, "edit");
                assert_eq!(content.len(), 1);
            }
            other => panic!("{other:?}"),
        }
        match &t[3] {
            TranscriptItem::Tool {
                title,
                status,
                content,
                ..
            } => {
                assert_eq!(title, "Read old");
                assert_eq!(*status, ToolStatus::Ok);
                assert!(content.is_empty(), "evicted calls keep their line only");
            }
            other => panic!("{other:?}"),
        }
        assert!(
            matches!(&t[4], TranscriptItem::Tool { title, status: ToolStatus::Failed, .. } if title == "Orphan")
        );
    }

    #[test]
    fn working_on_prefers_plan_then_tool_then_progress_then_status() {
        let mut s = state();
        assert_eq!(s.working_on("pat"), "idle");
        s.employees[0].last_progress = Some("halfway".into());
        assert_eq!(s.working_on("pat"), "halfway");
        s.employees[0].status = EmployeeStatus::Working;
        s.live.insert(
            "pat".into(),
            LiveView {
                plan: vec![],
                tools: vec![tool("c1", "Run tests", "pending")],
            },
        );
        assert_eq!(s.working_on("pat"), "Run tests");
        s.live.get_mut("pat").unwrap().plan = vec![
            PlanEntry {
                content: "done step".into(),
                status: "completed".into(),
            },
            PlanEntry {
                content: "write code".into(),
                status: "in_progress".into(),
            },
        ];
        assert_eq!(s.working_on("pat"), "write code");
        assert_eq!(s.working_on("nobody"), "");
    }
}
