//! What a release contains and how it is named. Pure functions
//! over the projected state, so the gate's rules are unit-tested without IO.

use std::cmp::Ordering;
use std::fmt::Write as _;

use crate::engine::State;
use crate::model::*;
use crate::workflow::IN_QA;

/// A `vMAJOR.MINOR.PATCH` version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u64, pub u64, pub u64);

impl Version {
    /// `v1.2.3` or `1.2.3`; no pre-release or build suffixes.
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text
            .trim()
            .strip_prefix('v')
            .unwrap_or(text.trim())
            .split('.');
        let mut next = || parts.next()?.parse::<u64>().ok();
        let version = Self(next()?, next()?, next()?);
        parts.next().is_none().then_some(version)
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "v{}.{}.{}", self.0, self.1, self.2)
    }
}

/// Tickets QA signed off that no release has shipped yet.
pub fn candidates(state: &State) -> Vec<&Ticket> {
    state
        .tickets
        .iter()
        .filter(|t| state.is_finished(t) && !t.checks.contains("released"))
        .collect()
}

/// Tickets that failed QA and wait for a fix; they ship only if the gate is forced.
pub fn known_issues(state: &State) -> Vec<&Ticket> {
    state
        .tickets
        .iter()
        .filter(|t| t.state == IN_QA && t.checks.contains("qa_failed"))
        .collect()
}

/// The highest version among releases that were approved. One still at the gate or
/// rejected has not claimed its version.
pub fn latest_version(state: &State) -> Option<Version> {
    state
        .releases
        .iter()
        .filter(|r| !matches!(r.status, ReleaseStatus::Rejected | ReleaseStatus::Gated))
        .filter_map(|r| Version::parse(&r.version))
        .max()
}

/// The first release is `v0.1.0`; later ones bump the minor version when a story
/// ships and the patch version when only bugs and tasks do.
pub fn suggest_version(state: &State, shipping: &[&Ticket]) -> Version {
    match latest_version(state) {
        None => Version(0, 1, 0),
        Some(Version(major, minor, patch)) => {
            if shipping.iter().any(|t| t.kind == TicketKind::Story) {
                Version(major, minor + 1, 0)
            } else {
                Version(major, minor, patch + 1)
            }
        }
    }
}

/// Release notes in Markdown: stories by epic, fixes, chores, known issues, and what
/// QA did. The dispatcher adds the date when it writes `CHANGELOG.md`.
pub fn notes(
    state: &State,
    version: &Version,
    shipping: &[&Ticket],
    known: &[&Ticket],
    round: Option<&QaRound>,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "## {version}");
    let line = |t: &Ticket| {
        let key = t
            .external
            .as_ref()
            .map(|e| match (&e.key, &e.url) {
                (Some(key), Some(url)) => format!(" ([{key}]({url}))"),
                (Some(key), None) => format!(" ({key})"),
                _ => String::new(),
            })
            .unwrap_or_default();
        format!("- {} {}{key}", t.id, t.title)
    };

    let stories: Vec<&&Ticket> = shipping
        .iter()
        .filter(|t| t.kind == TicketKind::Story)
        .collect();
    if !stories.is_empty() {
        let _ = writeln!(out, "\n### Features");
        let mut epics: Vec<Option<&str>> = Vec::new();
        for t in &stories {
            let epic = t.epic.as_deref();
            if !epics.contains(&epic) {
                epics.push(epic);
            }
        }
        // Epics in the order their first story ships; stories outside one come last.
        epics.sort_by_key(|e| e.is_none());
        for epic in epics {
            let title = epic
                .and_then(|id| state.epics.iter().find(|e| e.id == id))
                .map_or("Other", |e| e.title.as_str());
            let _ = writeln!(out, "\n**{title}**");
            for t in stories.iter().filter(|t| t.epic.as_deref() == epic) {
                let _ = writeln!(out, "{}", line(t));
            }
        }
    }
    for (heading, kind) in [("Fixes", TicketKind::Bug), ("Chores", TicketKind::Task)] {
        let of_kind: Vec<&&Ticket> = shipping.iter().filter(|t| t.kind == kind).collect();
        if of_kind.is_empty() {
            continue;
        }
        let _ = writeln!(out, "\n### {heading}\n");
        for t in of_kind {
            let found = t
                .found_in
                .as_deref()
                .map(|s| format!(" (found in {s})"))
                .unwrap_or_default();
            let _ = writeln!(out, "{}{found}", line(t));
        }
    }
    if !known.is_empty() {
        let _ = writeln!(out, "\n### Known issues\n");
        for t in known {
            let _ = writeln!(out, "{}", line(t));
        }
    }
    if let Some(round) = round {
        let passed = round
            .verdicts
            .values()
            .filter(|v| **v == QaVerdict::Pass)
            .count();
        let _ = writeln!(
            out,
            "\n_QA round {}: {passed} of {} tickets passed._",
            round.number,
            round.tickets.len()
        );
    }
    out
}

