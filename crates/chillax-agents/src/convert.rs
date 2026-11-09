//! ACP session updates → Chillax events.

use std::path::{Component, Path, PathBuf};

use agent_client_protocol::schema::v1::{
    ContentBlock, PermissionOptionKind, PlanEntryStatus, SessionUpdate, ToolCallContent,
    ToolCallLocation, ToolCallStatus, ToolKind, UsageUpdate,
};
use chillax_core::{EventKind, PermissionChoice, PlanEntry, ToolContent};

/// The most text one event carries from a tool call, so the event log stays small.
const CONTENT_CAP: usize = 16 * 1024;

pub fn update_to_events(employee: &str, update: SessionUpdate) -> Vec<EventKind> {
    let employee = employee.to_string();
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => match chunk.content {
            ContentBlock::Text(text) if !text.text.is_empty() => {
                vec![EventKind::AgentMessageChunk {
                    employee,
                    text: text.text,
                }]
            }
            _ => Vec::new(),
        },
        SessionUpdate::ToolCall(call) => {
            let tool_call_id = call.tool_call_id.0.to_string();
            let mut events = vec![EventKind::AgentToolCall {
                employee: employee.clone(),
                tool_call_id: tool_call_id.clone(),
                title: call.title,
                tool_kind: tool_kind(call.kind).to_string(),
                status: tool_status(call.status).to_string(),
            }];
            events.extend(content_event(&employee, &tool_call_id, call.content));
            events
        }
        SessionUpdate::ToolCallUpdate(update) => {
            let tool_call_id = update.tool_call_id.0.to_string();
            let fields = update.fields;
            let mut events = Vec::new();
            if fields.status.is_some() || fields.title.is_some() {
                events.push(EventKind::AgentToolCallUpdated {
                    employee: employee.clone(),
                    tool_call_id: tool_call_id.clone(),
                    title: fields.title,
                    status: fields.status.map(|s| tool_status(s).to_string()),
                });
            }
            if let Some(content) = fields.content {
                events.extend(content_event(&employee, &tool_call_id, content));
            }
            events
        }
        SessionUpdate::Plan(plan) => vec![EventKind::AgentPlanUpdated {
            employee,
            entries: plan
                .entries
                .into_iter()
                .map(|e| PlanEntry {
                    content: e.content,
                    status: plan_status(e.status).to_string(),
                })
                .collect(),
        }],
        _ => Vec::new(),
    }
}

/// What a `usage_update` says: context fill and the session's CUMULATIVE cost so far.
/// `used`/`size` describe the context window, never billed tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageReading {
    pub context_used: u64,
    pub context_size: u64,
    /// The cumulative cost in millionths of `currency`, when the agent sent one.
    pub cost_micros: Option<u64>,
    pub currency: String,
}

pub fn usage_from_update(update: &UsageUpdate) -> UsageReading {
    let valid = update
        .cost
        .as_ref()
        .filter(|c| c.amount.is_finite() && c.amount >= 0.0);
    UsageReading {
        context_used: update.used,
        context_size: update.size,
        cost_micros: valid.map(|c| (c.amount * 1e6).round() as u64),
        currency: valid
            .map(|c| c.currency.trim().to_uppercase())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| "USD".to_string()),
    }
}

fn content_event(
    employee: &str,
    tool_call_id: &str,
    content: Vec<ToolCallContent>,
) -> Option<EventKind> {
    let content = tool_content(content);
    (!content.is_empty()).then(|| EventKind::AgentToolCallContent {
        employee: employee.to_string(),
        tool_call_id: tool_call_id.to_string(),
        content,
    })
}

/// Text and diffs a tool call produced, within a shared size budget. Terminal embeds
/// carry no output (Chillax offers no terminal capability) and images are not text.
pub fn tool_content(content: Vec<ToolCallContent>) -> Vec<ToolContent> {
    let mut budget = CONTENT_CAP;
    let mut out = Vec::new();
    for item in content {
        let converted = match item {
            ToolCallContent::Content(c) => match c.content {
                ContentBlock::Text(t) if !t.text.is_empty() => Some(ToolContent::Text {
                    text: take(t.text, &mut budget),
                }),
                _ => None,
            },
            ToolCallContent::Diff(d) => Some(ToolContent::Diff {
                path: d.path.to_string_lossy().to_string(),
                old: d.old_text.map(|t| take(t, &mut budget)),
                new: take(d.new_text, &mut budget),
            }),
            _ => None,
        };
        out.extend(converted);
        if budget == 0 {
            break;
        }
    }
    out
}

