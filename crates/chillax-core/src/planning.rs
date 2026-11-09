//! Planning the next sprint: which tickets are up for a sprint,
//! and what the project manager is told when the stakeholder asks for one. Pure.

use std::fmt::Write as _;

use crate::engine::{Engine, State};
use crate::model::*;

/// Tickets that could join the next sprint: not dropped, not finished, not already
/// merged into dev, and not in a sprint that is waiting or running. That includes the
/// backlog and the unfinished tickets of a sprint that was closed.
pub fn candidates(state: &State) -> Vec<&Ticket> {
    state
        .tickets
        .iter()
        .filter(|t| t.dropped.is_none() && !state.is_finished(t))
        .filter(|t| !t.checks.contains("merged_to_dev"))
        .filter(|t| {
            t.sprint
                .as_deref()
                .and_then(|sp| state.sprint(sp))
                .is_none_or(|sp| {
                    !matches!(sp.status, SprintStatus::Proposed | SprintStatus::Active)
                })
        })
        .collect()
}

fn line(t: &Ticket) -> String {
    let mut line = format!("- {} ({}", t.id, t.kind.label());
    if let Some(e) = t.estimate {
        let _ = write!(line, ", {e} pt");
    }
    if let Some(epic) = &t.epic {
        let _ = write!(line, ", epic {epic}");
    }
    if !t.blocked_by.is_empty() {
        let _ = write!(line, ", blocked by {}", t.blocked_by.join(" "));
    }
    let _ = write!(line, "): {}", t.title.lines().next().unwrap_or(""));
    line
}

/// The last release that was tagged, if any.
fn last_release(state: &State) -> Option<&Release> {
    state.releases.iter().rev().find(|r| {
        matches!(
            r.status,
            ReleaseStatus::Tagged | ReleaseStatus::Deployed | ReleaseStatus::DeployFailed
        )
    })
}

/// First meaningful line of the release notes, without Markdown heading marks.
fn headline(notes: &str) -> Option<&str> {
    notes
        .lines()
        .map(|l| l.trim().trim_start_matches('#').trim())
        .find(|l| !l.is_empty())
}

/// The briefing for a fresh start: nothing waits, so the PM talks to the stakeholder first.
fn fresh_start(state: &State, note: Option<&str>) -> String {
    let mut out = String::from("[Chillax] The stakeholder wants to start a new sprint.\n");
    if let Some(r) = last_release(state) {
        let sprint = r
            .sprint
            .as_deref()
            .map(|s| format!(", sprint {s}"))
            .unwrap_or_default();
        let _ = write!(out, "\nThe last release ({}{sprint}) shipped.", r.version);
        if let Some(h) = headline(&r.notes) {
            let _ = write!(out, " Release notes headline: {h}");
        }
        out.push('\n');
    }
    if let Some(note) = note {
        let _ = write!(out, "\nTheir note: {note}\n");
    }
    out.push_str(
        "\nStart the next one with the stakeholder: ask them in this chat what the next release \
         should deliver, then write epics and stories with acceptance criteria and propose the \
         sprint with `create_sprint`; it waits for their approval.\n\nTeam:\n",
    );
    for e in &state.employees {
        let _ = writeln!(out, "- {} ({})", e.employee.name, e.employee.role_title);
    }
    out
}

