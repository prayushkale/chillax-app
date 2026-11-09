//! Hiring from persona catalogs (ADR 0003).
//!
//! A catalog (e.g. agency-agents) supplies persona *text* only. The stakeholder picks a
//! trusted base role for its policy, reviews the text, optionally has an agent revise it,
//! and adopts the result as a project role. Drafts are ephemeral; only adoption is a
//! command (`AdoptRole`). The [`Recruiter`] adapter does the IO; the ranking and search
//! here are pure so every frontend filters the same way.

use std::collections::BTreeMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::CommandError;
use crate::role::Role;

/// One persona in a catalog. `slug` (the file stem) is unique within a catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersonaSummary {
    pub catalog: String,
    pub slug: String,
    pub path: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Display label of the division (top-level directory), e.g. `Engineering`.
    #[serde(default)]
    pub division: String,
    #[serde(default)]
    pub emoji: String,
    #[serde(default)]
    pub vibe: String,
    /// How often the stakeholder has hired roles adopted from this persona.
    #[serde(default)]
    pub hires: u32,
    /// The project role already adopted from this persona, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adopted_role: Option<String>,
    /// The trusted base role a draft gets when the stakeholder doesn't pick one
    /// ([`suggest_base`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_base: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warn,
}

/// A line in persona text that deserves a human look before adoption.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// 1-based line in the persona text.
    pub line: usize,
    pub severity: Severity,
    /// Short rule name, e.g. `override-instructions`.
    pub rule: String,
    pub message: String,
    pub excerpt: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffOp {
    Same,
    Added,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    pub op: DiffOp,
    pub text: String,
}

/// One agent revision of the persona text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revision {
    /// What the stakeholder asked for.
    pub instruction: String,
    /// The editor's own summary of what it changed.
    pub summary: String,
    /// Persona text after this revision.
    pub text: String,
}

/// A persona being reviewed for adoption.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleDraft {
    pub id: String,
    pub persona: PersonaSummary,
    /// Trusted role whose policy (tools, permissions, states) the new role inherits.
    pub base_role: String,
    /// Agent backend the employee will use; revisions run on it too.
    pub backend: String,
    /// Model on `backend` (agent default when `None`); revisions use it too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Proposed id for the adopted role.
    pub role_id: String,
    /// The role this draft is a copy of (`draft_from_role`); `None` for a catalog persona.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_role: Option<String>,
    /// The base role's prompt (trusted, not editable here).
    pub base_prompt: String,
    /// Upstream persona text as fetched.
    pub original: String,
    /// Persona text after the latest revision (equals `original` when unrevised).
    pub persona_text: String,
    pub findings: Vec<Finding>,
    pub revisions: Vec<Revision>,
    /// `original` → `persona_text`, line by line.
    pub diff: Vec<DiffLine>,
    /// The full prompt the employee will get (base prompt + project context + guard +
    /// persona).
    pub prompt: String,
    /// Project files the prompt points the employee at (`plan.md`, `AGENTS.md`, …,
    /// `docs/adr/`), found in the project root when the draft was made.
    #[serde(default)]
    pub project_files: Vec<String>,
}

/// State of a catalog's local checkout.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogStatus {
    pub name: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<DateTime<Utc>>,
    pub personas: usize,
    /// True while a refresh is running.
    #[serde(default)]
    pub refreshing: bool,
    /// Last refresh error; the cached checkout (if any) is still used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum RecruitError {
    #[error("the persona catalog is disabled (see [catalog] in chillax.toml)")]
    Disabled,
    #[error("the persona catalog has not been fetched yet; try again after the refresh")]
    NotReady,
    #[error("unknown persona `{0}`")]
    UnknownPersona(String),
    #[error("unknown role `{0}`")]
    UnknownRole(String),
    #[error("unknown draft `{0}`")]
    UnknownDraft(String),
    #[error("nothing to undo")]
    NothingToUndo,
    #[error("prompt editor failed: {0}")]
    Editor(String),
    #[error("catalog refresh failed: {0}")]
    Refresh(String),
    #[error("{0}")]
    Command(#[from] CommandError),
    #[error("{0}")]
    Io(String),
}

