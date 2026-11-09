//! The text a release writes into the repository: its `CHANGELOG.md` entry and the
//! message of its annotated tag. Pure, so the format is pinned by unit tests.

/// Where the changelog lives, relative to the repository root.
pub const CHANGELOG: &str = "CHANGELOG.md";

const HEADING: &str = "# Changelog";

/// The release notes with `date` added to their `## vX.Y.Z` heading.
pub fn dated_entry(notes: &str, version: &str, date: &str) -> String {
    let plain = format!("## {version}");
    let notes = notes.trim();
    match notes.strip_prefix(&plain) {
        Some(rest) => format!("{plain} — {date}{rest}\n"),
        None => format!("{plain} — {date}\n\n{notes}\n"),
    }
}

/// `existing` (the changelog on `qa`, if any) with `entry` at the top, newest first.
pub fn prepend(existing: Option<&str>, entry: &str) -> String {
    let old = existing
        .map(|text| {
            text.trim_start()
                .strip_prefix(HEADING)
                .unwrap_or(text)
                .trim()
        })
        .unwrap_or_default();
    let mut out = format!("{HEADING}\n\n{}\n", entry.trim_end());
    if !old.is_empty() {
        out.push('\n');
        out.push_str(old);
        out.push('\n');
    }
    out
}

/// True when the changelog already has an entry for `version`, so a release that was
/// interrupted does not write it twice.
pub fn has_entry(changelog: &str, version: &str) -> bool {
    let heading = format!("## {version}");
    changelog.lines().any(|line| {
        line.strip_prefix(&heading)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
    })
}

/// The annotated tag's message: the release, then its notes without their heading.
pub fn tag_message(version: &str, notes: &str) -> String {
    let notes = notes.trim();
    let body = notes
        .strip_prefix(&format!("## {version}"))
        .unwrap_or(notes)
        .trim();
    if body.is_empty() {
        format!("Release {version}")
    } else {
        format!("Release {version}\n\n{body}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOTES: &str = "## v0.2.0\n\n### Features\n\n**Other**\n- T-1 Login\n";

    #[test]
    fn the_date_goes_on_the_heading() {
        assert_eq!(
            dated_entry(NOTES, "v0.2.0", "2026-10-22"),
            "## v0.2.0 — 2026-10-22\n\n### Features\n\n**Other**\n- T-1 Login\n"
        );
        // Notes the stakeholder rewrote without the heading still get one.
        assert_eq!(
            dated_entry("Fixed things.", "v0.2.0", "2026-10-22"),
            "## v0.2.0 — 2026-10-22\n\nFixed things.\n"
        );
    }

    #[test]
    fn new_entries_go_first_under_one_heading() {
        let first = prepend(None, "## v0.1.0 — 2026-10-01\n\n- T-1 Login\n");
        assert_eq!(
            first,
            "# Changelog\n\n## v0.1.0 — 2026-10-01\n\n- T-1 Login\n"
        );
        let second = prepend(Some(&first), "## v0.2.0 — 2026-10-22\n\n- T-2 Logout\n");
        assert_eq!(
            second,
            "# Changelog\n\n## v0.2.0 — 2026-10-22\n\n- T-2 Logout\n\n\
             ## v0.1.0 — 2026-10-01\n\n- T-1 Login\n"
        );
        // A changelog somebody wrote by hand, with no heading of ours, is kept.
        let adopted = prepend(Some("## v0.0.1\n- old\n"), "## v0.1.0\n- new\n");
        assert_eq!(
            adopted,
            "# Changelog\n\n## v0.1.0\n- new\n\n## v0.0.1\n- old\n"
        );
    }

    #[test]
    fn an_existing_entry_is_found_by_its_version() {
        let text = "# Changelog\n\n## v0.10.0 — 2026-10-22\n\n- x\n";
        assert!(has_entry(text, "v0.10.0"));
        assert!(!has_entry(text, "v0.1.0"), "v0.10.0 is not v0.1.0");
        assert!(has_entry("## v0.1.0\n", "v0.1.0"));
    }

    #[test]
    fn the_tag_message_is_the_release_and_its_notes() {
        assert_eq!(
            tag_message("v0.2.0", NOTES),
            "Release v0.2.0\n\n### Features\n\n**Other**\n- T-1 Login"
        );
        assert_eq!(tag_message("v0.2.0", "## v0.2.0"), "Release v0.2.0");
    }
}