/// What the project manager is told: the backlog, the leftovers, the team, and what to do.
pub fn briefing(engine: &Engine, note: Option<&str>) -> String {
    let state = &engine.state;
    let initial = engine.workflow.initial_state();
    let (backlog, leftovers): (Vec<&Ticket>, Vec<&Ticket>) = candidates(state)
        .into_iter()
        .partition(|t| t.state == initial && t.branch.is_none());
    if backlog.is_empty() && leftovers.is_empty() {
        return fresh_start(state, note);
    }
    let mut out = String::from(
        "[Chillax] The stakeholder asked you to plan the next sprint. Propose it with \
         `create_sprint`; it waits for their approval.\n",
    );
    if let Some(note) = note {
        let _ = write!(out, "\nTheir note: {note}\n");
    }
    out.push_str("\nBacklog:\n");
    if backlog.is_empty() {
        out.push_str("(empty)\n");
    }
    for t in &backlog {
        out.push_str(&line(t));
        out.push('\n');
    }
    out.push_str(
        "\nLeftovers from earlier sprints (unfinished work, already started or planned):\n",
    );
    if leftovers.is_empty() {
        out.push_str("(none)\n");
    }
    for t in &leftovers {
        let _ = writeln!(out, "{} [{}]", line(t), t.state);
    }
    out.push_str("\nTeam:\n");
    for e in &state.employees {
        let _ = writeln!(out, "- {} ({})", e.employee.name, e.employee.role_title);
    }
    out.push_str(
        "\nPick a sprint that fits the team: put the leftovers first, then the backlog items \
         that serve the note or the most important epics. If the note asks for work that has \
         no ticket yet, file it with `create_story` first. Keep dependencies together (use \
         `blocked_by`). Dropped tickets are not listed; do not revive them. Reply with the \
         sprint you proposed and why.",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::Command;
    use crate::engine::Engine;
    use crate::event::{Actor, Event};
    use crate::role::RoleCatalog;
    use crate::workflow::Workflow;

    fn engine() -> Engine {
        Engine::new("Acme", RoleCatalog::default(), Workflow::default())
    }

    fn run(e: &mut Engine, actor: Actor, cmd: Command) -> Vec<crate::event::EventKind> {
        let kinds = e.decide(&actor, cmd).expect("accepted");
        for kind in kinds.clone() {
            let event = Event {
                seq: e.state.last_seq + 1,
                ts: chrono::DateTime::from_timestamp(0, 0).expect("epoch"),
                actor: actor.clone(),
                kind,
            };
            e.apply(&event);
        }
        kinds
    }

    fn story(e: &mut Engine, title: &str) {
        run(
            e,
            Actor::Stakeholder,
            Command::CreateTicket {
                kind: TicketKind::Story,
                title: title.into(),
                description: String::new(),
                acceptance_criteria: vec![],
                estimate: Some(2),
                epic: None,
                external: None,
            },
        );
    }

    #[test]
    fn candidates_skip_dropped_finished_and_planned_tickets() {
        let mut e = engine();
        for t in ["a", "b", "c"] {
            story(&mut e, t);
        }
        run(
            &mut e,
            Actor::Stakeholder,
            Command::DropTicket {
                id: "T-2".into(),
                reason: "no".into(),
            },
        );
        run(
            &mut e,
            Actor::Stakeholder,
            Command::ProposeSprint {
                name: "S".into(),
                goal: String::new(),
                tickets: vec!["T-3".into()],
            },
        );
        let ids: Vec<&str> = candidates(&e.state).iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["T-1"]);
    }

    #[test]
    fn briefing_asks_stakeholder_when_backlog_empty() {
        let mut e = engine();
        e.state.releases.push(Release {
            id: "R-1".into(),
            version: "v0.1.0".into(),
            sprint: Some("SP-1".into()),
            round: 1,
            tickets: vec![],
            known_issues: vec![],
            notes: "\n## Login and checkout\n- stuff".into(),
            status: ReleaseStatus::Tagged,
            tag: Some("v0.1.0".into()),
            commit: None,
        });
        let text = briefing(&e, None);
        assert!(text.contains("ask them in this chat"), "{text}");
        assert!(text.contains("v0.1.0"), "{text}");
        assert!(text.contains("sprint SP-1"), "{text}");
        assert!(text.contains("Login and checkout"), "{text}");
        assert!(text.contains("create_sprint"));
    }

    #[test]
    fn briefing_lists_backlog_leftovers_and_the_note() {
        let mut e = engine();
        story(&mut e, "Backlog item");
        story(&mut e, "Half done");
        e.state.tickets[1].state = "in_progress".into();
        e.state.tickets[1].branch = Some("feat/t-2".into());
        let text = briefing(&e, Some("focus on checkout"));
        assert!(text.contains("focus on checkout"));
        assert!(text.contains("- T-1 (story, 2 pt): Backlog item"));
        assert!(text.contains("Leftovers"));
        assert!(text.contains("T-2 (story, 2 pt): Half done [in_progress]"));
        assert!(text.contains("create_sprint"));
    }
}
