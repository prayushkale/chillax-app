//! `chillax transcript <employee>`: what one employee is saying and doing, as plain text.
//! Reads the same event stream as the TUI and the web UI (`/api/v1/events`, SSE), so it
//! works in any terminal; `--follow` keeps going like `tail -f`.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chillax_core::{Event, EventKind};

use crate::company::live_url;

/// With no `--follow`, stop once the server has been quiet this long: the backlog is
/// sent at once, so silence means it is exhausted.
const QUIET: Duration = Duration::from_millis(800);

/// Folds one employee's events into text. Message chunks run together; each tool call
/// and each turn end starts on its own line.
pub struct Folder {
    employee: String,
    mid_line: bool,
}

impl Folder {
    pub fn new(employee: &str) -> Self {
        Self {
            employee: employee.to_string(),
            mid_line: false,
        }
    }

    /// The text this event adds, or none when it is someone else's or not shown.
    pub fn push(&mut self, kind: &EventKind) -> Option<String> {
        match kind {
            EventKind::AgentMessageChunk { employee, text } if *employee == self.employee => {
                self.mid_line = !text.ends_with('\n');
                Some(text.clone())
            }
            EventKind::AgentToolCall {
                employee,
                title,
                tool_kind,
                ..
            } if *employee == self.employee => {
                let kind = if tool_kind.is_empty() {
                    String::new()
                } else {
                    format!(" [{tool_kind}]")
                };
                Some(format!("{}> {title}{kind}\n", self.break_line()))
            }
            EventKind::AgentTurnEnded {
                employee,
                stop_reason,
                ..
            } if *employee == self.employee => Some(format!(
                "{}-- turn ended: {stop_reason} --\n",
                self.break_line()
            )),
            _ => None,
        }
    }

    fn break_line(&mut self) -> &'static str {
        if std::mem::replace(&mut self.mid_line, false) {
            "\n"
        } else {
            ""
        }
    }
}

/// The JSON of an SSE `data:` line, if it is one.
pub fn sse_data(line: &str) -> Option<&str> {
    let data = line.strip_prefix("data:")?;
    Some(data.strip_prefix(' ').unwrap_or(data))
}

pub async fn run(cwd: &Path, employee: &str, follow: bool, since: u64) -> Result<()> {
    let project = crate::project::ensure_project(cwd)?;
    let state = crate::settings::company_state(&project).await?;
    let id = crate::settings::find_employee(&state, employee)?
        .employee
        .id
        .clone();
    let mut folder = Folder::new(&id);
    let Some(url) = live_url(&project).await else {
        if follow {
            bail!("Chillax is not running for this project; start it with `chillax` to follow");
        }
        let hub = crate::company::Company::open(&project).await?;
        for event in chillax_core::CompanyHandle::events_since(&*hub, since).await? {
            print_event(&mut folder, &event);
        }
        return Ok(());
    };
    let mut resp = reqwest::Client::new()
        .get(format!("{url}/api/v1/events?since={since}"))
        .send()
        .await
        .context("connecting to Chillax")?
        .error_for_status()?;
    let mut buf = String::new();
    loop {
        let next = if follow {
            resp.chunk().await?
        } else {
            match tokio::time::timeout(QUIET, resp.chunk()).await {
                Ok(chunk) => chunk?,
                Err(_) => break,
            }
        };
        let Some(bytes) = next else { break };
        buf.push_str(&String::from_utf8_lossy(&bytes));
        while let Some(end) = buf.find('\n') {
            let line: String = buf.drain(..=end).collect();
            if let Some(data) = sse_data(line.trim_end())
                && let Ok(event) = serde_json::from_str::<Event>(data)
            {
                print_event(&mut folder, &event);
            }
        }
    }
    Ok(())
}

fn print_event(folder: &mut Folder, event: &Event) {
    use std::io::Write;
    if let Some(text) = folder.push(&event.kind) {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(text.as_bytes());
        let _ = out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(who: &str, text: &str) -> EventKind {
        EventKind::AgentMessageChunk {
            employee: who.into(),
            text: text.into(),
        }
    }

    #[test]
    fn chunks_run_together_and_tool_calls_start_a_line() {
        let mut f = Folder::new("pat");
        let events = [
            chunk("pat", "Looking "),
            chunk("pat", "at it."),
            chunk("dana", "not mine"),
            EventKind::AgentToolCall {
                employee: "pat".into(),
                tool_call_id: "1".into(),
                title: "cargo test".into(),
                tool_kind: "execute".into(),
                status: "pending".into(),
            },
            EventKind::AgentTurnEnded {
                employee: "pat".into(),
                stop_reason: "end_turn".into(),
                ticket: None,
            },
        ];
        let out: String = events.iter().filter_map(|e| f.push(e)).collect();
        assert_eq!(
            out,
            "Looking at it.\n> cargo test [execute]\n-- turn ended: end_turn --\n"
        );
    }

    #[test]
    fn sse_lines_yield_their_data_only() {
        assert_eq!(sse_data("data: {\"a\":1}"), Some("{\"a\":1}"));
        assert_eq!(sse_data("data:{\"a\":1}"), Some("{\"a\":1}"));
        assert_eq!(sse_data("event: chillax"), None);
        assert_eq!(sse_data(": keep-alive"), None);
    }

    #[test]
    fn an_event_parses_from_the_sse_json() {
        let sent = Event {
            seq: 3,
            ts: chrono::Utc::now(),
            actor: chillax_core::Actor::System,
            kind: chunk("pat", "hi"),
        };
        let json = serde_json::to_string(&sent).expect("json");
        let e: Event = serde_json::from_str(&json).expect("event");
        assert_eq!(Folder::new("pat").push(&e.kind).as_deref(), Some("hi"));
    }
}