/// Cut `text` to what is left of `budget`, on a character boundary.
fn take(mut text: String, budget: &mut usize) -> String {
    if text.len() > *budget {
        let mut cut = *budget;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n… (truncated)");
        *budget = 0;
    } else {
        *budget -= text.len();
    }
    text
}

/// True when every location a tool call touches is inside `dir`. Calls that name no
/// location (shell commands) cannot be checked and pass; the role policy decides them.
pub fn within(dir: &Path, locations: &[ToolCallLocation]) -> bool {
    locations
        .iter()
        .all(|l| normalize(&l.path).starts_with(normalize(dir)))
}

/// Resolve `.` and `..` without touching the filesystem.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

pub fn plan_status(status: PlanEntryStatus) -> &'static str {
    match status {
        PlanEntryStatus::Pending => "pending",
        PlanEntryStatus::InProgress => "in_progress",
        PlanEntryStatus::Completed => "completed",
        _ => "pending",
    }
}

pub fn tool_kind(kind: ToolKind) -> &'static str {
    match kind {
        ToolKind::Read => "read",
        ToolKind::Edit => "edit",
        ToolKind::Delete => "delete",
        ToolKind::Move => "move",
        ToolKind::Search => "search",
        ToolKind::Execute => "execute",
        ToolKind::Think => "think",
        ToolKind::Fetch => "fetch",
        ToolKind::SwitchMode => "switch_mode",
        _ => "other",
    }
}

pub fn tool_status(status: ToolCallStatus) -> &'static str {
    match status {
        ToolCallStatus::Pending => "pending",
        ToolCallStatus::InProgress => "in_progress",
        ToolCallStatus::Completed => "completed",
        ToolCallStatus::Failed => "failed",
        _ => "unknown",
    }
}

pub fn option_kind(kind: PermissionOptionKind) -> &'static str {
    match kind {
        PermissionOptionKind::AllowOnce => "allow_once",
        PermissionOptionKind::AllowAlways => "allow_always",
        PermissionOptionKind::RejectOnce => "reject_once",
        PermissionOptionKind::RejectAlways => "reject_always",
        _ => "other",
    }
}

/// The option the role policy picks when it auto-approves: allow once if offered,
/// otherwise any allow option.
pub fn auto_choice(choices: &[PermissionChoice]) -> Option<&PermissionChoice> {
    choices
        .iter()
        .find(|c| c.kind == "allow_once")
        .or_else(|| choices.iter().find(|c| c.kind.starts_with("allow")))
}

