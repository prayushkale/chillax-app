//! Reading personas out of a catalog checkout.
//!
//! Files are listed with `git ls-tree` rather than by walking the directory, so the
//! file mode is known: symlinks (`120000`) and submodules (`160000`) are skipped, which
//! keeps a hostile catalog from pointing a "persona" at files elsewhere on disk.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use chillax_core::PersonaSummary;
use serde::Deserialize;

/// Larger files are not personas; skip them rather than load them.
const MAX_PERSONA_BYTES: u64 = 256 * 1024;

/// Top-level directories that hold tooling or generated output, not personas
/// (used only when the catalog has no `divisions.json`).
const NON_DIVISIONS: &[&str] = &["integrations", "strategy", "examples", "scripts", "docs"];

#[derive(Debug, Clone)]
pub struct Persona {
    pub summary: PersonaSummary,
    /// Git blob id of the file at `commit`.
    pub blob: String,
    /// The markdown body after the frontmatter.
    pub body: String,
}

#[derive(Debug, Clone, Default)]
pub struct Index {
    pub commit: String,
    pub license: Option<String>,
    pub personas: Vec<Persona>,
}

impl Index {
    pub fn get(&self, slug: &str) -> Option<&Persona> {
        self.personas.iter().find(|p| p.summary.slug == slug)
    }
}

#[derive(Deserialize)]
struct Frontmatter {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    emoji: Option<String>,
    #[serde(default)]
    vibe: Option<String>,
}

#[derive(Deserialize)]
struct Divisions {
    divisions: BTreeMap<String, Division>,
}

#[derive(Deserialize)]
struct Division {
    label: String,
}

/// Index the checkout at `dir` (blocking: run it on a blocking thread).
pub fn load(catalog: &str, dir: &Path) -> Result<Index, String> {
    let commit = git(dir, &["rev-parse", "HEAD"])?;
    let listing = git(dir, &["ls-tree", "-r", "-z", "--full-tree", "HEAD"])?;
    let divisions = divisions(dir);

    let mut personas = Vec::new();
    for entry in listing.split('\0').filter(|e| !e.is_empty()) {
        let Some((meta, path)) = entry.split_once('\t') else {
            continue;
        };
        let mut meta = meta.split(' ');
        let (Some(mode), Some(kind), Some(blob)) = (meta.next(), meta.next(), meta.next()) else {
            continue;
        };
        if kind != "blob" || !matches!(mode, "100644" | "100755") || !path.ends_with(".md") {
            continue;
        }
        let Some((top, _)) = path.split_once('/') else {
            continue;
        };
        let division = match &divisions {
            Some(known) => match known.get(top) {
                Some(label) => label.clone(),
                None => continue,
            },
            None if NON_DIVISIONS.contains(&top) || top.starts_with('.') => continue,
            None => title_case(top),
        };
        let Some(source) = read_regular_file(dir, path) else {
            continue;
        };
        let Some((front, body)) = split_frontmatter(&source) else {
            continue;
        };
        let Ok(front) = serde_yaml::from_str::<Frontmatter>(front) else {
            tracing::debug!(path, "skipping persona with unreadable frontmatter");
            continue;
        };
        if front.name.trim().is_empty() || body.trim().is_empty() {
            continue;
        }
        personas.push(Persona {
            summary: PersonaSummary {
                catalog: catalog.to_string(),
                slug: String::new(),
                path: path.to_string(),
                name: one_line(&front.name),
                description: one_line(front.description.as_deref().unwrap_or_default()),
                division,
                emoji: front.emoji.unwrap_or_default().trim().to_string(),
                vibe: one_line(front.vibe.as_deref().unwrap_or_default()),
                hires: 0,
                adopted_role: None,
                suggested_base: None,
            },
            blob: blob.to_string(),
            body: body.trim().to_string(),
        });
    }
    assign_slugs(&mut personas);
    personas.sort_by(|a, b| a.summary.name.cmp(&b.summary.name));

    Ok(Index {
        commit,
        license: license(dir),
        personas,
    })
}

/// Slug = file stem; when two files share a stem, the path makes them unique.
fn assign_slugs(personas: &mut [Persona]) {
    let stem = |path: &str| {
        Path::new(path)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    };
    let mut seen = HashSet::new();
    let mut clashes = HashSet::new();
    for p in personas.iter() {
        let s = stem(&p.summary.path);
        if !seen.insert(s.clone()) {
            clashes.insert(s);
        }
    }
    for p in personas.iter_mut() {
        let s = stem(&p.summary.path);
        p.summary.slug = if clashes.contains(&s) {
            chillax_core::engine::slug(p.summary.path.trim_end_matches(".md"))
        } else {
            chillax_core::engine::slug(&s)
        };
    }
}