/// Catalog access for frontends. Implemented by `chillax-catalog` (local) and by the
/// CLI over `/api/v1` when a server is already running.
#[async_trait]
pub trait Recruiter: Send + Sync {
    async fn status(&self) -> CatalogStatus;
    /// Fetch the latest catalog, keeping the cache on failure.
    async fn refresh(&self) -> Result<CatalogStatus, RecruitError>;
    /// Every persona, with hire counts and adopted roles filled in.
    async fn personas(&self) -> Result<Vec<PersonaSummary>, RecruitError>;
    /// The top personas to show before the stakeholder searches.
    async fn featured(&self, limit: usize) -> Result<Vec<PersonaSummary>, RecruitError>;
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<PersonaSummary>, RecruitError> {
        Ok(search(&self.personas().await?, query, limit))
    }
    /// Start reviewing a persona on top of a base role (the suggested one if `None`).
    /// `backend`/`model` default to the stakeholder's hiring defaults.
    async fn draft(
        &self,
        persona: &str,
        base_role: Option<&str>,
        backend: Option<String>,
        model: Option<String>,
    ) -> Result<RoleDraft, RecruitError>;
    /// Start a new role from an existing one (built-in, project or adopted). The draft's
    /// persona text is the reference's prompt body. Works with the catalog disabled.
    async fn draft_from_role(
        &self,
        from_role: &str,
        new_id: &str,
        title: &str,
        backend: Option<String>,
        model: Option<String>,
    ) -> Result<RoleDraft, RecruitError>;
    async fn get_draft(&self, id: &str) -> Result<RoleDraft, RecruitError>;
    /// Put the draft on another trusted base role, keeping its revisions.
    async fn set_base(&self, id: &str, base_role: &str) -> Result<RoleDraft, RecruitError>;
    /// Have an agent apply the stakeholder's instruction to the persona text.
    async fn revise(&self, id: &str, instruction: &str) -> Result<RoleDraft, RecruitError>;
    /// Have an agent tailor the persona to the project, from excerpts of its plan files
    /// (`RoleDraft::project_files`). Recorded as an ordinary revision.
    async fn tailor(&self, id: &str) -> Result<RoleDraft, RecruitError> {
        let _ = id;
        Err(RecruitError::Editor(
            "tailoring to the project is not available here".into(),
        ))
    }
    async fn undo(&self, id: &str) -> Result<RoleDraft, RecruitError>;
    /// Adopt the draft as a project role (command + `.chillax/roles/<id>.md`).
    async fn adopt(
        &self,
        id: &str,
        role_id: Option<String>,
        overwrite: bool,
    ) -> Result<Role, RecruitError>;
    async fn discard(&self, id: &str);
}

/// The editor's answer: the new persona text and what it changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rewrite {
    pub text: String,
    pub summary: String,
}

/// Rewrites persona text on request. `chillax-agents` runs this as a one-shot ACP
/// session with no tools and no repository access.
#[async_trait]
pub trait PromptEditor: Send + Sync {
    /// `context` is reference material for the rewrite (e.g. excerpts of the project's
    /// plan files). Like the persona, it is data, never instructions.
    async fn rewrite(
        &self,
        backend: &str,
        model: Option<&str>,
        persona: &str,
        instruction: &str,
        context: Option<&str>,
    ) -> Result<Rewrite, String>;
}

/// Hire counts by persona path: hires of each role, attributed to the persona the role
/// was adopted from.
pub fn hires_by_persona(
    roles: &[Role],
    hires_by_role: &BTreeMap<String, u32>,
) -> BTreeMap<String, u32> {
    let mut out = BTreeMap::new();
    for role in roles {
        if let Some(source) = role.source.as_ref().filter(|s| s.copy_of.is_none()) {
            *out.entry(source.path.clone()).or_default() +=
                hires_by_role.get(&role.id).copied().unwrap_or(0);
        }
    }
    out
}