/// The option `permissions = "auto"` picks: the broadest allow ("always"), so the agent
/// stops asking, then any other allow. `None` when only rejections are offered.
pub fn yolo_choice(choices: &[PermissionChoice]) -> Option<&PermissionChoice> {
    ["allow_always", "allow_once"]
        .iter()
        .find_map(|kind| choices.iter().find(|c| c.kind == *kind))
        .or_else(|| choices.iter().find(|c| c.kind.starts_with("allow")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{ContentChunk, ToolCall};

    #[test]
    fn text_chunks_and_tool_calls_become_events() {
        let chunk = SessionUpdate::AgentMessageChunk(ContentChunk::new("hi".into()));
        assert_eq!(
            update_to_events("pat", chunk),
            [EventKind::AgentMessageChunk {
                employee: "pat".into(),
                text: "hi".into()
            }]
        );
        let call = SessionUpdate::ToolCall(ToolCall::new("c1", "Read file").kind(ToolKind::Read));
        assert!(matches!(
            update_to_events("pat", call).as_slice(),
            [EventKind::AgentToolCall { tool_kind, status, .. }]
                if tool_kind == "read" && status == "pending"
        ));
    }

    #[test]
    fn usage_updates_become_readings_not_events() {
        use agent_client_protocol::schema::v1::Cost;
        let with = UsageUpdate::new(10, 100).cost(Cost::new(0.25, "usd"));
        assert_eq!(
            usage_from_update(&with),
            UsageReading {
                context_used: 10,
                context_size: 100,
                cost_micros: Some(250_000),
                currency: "USD".into()
            }
        );
        let without = usage_from_update(&UsageUpdate::new(1, 2));
        assert_eq!(without.cost_micros, None);
        let bad = usage_from_update(&UsageUpdate::new(1, 2).cost(Cost::new(-1.0, "USD")));
        assert_eq!(bad.cost_micros, None);
        assert!(update_to_events("pat", SessionUpdate::UsageUpdate(with)).is_empty());
    }

    #[test]
    fn plans_become_events() {
        let plan = SessionUpdate::Plan(agent_client_protocol::schema::v1::Plan::new(vec![
            agent_client_protocol::schema::v1::PlanEntry::new(
                "write it",
                agent_client_protocol::schema::v1::PlanEntryPriority::High,
                PlanEntryStatus::InProgress,
            ),
        ]));
        assert_eq!(
            update_to_events("pat", plan),
            [EventKind::AgentPlanUpdated {
                employee: "pat".into(),
                entries: vec![PlanEntry {
                    content: "write it".into(),
                    status: "in_progress".into()
                }]
            }]
        );
    }

    #[test]
    fn tool_output_and_diffs_become_content_events() {
        use agent_client_protocol::schema::v1::{Diff, ToolCall};
        let call = SessionUpdate::ToolCall(
            ToolCall::new("c1", "Edit a.txt")
                .kind(ToolKind::Edit)
                .content(vec![
                    Diff::new("/w/a.txt", "new\n").old_text("old\n").into(),
                    "done".into(),
                ]),
        );
        let events = update_to_events("pat", call);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[1],
            EventKind::AgentToolCallContent {
                employee: "pat".into(),
                tool_call_id: "c1".into(),
                content: vec![
                    ToolContent::Diff {
                        path: "/w/a.txt".into(),
                        old: Some("old\n".into()),
                        new: "new\n".into()
                    },
                    ToolContent::Text {
                        text: "done".into()
                    }
                ]
            }
        );
    }

    #[test]
    fn huge_tool_output_is_cut() {
        let big = "x".repeat(CONTENT_CAP * 2);
        let content = tool_content(vec![big.into(), "never kept".into()]);
        assert_eq!(content.len(), 1);
        let ToolContent::Text { text } = &content[0] else {
            panic!("expected text");
        };
        assert!(text.len() < CONTENT_CAP + 32 && text.ends_with("(truncated)"));
    }

    #[test]
    fn locations_must_stay_inside_the_worktree() {
        let at = |p: &str| ToolCallLocation::new(p);
        let dir = Path::new("/repo/.chillax/worktrees/T-1");
        assert!(within(dir, &[at("/repo/.chillax/worktrees/T-1/src/a.rs")]));
        assert!(within(dir, &[]));
        assert!(!within(dir, &[at("/repo/src/a.rs")]));
        assert!(!within(
            dir,
            &[at("/repo/.chillax/worktrees/T-1/../T-2/a.rs")]
        ));
        assert!(!within(
            dir,
            &[at("/repo/.chillax/worktrees/T-1-evil/a.rs")]
        ));
        assert!(!within(
            dir,
            &[at("/repo/.chillax/worktrees/T-1/a.rs"), at("/etc/passwd")]
        ));
    }

    #[test]
    fn auto_choice_prefers_allow_once() {
        let choice = |id: &str, kind: &str| PermissionChoice {
            id: id.into(),
            name: id.into(),
            kind: kind.into(),
        };
        let choices = [
            choice("always", "allow_always"),
            choice("once", "allow_once"),
            choice("no", "reject_once"),
        ];
        assert_eq!(auto_choice(&choices).map(|c| c.id.as_str()), Some("once"));
        assert_eq!(auto_choice(&choices[2..]), None);
    }

    #[test]
    fn yolo_prefers_allow_always_and_never_picks_a_rejection() {
        let choice = |id: &str, kind: &str| PermissionChoice {
            id: id.into(),
            name: id.into(),
            kind: kind.into(),
        };
        let choices = [
            choice("once", "allow_once"),
            choice("always", "allow_always"),
            choice("no", "reject_once"),
        ];
        assert_eq!(yolo_choice(&choices).map(|c| c.id.as_str()), Some("always"));
        assert_eq!(
            yolo_choice(&[choices[0].clone(), choices[2].clone()]).map(|c| c.id.as_str()),
            Some("once")
        );
        assert_eq!(yolo_choice(&choices[2..]), None);
    }
}