/// Read a file only if it is a regular file inside `dir` and small enough.
fn read_regular_file(dir: &Path, rel: &str) -> Option<String> {
    let path = dir.join(rel);
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > MAX_PERSONA_BYTES {
        return None;
    }
    let root = dir.canonicalize().ok()?;
    if !path.canonicalize().ok()?.starts_with(root) {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// `---\n<yaml>\n---\n<body>` → (yaml, body).
pub fn split_frontmatter(source: &str) -> Option<(&str, &str)> {
    let rest = source.trim_start_matches('\u{feff}').strip_prefix("---")?;
    let end = rest.find("\n---")?;
    let (yaml, body) = rest.split_at(end);
    let body = body.trim_start_matches("\n---");
    Some((yaml, body))
}

fn divisions(dir: &Path) -> Option<BTreeMap<String, String>> {
    let source = read_regular_file(dir, "divisions.json")?;
    let parsed: Divisions = serde_json::from_str(&source).ok()?;
    Some(
        parsed
            .divisions
            .into_iter()
            .map(|(dir, d)| (dir, d.label))
            .collect(),
    )
}

fn license(dir: &Path) -> Option<String> {
    let text = read_regular_file(dir, "LICENSE")?;
    let first = text.lines().find(|l| !l.trim().is_empty())?.trim();
    let name = first.strip_suffix(" License").unwrap_or(first);
    Some(name.chars().take(40).collect())
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn title_case(dir: &str) -> String {
    dir.split('-')
        .map(|w| {
            let mut c = w.chars();
            c.next()
                .map(|f| f.to_uppercase().chain(c).collect::<String>())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("running git (is it installed?): {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args[0],
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::tests::{commit, file_url, upstream};

    const BACKEND: &str = "---\nname: Backend Architect\ndescription: >\n  Designs APIs\n  and data.\nemoji: 🏗️\nvibe: Calm.\n---\n\n# Backend\n\nYou design APIs.\n";

    async fn checkout(up: &Path) -> tempfile::TempDir {
        let cache = tempfile::tempdir().expect("tempdir");
        crate::git::sync(&file_url(up), "main", &cache.path().join("c"))
            .await
            .expect("sync");
        cache
    }

    #[tokio::test]
    async fn indexes_divisions_and_parses_folded_frontmatter() {
        let up = upstream(&[
            ("LICENSE", "MIT License\n\nCopyright"),
            ("engineering/engineering-backend.md", BACKEND),
            ("engineering/README.md", "# not a persona\n"),
            ("scripts/tool.md", BACKEND),
            (
                "game-development/unity/unity-dev.md",
                &BACKEND.replace("Backend Architect", "Unity Dev"),
            ),
        ]);
        let cache = checkout(up.path()).await;
        let index = load("agency", &cache.path().join("c")).expect("indexes");

        let names: Vec<_> = index
            .personas
            .iter()
            .map(|p| p.summary.name.as_str())
            .collect();
        assert_eq!(names, ["Backend Architect", "Unity Dev"]);
        let be = index.get("engineering-backend").expect("by slug");
        assert_eq!(be.summary.description, "Designs APIs and data.");
        assert_eq!(be.summary.division, "Engineering");
        assert_eq!(be.summary.emoji, "🏗️");
        assert!(be.body.starts_with("# Backend"));
        assert_eq!(be.blob.len(), 40);
        assert_eq!(index.license.as_deref(), Some("MIT"));
        assert_eq!(
            index.get("unity-dev").map(|p| p.summary.division.as_str()),
            Some("Game Development")
        );
    }

    #[tokio::test]
    async fn divisions_json_limits_directories_and_labels_them() {
        let up = upstream(&[
            (
                "divisions.json",
                r#"{"divisions": {"eng": {"label": "Engineering", "icon": "Code"}}}"#,
            ),
            ("eng/a.md", BACKEND),
            ("other/b.md", BACKEND),
        ]);
        let cache = checkout(up.path()).await;
        let index = load("agency", &cache.path().join("c")).expect("indexes");
        assert_eq!(index.personas.len(), 1);
        assert_eq!(index.personas[0].summary.division, "Engineering");
    }

    #[tokio::test]
    async fn skips_symlinks_and_oversized_files_and_dedupes_slugs() {
        let up = upstream(&[
            ("eng/dup.md", BACKEND),
            ("ops/dup.md", BACKEND),
            (
                "eng/huge.md",
                &format!("{BACKEND}{}", "x".repeat(300 * 1024)),
            ),
        ]);
        let secret = up.path().join("secret.txt");
        std::fs::write(&secret, "---\nname: Leak\n---\nsecret\n").expect("write");
        std::os::unix::fs::symlink(&secret, up.path().join("eng/leak.md")).expect("symlink");
        commit(up.path(), &[]);

        let cache = checkout(up.path()).await;
        let index = load("agency", &cache.path().join("c")).expect("indexes");
        let mut slugs: Vec<_> = index
            .personas
            .iter()
            .map(|p| p.summary.slug.as_str())
            .collect();
        slugs.sort();
        assert_eq!(slugs, ["eng-dup", "ops-dup"]);
    }
}
