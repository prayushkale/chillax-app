//! The first message of a ticket session: everything the agent needs to start, so a
//! session that restarts (after a crash or a pause) picks up where the ticket stands.

use std::path::Path;

use chillax_core::{QaRound, Ticket};

/// How many of the latest ticket comments a briefing repeats, and how long each may be.
const HISTORY: usize = 3;
const COMMENT_CHARS: usize = 1500;

pub fn work(ticket: &Ticket, base: &str) -> String {
    let branch = ticket.branch.as_deref().unwrap_or("(none yet)");
    let mut out = format!(
        "You are assigned ticket {id}: {title}\n\nBranch: {branch}. Your working directory is \
         a git worktree with that branch checked out, cut from `{base}`. Commit your work here.\n",
        id = ticket.id,
        title = ticket.title,
    );
    details(&mut out, ticket);
    rework(&mut out, ticket);
    history(&mut out, ticket);
    out.push_str(
        "\nWhen the work is done and the tests pass, call `request_review` with a summary of \
         the change and how you tested it. Chillax then runs the tests and finds a reviewer. \
         Do not switch branches, merge or push.",
    );
    out
}

/// The focused turn of an assignee whose branch conflicts with `base`. It
/// overrides the work briefing's "do not merge": merging `base` is the whole task. Sent
/// into a running session, or as the briefing of a new one.
pub fn resolve_conflict(ticket: &Ticket, base: &str, files: &[String]) -> String {
    let branch = ticket.branch.as_deref().unwrap_or("(none)");
    let approved = ticket.checks.contains("review_approved");
    let what = if approved {
        "Your approved change"
    } else {
        "Your change"
    };
    let files = if files.is_empty() {
        "files git did not name (run the merge to see them)".to_string()
    } else {
        files
            .iter()
            .map(|f| format!("`{f}`"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut out = format!(
        "[Chillax] {what} {id}: {title} (branch {branch}) conflicts with `{base}` in: \
         {files}.\n\nIn your working directory, run `git merge {base}`, resolve only these \
         conflicts keeping the intent of both sides, run the tests and commit the merge. Do \
         not change behaviour beyond the resolution, and do not push.\n",
        id = ticket.id,
        title = ticket.title,
    );
    details(&mut out, ticket);
    out.push_str(if approved {
        "\nThen call `request_review`. Chillax re-runs the tests and merges; it will not be \
         reviewed again."
    } else {
        "\nThen carry on with the ticket and call `request_review` when it is done."
    });
    out
}

pub fn review(ticket: &Ticket, base: &str, diff: &str) -> String {
    let branch = ticket.branch.as_deref().unwrap_or("(unknown)");
    let developer = ticket.assignee.as_deref().unwrap_or("the developer");
    let mut out = format!(
        "You are reviewing ticket {id}: {title}, built by {developer}.\n\nBranch: {branch}. Your \
         working directory has their branch checked out. Treat it as read-only.\n",
        id = ticket.id,
        title = ticket.title,
    );
    details(&mut out, ticket);
    history(&mut out, ticket);
    out.push_str(&format!(
        "\nWhat the branch adds to `{base}` (`git diff {base}...HEAD`):\n\n```diff\n{}\n```\n",
        diff.trim_end()
    ));
    out.push_str(
        "\nCheck the change against the acceptance criteria, then call `submit_review` with \
         `approve`, or `request_changes` and specific notes. Chillax merges approved work \
         into `dev`; do not merge anything yourself.",
    );
    out
}

/// The briefing of a QA session: one ticket of the round, tested in the shared `qa`
/// tree, which is read-only for the tester.
pub fn qa(
    ticket: &Ticket,
    round: &QaRound,
    tree: &Path,
    scratch: &Path,
    bugs: &[&Ticket],
) -> String {
    let mut out = format!(
        "You are testing ticket {id}: {title} in QA round {number}.\n\nYour working directory is \
         the shared `qa` worktree at commit {commit}. Chillax has brought the QA environment up \
         and its own checks passed. Treat the tree ({tree}) as read-only: run the project's \
         commands, but do not edit files, commit, or switch branches. Write any scratch files \
         only under {scratch}.\n",
        id = ticket.id,
        title = ticket.title,
        number = round.number,
        commit = round.commit.get(..7).unwrap_or(&round.commit),
        tree = tree.display(),
        scratch = scratch.display(),
    );
    details(&mut out, ticket);
    if !bugs.is_empty() {
        out.push_str("\nBugs filed against this ticket earlier (check that each is fixed):\n");
        for bug in bugs {
            out.push_str(&format!("- {}: {} ({})\n", bug.id, bug.title, bug.state));
        }
    }
    history(&mut out, ticket);
    out.push_str(
        "\nExercise every acceptance criterion, then call `submit_qa` once:\n\
         - `pass` with a result and the evidence you saw for every criterion;\n\
         - `fail` with the failed criteria and a bug (a title, and steps to reproduce, expected \
         and actual behaviour);\n\
         - `env` when the environment is broken rather than the product (it is retried once, \
         then goes to the stakeholder).\n\
         Chillax decides what happens next; do not move the ticket yourself.",
    );
    out
}

fn details(out: &mut String, ticket: &Ticket) {
    if !ticket.description.trim().is_empty() {
        out.push_str(&format!("\nDescription:\n{}\n", ticket.description.trim()));
    }
    if !ticket.acceptance_criteria.is_empty() {
        out.push_str("\nAcceptance criteria:\n");
        for criterion in &ticket.acceptance_criteria {
            out.push_str(&format!("- [ ] {criterion}\n"));
        }
    }
}

/// Every ticket session starts from a clean context, so a rework spells out what the last
/// attempt got wrong: the latest review, test or merge failure, unless a pass followed it.
fn rework(out: &mut String, ticket: &Ticket) {
    const FAILURES: [&str; 3] = ["Changes requested", "Tests failed", "Could not merge"];
    const PASSES: [&str; 3] = ["Tests passed", "Tests skipped", "Review approved"];
    let Some(at) = ticket
        .comments
        .iter()
        .rposition(|c| FAILURES.iter().any(|f| c.body.starts_with(f)))
    else {
        return;
    };
    if ticket.comments[at + 1..]
        .iter()
        .any(|c| PASSES.iter().any(|p| c.body.starts_with(p)))
    {
        return;
    }
    let body: String = ticket.comments[at]
        .body
        .chars()
        .take(COMMENT_CHARS * 2)
        .collect();
    out.push_str(&format!(
        "\nThis ticket came back for rework. A fresh session has no memory of the earlier \
         attempt; the branch has its commits. What needs fixing:\n\n{}\n",
        body.trim()
    ));
}

fn history(out: &mut String, ticket: &Ticket) {
    let start = ticket.comments.len().saturating_sub(HISTORY);
    let recent = &ticket.comments[start..];
    if recent.is_empty() {
        return;
    }
    out.push_str("\nRecent history on the ticket:\n");
    for comment in recent {
        let body: String = comment.body.chars().take(COMMENT_CHARS).collect();
        out.push_str(&format!("- {}: {}\n", comment.author, body.trim()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chillax_core::{Comment, TicketKind};

    fn ticket() -> Ticket {
        Ticket {
            id: "T-1".into(),
            kind: TicketKind::Story,
            title: "Login".into(),
            description: "As a user I log in.".into(),
            acceptance_criteria: vec!["valid creds work".into(), "bad creds fail".into()],
            estimate: None,
            epic: None,
            state: "in_progress".into(),
            assignee: Some("dev".into()),
            sprint: None,
            comments: vec![],
            external: None,
            blocked_by: vec![],
            branch: Some("feat/T-1-login".into()),
            worktree: Some(".chillax/worktrees/T-1".into()),
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

    #[test]
    fn work_briefing_has_the_ticket_the_branch_and_the_way_out() {
        let text = work(&ticket(), "dev");
        assert!(text.contains("T-1: Login"), "{text}");
        assert!(text.contains("feat/T-1-login"), "{text}");
        assert!(text.contains("- [ ] bad creds fail"), "{text}");
        assert!(text.contains("request_review"), "{text}");
        assert!(!text.contains("Recent history"), "{text}");
    }

    #[test]
    fn a_rework_briefing_repeats_what_the_last_attempt_got_wrong() {
        let mut t = ticket();
        let say = |t: &mut Ticket, body: &str| {
            t.comments.push(Comment {
                author: "Chillax".into(),
                body: body.into(),
                external_id: None,
                attachments: Vec::new(),
            });
        };
        say(&mut t, "Changes requested.\n\nHandle empty passwords.");
        for n in 0..4 {
            say(&mut t, &format!("filler {n}"));
        }
        let text = work(&t, "dev");
        assert!(text.contains("came back for rework"), "{text}");
        assert!(text.contains("Handle empty passwords."), "{text}");
        // A pass after the failure means the rework is done.
        say(&mut t, "Tests passed.");
        assert!(!work(&t, "dev").contains("came back for rework"));
        assert!(!work(&ticket(), "dev").contains("came back for rework"));
    }

    #[test]
    fn a_restarted_session_sees_the_latest_feedback() {
        let mut t = ticket();
        for n in 0..5 {
            t.comments.push(Comment {
                author: "Lee".into(),
                body: format!("note {n}"),
                external_id: None,
                attachments: Vec::new(),
            });
        }
        let text = work(&t, "dev");
        assert!(
            text.contains("Lee: note 4") && text.contains("Lee: note 2"),
            "{text}"
        );
        assert!(!text.contains("note 1"), "{text}");
    }

    #[test]
    fn qa_briefing_names_the_round_the_tree_and_the_verdicts() {
        let round = QaRound {
            number: 2,
            sprint: None,
            commit: "abcdef0123".into(),
            status: chillax_core::QaRoundStatus::Testing,
            tickets: vec!["T-1".into()],
            verdicts: Default::default(),
            env_retries: Default::default(),
            up_done: true,
        };
        let mut bug = ticket();
        bug.id = "T-5".into();
        bug.title = "Login 500s".into();
        bug.state = "in_qa".into();
        let text = qa(
            &ticket(),
            &round,
            Path::new("/repo/.chillax/worktrees/_qa"),
            Path::new("/repo/.chillax/state/qa/T-1"),
            &[&bug],
        );
        assert!(text.contains("T-1: Login in QA round 2"), "{text}");
        assert!(text.contains("commit abcdef0"), "{text}");
        assert!(text.contains("/repo/.chillax/worktrees/_qa"), "{text}");
        assert!(text.contains("under /repo/.chillax/state/qa/T-1"), "{text}");
        assert!(text.contains("- [ ] bad creds fail"), "{text}");
        assert!(text.contains("T-5: Login 500s (in_qa)"), "{text}");
        for word in ["submit_qa", "pass", "fail", "env"] {
            assert!(text.contains(word), "{word}: {text}");
        }
        let clean = qa(
            &ticket(),
            &round,
            Path::new("/repo/.chillax/worktrees/_qa"),
            Path::new("/repo/.chillax/state/qa/T-1"),
            &[],
        );
        assert!(!clean.contains("Bugs filed against"), "{clean}");
    }

    #[test]
    fn review_briefing_carries_the_diff_and_forbids_merging() {
        let text = review(&ticket(), "dev", "+hello\n");
        assert!(text.contains("built by dev"), "{text}");
        assert!(text.contains("```diff\n+hello\n```"), "{text}");
        assert!(text.contains("submit_review"), "{text}");
        assert!(text.contains("do not merge anything yourself"), "{text}");
    }
}