/// True when `new` may be released after the releases so far.
pub fn is_newer(state: &State, new: &Version) -> bool {
    latest_version(state).is_none_or(|latest| new.cmp(&latest) == Ordering::Greater)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_and_order() {
        assert_eq!(Version::parse("v1.2.3"), Some(Version(1, 2, 3)));
        assert_eq!(Version::parse("0.10.0"), Some(Version(0, 10, 0)));
        for bad in ["", "v1", "v1.2", "v1.2.3.4", "v1.2.x", "v1.2.3-rc1"] {
            assert_eq!(Version::parse(bad), None, "{bad}");
        }
        assert!(Version(0, 10, 0) > Version(0, 9, 9));
        assert_eq!(Version(1, 0, 2).to_string(), "v1.0.2");
    }

    fn ticket(id: &str, kind: TicketKind, state: &str) -> Ticket {
        Ticket {
            id: id.into(),
            kind,
            title: format!("{id} title"),
            description: String::new(),
            acceptance_criteria: Vec::new(),
            estimate: None,
            epic: None,
            state: state.into(),
            assignee: None,
            sprint: None,
            comments: Vec::new(),
            external: None,
            blocked_by: Vec::new(),
            branch: None,
            worktree: None,
            reviewer: None,
            checks: Default::default(),
            needs_rebase: false,
            review_requested: false,
            found_in: None,
            pr_number: None,
            pr_url: None,
            dropped: None,
            urgent: false,
            conflict: None,
        }
    }

    fn state_with(tickets: Vec<Ticket>) -> State {
        State {
            tickets,
            ..State::default()
        }
    }

    #[test]
    fn only_signed_off_unreleased_tickets_are_candidates() {
        let mut shipped = ticket("T-3", TicketKind::Story, "done");
        shipped.checks.insert("released".into());
        let mut failed = ticket("T-4", TicketKind::Story, "in_qa");
        failed.checks.insert("qa_failed".into());
        let s = state_with(vec![
            ticket("T-1", TicketKind::Story, "done"),
            ticket("T-2", TicketKind::Bug, "in_progress"),
            shipped,
            failed,
        ]);
        let ids = |v: Vec<&Ticket>| v.iter().map(|t| t.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(candidates(&s)), ["T-1"]);
        assert_eq!(ids(known_issues(&s)), ["T-4"]);
    }

    #[test]
    fn the_first_release_is_0_1_0_then_stories_bump_minor_and_fixes_patch() {
        let story = ticket("T-1", TicketKind::Story, "done");
        let bug = ticket("T-2", TicketKind::Bug, "done");
        let mut s = state_with(Vec::new());
        assert_eq!(suggest_version(&s, &[&bug]), Version(0, 1, 0));
        s.releases.push(Release {
            id: "R-1".into(),
            version: "v0.4.2".into(),
            sprint: None,
            round: 1,
            tickets: Vec::new(),
            known_issues: Vec::new(),
            notes: String::new(),
            status: ReleaseStatus::Deployed,
            tag: None,
            commit: None,
        });
        assert_eq!(suggest_version(&s, &[&story, &bug]), Version(0, 5, 0));
        assert_eq!(suggest_version(&s, &[&bug]), Version(0, 4, 3));
        assert!(is_newer(&s, &Version(0, 4, 3)));
        assert!(!is_newer(&s, &Version(0, 4, 2)));
        // A rejected release does not count.
        s.releases[0].status = ReleaseStatus::Rejected;
        assert!(is_newer(&s, &Version(0, 1, 0)));
    }

    #[test]
    fn notes_group_stories_by_epic_and_list_fixes_and_known_issues() {
        let mut s = state_with(Vec::new());
        s.epics.push(Epic {
            id: "EP-1".into(),
            title: "Accounts".into(),
            description: String::new(),
            external: None,
        });
        let mut login = ticket("T-1", TicketKind::Story, "done");
        login.epic = Some("EP-1".into());
        login.external = Some(ExternalRef {
            id: "uuid".into(),
            key: Some("CHILL-3".into()),
            url: Some("http://tracker/CHILL-3".into()),
        });
        let loose = ticket("T-2", TicketKind::Story, "done");
        let mut fix = ticket("T-3", TicketKind::Bug, "done");
        fix.found_in = Some("T-1".into());
        let open = ticket("T-4", TicketKind::Story, "in_qa");
        let round = QaRound {
            number: 2,
            sprint: None,
            commit: "abc".into(),
            status: QaRoundStatus::Closed,
            tickets: vec!["T-1".into(), "T-2".into(), "T-4".into()],
            verdicts: [
                ("T-1".to_string(), QaVerdict::Pass),
                ("T-2".to_string(), QaVerdict::Pass),
                ("T-4".to_string(), QaVerdict::Fail),
            ]
            .into(),
            env_retries: Default::default(),
            up_done: true,
        };
        let text = notes(
            &s,
            &Version(0, 2, 0),
            &[&loose, &login, &fix],
            &[&open],
            Some(&round),
        );
        let expected = "\
## v0.2.0

### Features

**Accounts**
- T-1 T-1 title ([CHILL-3](http://tracker/CHILL-3))

**Other**
- T-2 T-2 title

### Fixes

- T-3 T-3 title (found in T-1)

### Known issues

- T-4 T-4 title

_QA round 2: 2 of 3 tickets passed._
";
        assert_eq!(text, expected);
    }
}
