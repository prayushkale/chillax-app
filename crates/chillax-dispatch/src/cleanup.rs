//! Retention of the scratch space Chillax and its agents leave under `.chillax/`:
//! QA scratch folders (`state/qa/<ticket>`, often gigabytes of build output)
//! and worktrees of tickets that are over. All pure planning plus blocking filesystem
//! work; callers run it through `spawn_blocking`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use chillax_core::Ticket;
use chillax_core::engine::State;

/// Whether a ticket is over for good: in the workflow's final state, or dropped.
pub(crate) fn is_over(ticket: &Ticket, terminal: &str) -> bool {
    ticket.dropped.is_some() || ticket.state == terminal
}

/// Whether the ticket's QA scratch folder may go: the ticket is over, no round still
/// tests it and no agent session is still working on it.
pub(crate) fn scratch_removable(
    state: &State,
    ticket: &Ticket,
    terminal: &str,
    sessions: &BTreeSet<(String, String)>,
) -> bool {
    is_over(ticket, terminal)
        && !state
            .open_round()
            .is_some_and(|r| r.tickets.contains(&ticket.id))
        && !sessions.iter().any(|(_, t)| *t == ticket.id)
}

/// The `state/qa/*` folders that belong to no live ticket: over, or unknown. A folder
/// whose ticket is still in progress (or in an open round, or has a session) stays.
pub(crate) fn orphan_qa_dirs(
    qa_dir: &Path,
    state: &State,
    terminal: &str,
    sessions: &BTreeSet<(String, String)>,
) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(qa_dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            match state.ticket(&name) {
                Some(ticket) => scratch_removable(state, ticket, terminal, sessions),
                None => !sessions.iter().any(|(_, t)| *t == name),
            }
        })
        .map(|e| e.path())
        .collect()
}

/// What to remove from `.chillax/worktrees` at startup: the worktree of every over
/// ticket and any `_merge-*` scratch left by a crash. `_dev`, `_qa`, `_prod` and every
/// other `_` name are shared and never listed.
pub(crate) fn stale_worktrees(
    worktrees_dir: &Path,
    state: &State,
    terminal: &str,
    sessions: &BTreeSet<(String, String)>,
) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(worktrees_dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("_merge-") {
                return true;
            }
            !name.starts_with('_')
                && state
                    .ticket(&name)
                    .is_some_and(|t| scratch_removable(state, t, terminal, sessions))
        })
        .map(|e| e.path())
        .collect()
}

/// Size of a directory tree in bytes (symlinks are not followed).
fn dir_size(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(_) => e.metadata().map_or(0, |m| m.len()),
            Err(_) => 0,
        })
        .sum()
}

/// Delete a directory tree and return the bytes it held.
pub(crate) fn remove_measured(path: &Path) -> std::io::Result<u64> {
    let bytes = dir_size(path);
    std::fs::remove_dir_all(path)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket(id: &str, state: &str, dropped: bool) -> serde_json::Value {
        serde_json::json!({"id": id, "kind": "story", "title": id, "state": state,
            "dropped": dropped.then_some("no")})
    }

    /// A state with one ticket per (id, state, dropped) and an optional open round.
    fn state(tickets: &[(&str, &str, bool)], round: Option<&[&str]>) -> State {
        let tickets: Vec<_> = tickets.iter().map(|(i, s, d)| ticket(i, s, *d)).collect();
        let qa = round.map(|ids| {
            serde_json::json!({"number": 1, "commit": "c", "status": "testing", "tickets": ids})
        });
        State {
            tickets: serde_json::from_value(serde_json::json!(tickets)).expect("tickets parse"),
            qa: qa.map(|q| serde_json::from_value(q).expect("round parses")),
            ..State::default()
        }
    }

    fn dirs(root: &Path, names: &[&str]) {
        for n in names {
            std::fs::create_dir_all(root.join(n)).expect("mkdir");
            std::fs::write(root.join(n).join("f"), b"x").expect("write");
        }
    }

    fn names(paths: Vec<PathBuf>) -> Vec<String> {
        let mut v: Vec<String> = paths
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn measures_and_removes_a_tree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tree = dir.path().join("t1/target");
        std::fs::create_dir_all(&tree).expect("mkdir");
        std::fs::write(tree.join("a"), [0u8; 100]).expect("write");
        std::fs::write(dir.path().join("t1/b"), [0u8; 20]).expect("write");
        let freed = remove_measured(&dir.path().join("t1")).expect("removed");
        assert_eq!(freed, 120);
        assert!(!dir.path().join("t1").exists());
    }

    #[test]
    fn a_done_ticket_scratch_goes_but_one_under_test_stays() {
        let s = state(
            &[
                ("A", "done", false),
                ("B", "in_qa", false),
                ("C", "done", false),
            ],
            Some(&["C"]),
        );
        let none = BTreeSet::new();
        let removable =
            |id: &str, sessions| scratch_removable(&s, s.ticket(id).unwrap(), "done", sessions);
        assert!(removable("A", &none));
        assert!(!removable("B", &none), "still in QA");
        assert!(!removable("C", &none), "done but its round is open");
        let busy = BTreeSet::from([("qa".to_string(), "A".to_string())]);
        assert!(!removable("A", &busy), "a session is still running");
    }

    #[test]
    fn a_dropped_ticket_is_over() {
        let s = state(&[("D", "in_progress", true)], None);
        assert!(scratch_removable(
            &s,
            s.ticket("D").unwrap(),
            "done",
            &BTreeSet::new()
        ));
    }

    #[test]
    fn startup_sweep_removes_orphans_and_keeps_active_ones() {
        let dir = tempfile::tempdir().expect("tempdir");
        let qa = dir.path().join("qa");
        dirs(&qa, &["done1", "live", "gone", "testing"]);
        let s = state(
            &[
                ("done1", "done", false),
                ("live", "in_progress", false),
                ("testing", "done", false),
            ],
            Some(&["testing"]),
        );
        let found = orphan_qa_dirs(&qa, &s, "done", &BTreeSet::new());
        assert_eq!(names(found), ["done1", "gone"]);
    }

    #[test]
    fn stale_worktrees_spare_the_shared_ones() {
        let dir = tempfile::tempdir().expect("tempdir");
        let wt = dir.path().join("worktrees");
        dirs(
            &wt,
            &[
                "_dev",
                "_qa",
                "_prod",
                "_merge-123",
                "done1",
                "live",
                "unknown",
            ],
        );
        let s = state(
            &[("done1", "done", false), ("live", "in_progress", false)],
            None,
        );
        let found = stale_worktrees(&wt, &s, "done", &BTreeSet::new());
        assert_eq!(names(found), ["_merge-123", "done1"]);
    }
}