/// The trusted base role to start a persona's review on, so the stakeholder only
/// steps in when the guess is wrong. In order:
///
/// 1. the base of the role already adopted from this persona;
/// 2. the most specific `*name pattern*` rule matching the persona's name;
/// 3. the base the stakeholder used most for adopted personas of the same division
///    (so corrections stick);
/// 4. the rule for the persona's division (its top-level directory or label);
/// 5. the `"*"` rule, then the first trusted role.
///
/// Only trusted roles (no catalog `source`) qualify; rules naming others are skipped.
pub fn suggest_base(
    persona: &PersonaSummary,
    rules: &BTreeMap<String, String>,
    roles: &[Role],
) -> Option<String> {
    let trusted = |id: &str| roles.iter().any(|r| r.id == id && r.source.is_none());
    let pick = |id: &str| trusted(id).then(|| id.to_string());

    let adopted = persona
        .adopted_role
        .as_deref()
        .and_then(|id| roles.iter().find(|r| r.id == id))
        .and_then(|r| r.base_role.as_deref())
        .and_then(pick);

    let name = persona.name.to_lowercase();
    let mut patterns: Vec<(&String, &String)> = rules
        .iter()
        .filter(|(k, _)| k.contains('*') && k.as_str() != "*")
        .collect();
    // Most specific first: more literal characters.
    patterns.sort_by_key(|(k, _)| std::cmp::Reverse(k.replace('*', "").len()));
    let by_name = || {
        patterns
            .iter()
            .filter(|(k, _)| glob(&k.to_lowercase(), &name))
            .find_map(|(_, role)| pick(role))
    };

    let division = division_of(&persona.path);
    let learned = || {
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for role in roles {
            if let (Some(src), Some(base)) = (&role.source, &role.base_role)
                && src.catalog == persona.catalog
                && division_of(&src.path) == division
                && trusted(base)
            {
                *counts.entry(base.as_str()).or_default() += 1;
            }
        }
        counts
            .into_iter()
            .max_by_key(|(_, n)| *n)
            .map(|(id, _)| id.to_string())
    };

    let label = persona.division.to_lowercase();
    let by_division = || {
        rules
            .iter()
            .filter(|(k, _)| !k.contains('*'))
            .filter(|(k, _)| {
                let k = k.to_lowercase();
                k == division || k == label || k.replace('-', " ") == label
            })
            .find_map(|(_, role)| pick(role))
    };

    adopted
        .or_else(by_name)
        .or_else(learned)
        .or_else(by_division)
        .or_else(|| rules.get("*").and_then(|r| pick(r)))
        .or_else(|| {
            roles
                .iter()
                .find(|r| r.source.is_none())
                .map(|r| r.id.clone())
        })
}

/// A persona's division key: the top-level directory of its path, lowercased.
fn division_of(path: &str) -> String {
    match path.split_once('/') {
        Some((dir, _)) => dir.to_lowercase(),
        None => String::new(),
    }
}

/// `*` matches any run of characters; everything else literally.
fn glob(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    let (first, rest) = parts.split_first().expect("split yields at least one part");
    let Some(mut text) = text.strip_prefix(first) else {
        return false;
    };
    let Some((last, middle)) = rest.split_last() else {
        return text.is_empty();
    };
    for part in middle {
        match text.find(part) {
            Some(at) => text = &text[at + part.len()..],
            None => return false,
        }
    }
    text.ends_with(last)
}

/// Top personas: the ones the stakeholder hires most first, then the curated order.
pub fn featured(
    personas: &[PersonaSummary],
    curated: &[String],
    limit: usize,
) -> Vec<PersonaSummary> {
    let rank = |p: &PersonaSummary| curated.iter().position(|c| *c == p.path);
    let mut picked: Vec<&PersonaSummary> = personas
        .iter()
        .filter(|p| p.hires > 0 || rank(p).is_some())
        .collect();
    picked.sort_by_key(|p| (std::cmp::Reverse(p.hires), rank(p).unwrap_or(usize::MAX)));
    picked.into_iter().take(limit).cloned().collect()
}

