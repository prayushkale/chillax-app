//! The project's own plan and convention files, for the prompt's project-context
//! section and for tailoring a persona to the project.

use std::path::Path;

/// Files (and the ADR directory, with a trailing `/`) a hire is pointed at, in order.
pub const PROJECT_FILES: &[&str] = &[
    "plan.md",
    "AGENTS.md",
    "CLAUDE.md",
    "DESIGN.md",
    "README.md",
    "docs/adr/",
];

/// At most this many bytes are quoted from each file when tailoring.
pub const EXCERPT_BYTES: usize = 4 * 1024;
/// At most this many bytes of excerpts in all.
pub const EXCERPTS_TOTAL_BYTES: usize = 16 * 1024;

/// What the editor is asked to do when tailoring.
pub const TAILOR_INSTRUCTION: &str = "Tailor this persona to the project described in the \
project excerpts: its stack, conventions and architecture rules. Keep the role's core \
responsibilities.";

/// Which of [`PROJECT_FILES`] exist under `root`.
pub fn find(root: &Path) -> Vec<String> {
    PROJECT_FILES
        .iter()
        .filter(|name| match name.strip_suffix('/') {
            Some(dir) => root.join(dir).is_dir(),
            None => root.join(name).is_file(),
        })
        .map(|name| (*name).to_string())
        .collect()
}

/// Bounded excerpts of `files` (as returned by [`find`]): the first [`EXCERPT_BYTES`] of
/// each file, [`EXCERPTS_TOTAL_BYTES`] in all, cut on a char boundary; a directory is
/// only listed by its `.md` file names. `None` when there is nothing to quote.
pub fn excerpts(root: &Path, files: &[String]) -> Option<String> {
    let mut out = String::new();
    for name in files {
        let budget = EXCERPTS_TOTAL_BYTES.saturating_sub(out.len());
        if budget == 0 {
            break;
        }
        let body = match name.strip_suffix('/') {
            Some(dir) => {
                let mut names: Vec<String> = std::fs::read_dir(root.join(dir))
                    .map(|entries| {
                        entries
                            .filter_map(Result::ok)
                            .map(|e| e.file_name().to_string_lossy().into_owned())
                            .filter(|n| n.ends_with(".md"))
                            .collect()
                    })
                    .unwrap_or_default();
                names.sort();
                names.join("\n")
            }
            None => match std::fs::read(root.join(name)) {
                Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                Err(err) => {
                    tracing::debug!(%err, file = %name, "skipping an unreadable project file");
                    continue;
                }
            },
        };
        let header = format!("=== {name} ===\n");
        let limit = EXCERPT_BYTES.min(budget.saturating_sub(header.len() + 1));
        let body = clip(body.trim(), limit);
        if body.is_empty() {
            continue;
        }
        out.push_str(&header);
        out.push_str(body);
        out.push('\n');
    }
    (!out.is_empty()).then_some(out)
}

/// The longest prefix of `text` within `max` bytes that ends on a char boundary.
fn clip(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_only_the_files_that_exist() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("plan.md"), "# Plan").expect("write");
        std::fs::write(dir.path().join("README.md"), "# Readme").expect("write");
        std::fs::create_dir_all(dir.path().join("docs/adr")).expect("mkdir");
        // A directory named like a file, or a file named like the directory, don't count.
        std::fs::create_dir(dir.path().join("AGENTS.md")).expect("mkdir");
        assert_eq!(find(dir.path()), ["plan.md", "README.md", "docs/adr/"]);
        assert!(find(&dir.path().join("missing")).is_empty());
    }

    #[test]
    fn excerpts_are_bounded_and_cut_on_char_boundaries() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Multi-byte characters straddle every cut point.
        let long = "é".repeat(EXCERPT_BYTES);
        for name in [
            "plan.md",
            "AGENTS.md",
            "CLAUDE.md",
            "DESIGN.md",
            "README.md",
        ] {
            std::fs::write(dir.path().join(name), &long).expect("write");
        }
        std::fs::create_dir_all(dir.path().join("docs/adr")).expect("mkdir");
        std::fs::write(dir.path().join("docs/adr/0001-x.md"), "secret body").expect("write");
        let files = find(dir.path());
        let text = excerpts(dir.path(), &files).expect("excerpts");
        assert!(text.len() <= EXCERPTS_TOTAL_BYTES, "{}", text.len());
        let plan = text
            .split("=== plan.md ===\n")
            .nth(1)
            .and_then(|rest| rest.split("\n===").next())
            .expect("plan excerpt");
        assert!(plan.len() <= EXCERPT_BYTES && plan.len() > EXCERPT_BYTES - 4);
        // The total budget runs out before the later files.
        assert!(!text.contains("=== README.md ==="));

        let small = tempfile::tempdir().expect("tempdir");
        std::fs::write(small.path().join("AGENTS.md"), "Use Rust.\n").expect("write");
        std::fs::create_dir_all(small.path().join("docs/adr")).expect("mkdir");
        std::fs::write(small.path().join("docs/adr/0002-b.md"), "body").expect("write");
        std::fs::write(small.path().join("docs/adr/0001-a.md"), "body").expect("write");
        let text = excerpts(small.path(), &find(small.path())).expect("excerpts");
        assert_eq!(
            text,
            "=== AGENTS.md ===\nUse Rust.\n=== docs/adr/ ===\n0001-a.md\n0002-b.md\n"
        );
        assert_eq!(excerpts(small.path(), &[]), None);
    }
}
