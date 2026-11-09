//! Failure triage: some failures become bugs. Chillax records
//! the failure, briefs the project manager, who files a bug and assigns it to the Tech
//! Lead. Everything here is pure: the projection of failures from events, the
//! fingerprints that dedupe them, and who gets the bug.

use serde::{Deserialize, Serialize};

use crate::engine::{Engine, State};
use crate::model::{EnvStep, TicketId};
use crate::workflow::{IN_PROGRESS, IN_REVIEW};

/// The tag of the role that gets triaged bugs (the built-in Tech Lead). Roles are data:
/// any role with this tag will do.
pub const TRIAGE_OWNER_TAG: &str = "tech-lead";

/// How often the PM is briefed about one failure before Chillax gives up and tells the
/// stakeholder.
pub const MAX_ATTEMPTS: u32 = 3;

/// The most failure output a briefing and the state carry.
pub const OUTPUT_CAP: usize = 3000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// A QA environment `up` or `test` command failed.
    QaEnv,
    /// Merging an approved branch into `dev` conflicted.
    Merge,
    /// Rebasing an open branch onto `dev` conflicted.
    Rebase,
    /// The dev tests failed twice in a row on one ticket.
    DevTests,
}

impl FailureClass {
    pub fn label(self) -> &'static str {
        match self {
            Self::QaEnv => "QA environment failure",
            Self::Merge => "merge conflict",
            Self::Rebase => "rebase conflict",
            Self::DevTests => "failing tests",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriageStatus {
    /// Waiting for a bug: the PM is (or will be) briefed.
    Pending,
    /// A bug was filed for it.
    Filed,
    /// The cause went away before a bug was filed (tests passed, the branch merged).
    Resolved,
    /// The PM never filed a bug; the stakeholder was told.
    Abandoned,
}

/// One failure Chillax may turn into a bug.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub fingerprint: String,
    pub class: FailureClass,
    /// The ticket the failure happened on (none for the QA environment).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<TicketId>,
    /// What failed, for humans: `T-3` or `qa up`.
    pub subject: String,
    /// The tail of the failure output, capped at [`OUTPUT_CAP`].
    #[serde(default)]
    pub output: String,
    /// Times it happened, including repeats that did not file a second bug.
    pub occurrences: u32,
    /// Times the PM was briefed.
    pub attempts: u32,
    pub status: TriageStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bug: Option<TicketId>,
}

/// A stable id for "the same failure": the class, what it happened on, and (for failures
/// whose output identifies them) the output with numbers, hashes and spacing removed.
/// Conflicts are keyed by ticket alone: their text names commits that change each time.
pub fn fingerprint(class: FailureClass, subject: &str, output: &str) -> String {
    let text = match class {
        FailureClass::Merge | FailureClass::Rebase => String::new(),
        FailureClass::QaEnv | FailureClass::DevTests => normalize(output),
    };
    let key = format!("{class:?}|{subject}|{text}");
    // FNV-1a: stable across runs and platforms, unlike the std hasher.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in key.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// The last lines of `output` in lowercase, digits collapsed to `#` and whitespace
/// squeezed, so timings, counts and addresses do not make a failure look new.
fn normalize(output: &str) -> String {
    let lowered = tail_chars(output.trim(), 600).to_lowercase();
    // Addresses (`0xdead`) are as unstable as numbers.
    let tail = lowered
        .split_inclusive(char::is_whitespace)
        .map(|w| {
            if w.trim_start().starts_with("0x") {
                "# "
            } else {
                w
            }
        })
        .collect::<String>();
    let mut out = String::new();
    let mut last_digit = false;
    for c in tail.chars() {
        if c.is_ascii_digit() {
            if !last_digit {
                out.push('#');
            }
            last_digit = true;
        } else {
            last_digit = false;
            if c.is_whitespace() {
                if !out.ends_with(' ') {
                    out.push(' ');
                }
            } else {
                out.push(c);
            }
        }
    }
    out
}

/// The last `max` characters of `text`.
pub fn tail_chars(text: &str, max: usize) -> &str {
    let count = text.chars().count();
    if count <= max {
        return text;
    }
    let skip = count - max;
    let at = text.char_indices().nth(skip).map_or(0, |(i, _)| i);
    &text[at..]
}

/// Record that `class` failed on `subject`. Repeats of a failure with a bug that is
/// still open only count; a failure whose bug is done (or that had gone away) and comes
/// back is a regression and waits for a new bug.
pub fn record_failure(
    state: &mut State,
    class: FailureClass,
    ticket: Option<&str>,
    subject: &str,
    output: &str,
) {
    let fp = fingerprint(class, subject, output);
    let output = tail_chars(output.trim(), OUTPUT_CAP).to_string();
    let terminal = state.workflow_states.last().cloned().unwrap_or_default();
    let bug_open = |state: &State, bug: &Option<TicketId>| {
        bug.as_deref()
            .and_then(|b| state.ticket(b))
            .is_some_and(|t| t.state != terminal && t.dropped.is_none())
    };
    if let Some(i) = state.failures.iter().position(|f| f.fingerprint == fp) {
        let keep_filed = state.failures[i].status == TriageStatus::Filed
            && bug_open(state, &state.failures[i].bug);
        let f = &mut state.failures[i];
        f.occurrences += 1;
        f.output = output;
        if keep_filed || f.status == TriageStatus::Abandoned {
            return;
        }
        if f.status != TriageStatus::Pending {
            f.status = TriageStatus::Pending;
            f.attempts = 0;
        }
        return;
    }
    state.failures.push(Failure {
        fingerprint: fp,
        class,
        ticket: ticket.map(str::to_string),
        subject: subject.to_string(),
        output,
        occurrences: 1,
        attempts: 0,
        status: TriageStatus::Pending,
        bug: None,
    });
}

/// The cause of waiting failures went away: stop waiting for a bug. Failures that
/// already have a bug keep it.
pub fn resolve(state: &mut State, mut matches: impl FnMut(&Failure) -> bool) {
    for f in &mut state.failures {
        if f.status == TriageStatus::Pending && matches(f) {
            f.status = TriageStatus::Resolved;
        }
    }
}

/// Resolve pending failures of `classes` on `ticket`.
pub fn resolve_ticket(state: &mut State, ticket: &str, classes: &[FailureClass]) {
    resolve(state, |f| {
        f.ticket.as_deref() == Some(ticket) && classes.contains(&f.class)
    });
}

/// Whether a pending failure still needs a bug: its cause is still there.
pub fn still_failing(state: &State, f: &Failure) -> bool {
    match f.class {
        FailureClass::QaEnv => state
            .open_round()
            .is_some_and(|r| r.status == crate::model::QaRoundStatus::EnvFailed),
        FailureClass::Merge | FailureClass::Rebase | FailureClass::DevTests => f
            .ticket
            .as_deref()
            .and_then(|t| state.ticket(t))
            .is_some_and(|t| t.state == IN_PROGRESS && t.dropped.is_none()),
    }
}

/// Who gets a triaged bug: the least loaded employee of the [`TRIAGE_OWNER_TAG`] role,
/// else the least loaded who owns both development and review, else any developer.
pub fn bug_owner(engine: &Engine) -> Option<String> {
    engine
        .least_loaded(TRIAGE_OWNER_TAG, None)
        .or_else(|| {
            least_loaded_where(engine, |r| {
                engine.workflow.role_owns(r, IN_PROGRESS) && engine.workflow.role_owns(r, IN_REVIEW)
            })
        })
        .or_else(|| least_loaded_where(engine, |r| engine.workflow.role_owns(r, IN_PROGRESS)))
}

fn least_loaded_where(engine: &Engine, ok: impl Fn(&crate::role::Role) -> bool) -> Option<String> {
    engine
        .state
        .employees
        .iter()
        .filter(|e| engine.roles.get(&e.employee.role).is_some_and(&ok))
        .min_by_key(|e| {
            engine
                .state
                .assignments(&e.employee.id)
                .iter()
                .filter(|t| t.state != engine.workflow.terminal_state())
                .count()
        })
        .map(|e| e.employee.id.clone())
}

/// The sprint a triaged bug joins: its failure's ticket's sprint when that is active,
/// else the latest active sprint.
pub fn bug_sprint(state: &State, f: &Failure) -> Option<String> {
    use crate::model::SprintStatus::Active;
    let own = f
        .ticket
        .as_deref()
        .and_then(|t| state.ticket(t))
        .and_then(|t| t.sprint.as_deref())
        .and_then(|sp| state.sprint(sp))
        .filter(|sp| sp.status == Active);
    own.or_else(|| state.sprints.iter().rev().find(|sp| sp.status == Active))
        .map(|sp| sp.id.clone())
}

/// What the PM is told about a failure.
pub fn briefing(engine: &Engine, f: &Failure, owner: Option<&str>, base: &str) -> String {
    let state = &engine.state;
    let mut out = format!(
        "[Chillax] Failure triage: {} on {} (fingerprint `{}`).\n\n",
        f.class.label(),
        f.subject,
        f.fingerprint
    );
    if let Some(t) = f.ticket.as_deref().and_then(|t| state.ticket(t)) {
        out.push_str(&format!(
            "Ticket {}: {} (state {}{}).\n",
            t.id,
            t.title,
            t.state,
            t.branch
                .as_deref()
                .map(|b| format!(", branch `{b}`"))
                .unwrap_or_default()
        ));
    }
    out.push_str(&match f.class {
        FailureClass::QaEnv => format!(
            "The QA environment command failed while the QA round started; `{base}` was \
             promoted to `qa`.\n"
        ),
        FailureClass::Merge => {
            format!("The approved branch conflicts with `{base}` and could not be merged.\n")
        }
        FailureClass::Rebase => format!("The open branch conflicts with `{base}`.\n"),
        FailureClass::DevTests => {
            "The project's tests failed on the branch twice in a row; the developer has not fixed \
             them.\n"
                .to_string()
        }
    });
    if f.occurrences > 1 {
        out.push_str(&format!("It has happened {} times.\n", f.occurrences));
    }
    if let Some(previous) = &f.bug {
        out.push_str(&format!(
            "An earlier bug for it, {previous}, is done, so this is a regression: file a new bug \
             and mention {previous}.\n"
        ));
    }
    out.push_str(&format!("\nOutput (tail):\n```\n{}\n```\n\n", f.output));
    let who = owner
        .and_then(|o| state.employee(o))
        .map(|e| format!("{} (`{}`)", e.employee.name, e.employee.id));
    out.push_str("Check the failure, then:\n");
    out.push_str(
        "1. `list_tickets` - if an open bug already covers this failure, do not file another; \
         say so in a `comment` on it and stop.\n",
    );
    out.push_str(&format!(
        "2. File the bug with `create_story`: `kind` bug, a clear title, a description with the \
         cause as far as you can tell and the output above, 2-5 testable acceptance criteria, \
         `fingerprint` `{}`{}.\n",
        f.fingerprint,
        who.map(|w| format!(", `assignee` {w}"))
            .unwrap_or_else(|| ", and an `assignee` who can fix it".into())
    ));
    out.push_str(
        "Chillax puts the bug in the active sprint and starts it. Do not change the failing \
         ticket yourself.",
    );
    out
}

/// A one-line note for the inbox when a bug was filed.
pub fn filed_note(state: &State, f: &Failure, bug: &str) -> (String, String) {
    let title = format!("Bug {bug} filed for a {}", f.class.label());
    let owner = state
        .ticket(bug)
        .and_then(|t| t.assignee.as_deref())
        .map(|a| state.actor_name(&crate::event::Actor::Employee(a.to_string())))
        .unwrap_or_else(|| "nobody yet".into());
    let in_sprint = state.ticket(bug).is_some_and(|t| t.sprint.is_some());
    (
        title,
        format!(
            "{} on {}. The project manager filed {bug} and assigned it to {owner}.{}",
            f.class.label(),
            f.subject,
            if in_sprint {
                ""
            } else {
                " There is no active sprint, so it waits in the backlog."
            }
        ),
    )
}

/// The QA step name used in subjects: `qa up`, `qa test`.
pub fn env_subject(step: EnvStep) -> String {
    format!(
        "qa {}",
        match step {
            EnvStep::Up => "up",
            EnvStep::Test => "test",
            EnvStep::Down => "down",
            EnvStep::Deploy => "deploy",
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> State {
        State {
            workflow_states: vec!["backlog".into(), "done".into()],
            ..State::default()
        }
    }

    #[test]
    fn fingerprints_ignore_numbers_and_spacing_but_not_the_failure() {
        let a = fingerprint(
            FailureClass::DevTests,
            "T-1",
            "test x failed in 1.2s\n  at 0xdead",
        );
        let b = fingerprint(
            FailureClass::DevTests,
            "T-1",
            "test x   failed in 9.8s at 0xbeef",
        );
        assert_eq!(a, b);
        assert_ne!(
            a,
            fingerprint(FailureClass::DevTests, "T-1", "test y failed")
        );
        assert_ne!(
            a,
            fingerprint(FailureClass::DevTests, "T-2", "test x failed")
        );
        // Conflicts are keyed by ticket only.
        assert_eq!(
            fingerprint(FailureClass::Merge, "T-1", "CONFLICT a1b2c3"),
            fingerprint(FailureClass::Merge, "T-1", "CONFLICT ffffff")
        );
    }

    #[test]
    fn a_repeat_counts_and_a_resolved_failure_that_returns_waits_again() {
        let mut s = state();
        record_failure(&mut s, FailureClass::Merge, Some("T-1"), "T-1", "conflict");
        record_failure(
            &mut s,
            FailureClass::Merge,
            Some("T-1"),
            "T-1",
            "conflict again",
        );
        assert_eq!(s.failures.len(), 1);
        assert_eq!(s.failures[0].occurrences, 2);
        resolve_ticket(&mut s, "T-1", &[FailureClass::Merge]);
        assert_eq!(s.failures[0].status, TriageStatus::Resolved);
        record_failure(&mut s, FailureClass::Merge, Some("T-1"), "T-1", "conflict");
        assert_eq!(s.failures[0].status, TriageStatus::Pending);
        assert_eq!(s.failures[0].attempts, 0);
    }

    #[test]
    fn output_is_capped_to_its_tail() {
        let mut s = state();
        let long = format!("{}END", "x".repeat(10_000));
        record_failure(&mut s, FailureClass::QaEnv, None, "qa up", &long);
        assert_eq!(s.failures[0].output.chars().count(), OUTPUT_CAP);
        assert!(s.failures[0].output.ends_with("END"));
    }
}