/// Case-insensitive search. Every query word must match somewhere; name matches rank
/// above division/slug matches, which rank above description matches.
pub fn search(personas: &[PersonaSummary], query: &str, limit: usize) -> Vec<PersonaSummary> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if words.is_empty() {
        return personas.iter().take(limit).cloned().collect();
    }
    let mut scored: Vec<(u32, &PersonaSummary)> = personas
        .iter()
        .filter_map(|p| {
            let name = p.name.to_lowercase();
            let tags = format!("{} {}", p.division, p.slug).to_lowercase();
            let text = format!("{} {}", p.description, p.vibe).to_lowercase();
            let mut score = 0;
            for word in &words {
                score += match () {
                    _ if name.starts_with(word.as_str()) => 4,
                    _ if name.contains(word.as_str()) => 3,
                    _ if tags.contains(word.as_str()) => 2,
                    _ if text.contains(word.as_str()) => 1,
                    _ => return None,
                };
            }
            Some((score + p.hires.min(5), p))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, p)| p.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn persona(slug: &str, name: &str, description: &str) -> PersonaSummary {
        PersonaSummary {
            catalog: "agency".into(),
            slug: slug.into(),
            path: format!("eng/{slug}.md"),
            name: name.into(),
            description: description.into(),
            division: "Engineering".into(),
            emoji: String::new(),
            vibe: String::new(),
            hires: 0,
            adopted_role: None,
            suggested_base: None,
        }
    }

    fn catalog() -> Vec<PersonaSummary> {
        vec![
            persona("backend", "Backend Architect", "APIs and databases"),
            persona(
                "frontend",
                "Frontend Developer",
                "React UIs that talk to APIs",
            ),
            persona("seo", "SEO Specialist", "search rankings"),
        ]
    }

    #[test]
    fn search_requires_every_word_and_ranks_names_first() {
        let found = search(&catalog(), "api", 10);
        let names: Vec<_> = found.iter().map(|p| p.slug.as_str()).collect();
        assert_eq!(names, ["backend", "frontend"]);

        let found = search(&catalog(), "front api", 10);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].slug, "frontend");

        assert!(search(&catalog(), "kubernetes", 10).is_empty());
        assert_eq!(search(&catalog(), "  ", 2).len(), 2);
    }

    #[test]
    fn featured_puts_hired_personas_before_the_curated_order() {
        let mut personas = catalog();
        personas[2].hires = 2;
        let curated = vec!["eng/frontend.md".to_string(), "eng/backend.md".to_string()];
        let top: Vec<_> = featured(&personas, &curated, 10)
            .into_iter()
            .map(|p| p.slug)
            .collect();
        assert_eq!(top, ["seo", "frontend", "backend"]);
        assert_eq!(featured(&personas, &curated, 1).len(), 1);
    }

    fn role(id: &str) -> Role {
        Role::parse(&format!("---\nid: {id}\ntitle: {id}\n---\nx\n")).expect("role")
    }

    fn adopted(id: &str, base: &str, path: &str) -> Role {
        let mut r = role(id);
        r.base_role = Some(base.into());
        r.source = Some(crate::role::RoleSource {
            catalog: "agency".into(),
            repo: String::new(),
            commit: String::new(),
            path: path.into(),
            blob: String::new(),
            license: None,
            edited: false,
            copy_of: None,
        });
        r
    }

    fn at(path: &str, name: &str) -> PersonaSummary {
        PersonaSummary {
            path: path.into(),
            division: "Engineering".into(),
            ..persona("p", name, "")
        }
    }

    #[test]
    fn glob_matches_stars_anywhere() {
        assert!(glob("*architect*", "backend architect"));
        assert!(glob("*architect", "ux architect"));
        assert!(!glob("*architect", "architects"));
        assert!(glob("senior*dev*", "senior web developer"));
        assert!(!glob("senior*", "a senior"));
        assert!(glob("*", "anything"));
    }

    #[test]
    fn suggests_from_name_then_division_then_fallback() {
        let roles = [
            "project-manager",
            "fullstack-developer",
            "tech-lead",
            "qa-engineer",
        ]
        .map(role)
        .to_vec();
        let rules = crate::config::default_base_roles();
        let suggest = |p: &PersonaSummary| suggest_base(p, &rules, &roles);
        let architect = at(
            "engineering/engineering-backend-architect.md",
            "Backend Architect",
        );
        assert_eq!(suggest(&architect).as_deref(), Some("tech-lead"));
        let dev = at(
            "engineering/engineering-senior-developer.md",
            "Senior Developer",
        );
        assert_eq!(suggest(&dev).as_deref(), Some("fullstack-developer"));
        let tester = PersonaSummary {
            division: "Testing".into(),
            ..at("testing/testing-reality-checker.md", "Reality Checker")
        };
        assert_eq!(suggest(&tester).as_deref(), Some("qa-engineer"));
        let pm = PersonaSummary {
            division: "Project Management".into(),
            ..at("project-management/pm-senior.md", "Senior PM")
        };
        assert_eq!(suggest(&pm).as_deref(), Some("project-manager"));
        let seo = at("marketing/seo.md", "SEO Specialist");
        assert_eq!(suggest(&seo).as_deref(), Some("fullstack-developer"));
    }

    #[test]
    fn suggestion_skips_untrusted_and_unknown_roles() {
        let roles = vec![role("qa-engineer"), adopted("be", "qa-engineer", "x/y.md")];
        let rules = BTreeMap::from([
            ("*architect*".to_string(), "be".to_string()),
            ("*".to_string(), "tech-lead".to_string()),
        ]);
        let p = at("engineering/a.md", "Backend Architect");
        assert_eq!(
            suggest_base(&p, &rules, &roles).as_deref(),
            Some("qa-engineer")
        );
        assert_eq!(suggest_base(&p, &rules, &[]), None);
    }

    #[test]
    fn past_choices_beat_the_division_rule_but_not_name_rules() {
        let mut roles = ["fullstack-developer", "tech-lead", "qa-engineer"]
            .map(role)
            .to_vec();
        roles.push(adopted("fe", "qa-engineer", "engineering/fe.md"));
        roles.push(adopted("be", "qa-engineer", "engineering/be.md"));
        roles.push(adopted("ops", "tech-lead", "engineering/ops.md"));
        roles.push(adopted("seo", "tech-lead", "marketing/seo.md"));
        let rules = crate::config::default_base_roles();
        let dev = at("engineering/dev.md", "Senior Developer");
        assert_eq!(
            suggest_base(&dev, &rules, &roles).as_deref(),
            Some("qa-engineer")
        );
        let architect = at("engineering/arch.md", "Backend Architect");
        assert_eq!(
            suggest_base(&architect, &rules, &roles).as_deref(),
            Some("tech-lead")
        );
        // Re-reviewing an adopted persona keeps the base it was adopted on.
        let fe = PersonaSummary {
            adopted_role: Some("fe".into()),
            ..at("engineering/fe.md", "Frontend Architect")
        };
        assert_eq!(
            suggest_base(&fe, &rules, &roles).as_deref(),
            Some("qa-engineer")
        );
    }

    #[test]
    fn hires_are_attributed_to_the_source_persona() {
        let mut role = Role::parse("---\nid: be\ntitle: BE\n---\nx\n").expect("role");
        role.source = Some(crate::role::RoleSource {
            catalog: "agency".into(),
            repo: String::new(),
            commit: String::new(),
            path: "eng/backend.md".into(),
            blob: String::new(),
            license: None,
            edited: false,
            copy_of: None,
        });
        let hires = BTreeMap::from([("be".to_string(), 3), ("pm".to_string(), 1)]);
        let by_persona = hires_by_persona(&[role], &hires);
        assert_eq!(by_persona.get("eng/backend.md"), Some(&3));
        assert_eq!(by_persona.len(), 1);
    }
}
