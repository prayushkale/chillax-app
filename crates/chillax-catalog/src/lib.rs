//! Persona catalogs for hiring (ADR 0003).
//!
//! [`CatalogRecruiter`] keeps a shallow git checkout of a persona repo (agency-agents by
//! default) under `.chillax/state/catalogs/`, indexes it, and runs the review flow:
//! draft → scan → optional agent revisions → adopt as `.chillax/roles/<id>.md`.
//! Persona text is untrusted. It only ever lands in the persona section of a prompt
//! whose policy comes from a trusted base role.

mod draft;
mod git;
mod index;
pub mod project;
mod scan;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;
use chillax_core::config::CatalogConfig;
use chillax_core::engine::hire_choice;
use chillax_core::recruit::{featured, hires_by_persona, suggest_base};
use chillax_core::{
    Actor, CatalogStatus, Command, CompanyHandle, PersonaSummary, PromptEditor, RecruitError,
    Recruiter, Revision, Role, RoleDraft, RoleSource,
};

pub use draft::{PERSONA_GUARD, PROJECT_CONTEXT_HEADING, compose, diff};
pub use index::Index;
pub use scan::scan;

pub struct CatalogPaths {
    /// Where the checkout lives, e.g. `.chillax/state/catalogs/agency-agents`.
    pub checkout: PathBuf,
    /// Where adopted roles are written, i.e. `.chillax/roles`.
    pub roles: PathBuf,
    /// The project root, where its plan files (`plan.md`, `AGENTS.md`, …) live.
    pub project: PathBuf,
}

pub struct CatalogRecruiter {
    config: CatalogConfig,
    paths: CatalogPaths,
    company: Arc<dyn CompanyHandle>,
    editor: Arc<dyn PromptEditor>,
    index: RwLock<Option<Arc<Index>>>,
    status: Mutex<CatalogStatus>,
    drafts: Mutex<HashMap<String, RoleDraft>>,
    /// One refresh at a time.
    refreshing: tokio::sync::Mutex<()>,
}

impl CatalogRecruiter {
    pub fn new(
        config: CatalogConfig,
        paths: CatalogPaths,
        company: Arc<dyn CompanyHandle>,
        editor: Arc<dyn PromptEditor>,
    ) -> Arc<Self> {
        // A disabled catalog reports no name, so frontends can hide the catalog panel.
        let status = if config.enabled {
            CatalogStatus {
                name: config.name.clone(),
                url: config.url.clone(),
                ..CatalogStatus::default()
            }
        } else {
            CatalogStatus::default()
        };
        Arc::new(Self {
            config,
            paths,
            company,
            editor,
            index: RwLock::new(None),
            status: Mutex::new(status),
            drafts: Mutex::default(),
            refreshing: tokio::sync::Mutex::new(()),
        })
    }

    /// Index whatever checkout is already on disk (no network).
    pub async fn load_cached(&self) -> Result<(), RecruitError> {
        if !self.config.enabled || !self.paths.checkout.join(".git").is_dir() {
            return Ok(());
        }
        let index = self.reindex().await?;
        let fetched_at = std::fs::metadata(self.paths.checkout.join(".git/FETCH_HEAD"))
            .or_else(|_| std::fs::metadata(self.paths.checkout.join(".git/HEAD")))
            .and_then(|m| m.modified())
            .ok()
            .map(chrono::DateTime::<chrono::Utc>::from);
        let mut status = self.status.lock().expect("status lock poisoned");
        status.commit = Some(index.commit.clone());
        status.personas = index.personas.len();
        status.fetched_at = fetched_at;
        Ok(())
    }

    async fn reindex(&self) -> Result<Arc<Index>, RecruitError> {
        let name = self.config.name.clone();
        let dir = self.paths.checkout.clone();
        let index = tokio::task::spawn_blocking(move || index::load(&name, &dir))
            .await
            .map_err(|e| RecruitError::Io(e.to_string()))?
            .map_err(RecruitError::Refresh)?;
        let index = Arc::new(index);
        *self.index.write().expect("index lock poisoned") = Some(index.clone());
        Ok(index)
    }

    fn current_index(&self) -> Result<Arc<Index>, RecruitError> {
        if !self.config.enabled {
            return Err(RecruitError::Disabled);
        }
        self.index
            .read()
            .expect("index lock poisoned")
            .clone()
            .ok_or(RecruitError::NotReady)
    }

    fn set_refreshing(&self, on: bool) {
        self.status.lock().expect("status lock poisoned").refreshing = on;
    }

    /// Personas annotated with the stakeholder's hire counts and adopted roles.
    async fn annotated(&self) -> Result<Vec<PersonaSummary>, RecruitError> {
        let index = self.current_index()?;
        let roles = self.company.roles();
        let hires = hires_by_persona(&roles, &self.company.snapshot().await.hires_by_role);
        let rules = self.config.base_role_rules();
        Ok(index
            .personas
            .iter()
            .map(|p| {
                let mut s = p.summary.clone();
                s.hires = hires.get(&s.path).copied().unwrap_or(0);
                s.adopted_role = roles
                    .iter()
                    .find(|r| {
                        r.source
                            .as_ref()
                            .is_some_and(|src| src.catalog == s.catalog && src.path == s.path)
                    })
                    .map(|r| r.id.clone());
                s.suggested_base = suggest_base(&s, &rules, &roles);
                s
            })
            .collect())
    }

    fn with_draft<T>(
        &self,
        id: &str,
        f: impl FnOnce(&mut RoleDraft) -> Result<T, RecruitError>,
    ) -> Result<T, RecruitError> {
        let mut drafts = self.drafts.lock().expect("drafts lock poisoned");
        let draft = drafts
            .get_mut(id)
            .ok_or_else(|| RecruitError::UnknownDraft(id.to_string()))?;
        f(draft)
    }

    /// Have the editor rewrite the draft's persona and record it as a revision.
    async fn rewrite(
        &self,
        id: &str,
        instruction: &str,
        context: Option<&str>,
    ) -> Result<RoleDraft, RecruitError> {
        let (backend, model, text) = self.with_draft(id, |d| {
            Ok((d.backend.clone(), d.model.clone(), d.persona_text.clone()))
        })?;
        // The lock is not held while the agent works; that can take minutes.
        let rewrite = self
            .editor
            .rewrite(&backend, model.as_deref(), &text, instruction, context)
            .await
            .map_err(RecruitError::Editor)?;
        if rewrite.text.trim().is_empty() {
            return Err(RecruitError::Editor(
                "the editor returned an empty persona".into(),
            ));
        }
        self.with_draft(id, |d| {
            d.revisions.push(Revision {
                instruction: instruction.to_string(),
                summary: rewrite.summary,
                text: rewrite.text.trim().to_string(),
            });
            draft::refresh(d);
            Ok(d.clone())
        })
    }

    /// The project's plan files, found off the runtime (a few `stat` calls).
    async fn project_files(&self) -> Vec<String> {
        let root = self.paths.project.clone();
        tokio::task::spawn_blocking(move || project::find(&root))
            .await
            .unwrap_or_default()
    }

    /// Only trusted roles can be a base: built-ins and hand-written project roles.
    fn trusted_base(&self, id: &str) -> Result<Role, RecruitError> {
        self.company
            .roles()
            .into_iter()
            .find(|r| r.id == id && r.source.is_none())
            .ok_or_else(|| RecruitError::UnknownRole(id.to_string()))
    }

    /// A role copied from another one: no catalog index needed, and the source marks it
    /// untrusted so it can never be a base itself.
    fn build_copy(
        &self,
        draft: &RoleDraft,
        role_id: &str,
        from: &str,
    ) -> Result<Role, RecruitError> {
        let base = self.trusted_base(&draft.base_role)?;
        Ok(Role {
            id: role_id.to_string(),
            title: draft.persona.name.clone(),
            description: draft.persona.description.clone(),
            default_backend: Some(draft.backend.clone()),
            model: draft.model.clone().or_else(|| {
                base.model
                    .clone()
                    .filter(|_| base.default_backend.as_deref() == Some(draft.backend.as_str()))
            }),
            base_role: Some(base.id.clone()),
            source: Some(RoleSource {
                catalog: String::new(),
                repo: String::new(),
                commit: String::new(),
                path: String::new(),
                blob: String::new(),
                license: None,
                edited: !draft.revisions.is_empty(),
                copy_of: Some(from.to_string()),
            }),
            prompt: draft.prompt.clone(),
            ..base
        })
    }

    fn build_role(&self, draft: &RoleDraft, role_id: &str) -> Result<Role, RecruitError> {
        if let Some(from) = &draft.from_role {
            return self.build_copy(draft, role_id, from);
        }
        let index = self.current_index()?;
        let persona = index
            .personas
            .iter()
            .find(|p| p.summary.path == draft.persona.path)
            .ok_or_else(|| RecruitError::UnknownPersona(draft.persona.slug.clone()))?;
        let base = self
            .company
            .roles()
            .into_iter()
            .find(|r| r.id == draft.base_role)
            .ok_or_else(|| RecruitError::UnknownRole(draft.base_role.clone()))?;
        Ok(Role {
            id: role_id.to_string(),
            title: persona.summary.name.clone(),
            description: persona.summary.description.clone(),
            default_backend: Some(draft.backend.clone()),
            model: draft.model.clone().or_else(|| {
                base.model
                    .clone()
                    .filter(|_| base.default_backend.as_deref() == Some(draft.backend.as_str()))
            }),
            base_role: Some(base.id.clone()),
            source: Some(RoleSource {
                catalog: self.config.name.clone(),
                repo: self.config.url.clone(),
                commit: index.commit.clone(),
                path: persona.summary.path.clone(),
                blob: persona.blob.clone(),
                license: index.license.clone(),
                edited: !draft.revisions.is_empty(),
                copy_of: None,
            }),
            prompt: draft.prompt.clone(),
            // Policy comes only from the trusted base role.
            ..base
        })
    }
}

#[async_trait]
impl Recruiter for CatalogRecruiter {
    async fn status(&self) -> CatalogStatus {
        self.status.lock().expect("status lock poisoned").clone()
    }

    async fn refresh(&self) -> Result<CatalogStatus, RecruitError> {
        if !self.config.enabled {
            return Err(RecruitError::Disabled);
        }
        let _guard = self.refreshing.lock().await;
        self.set_refreshing(true);
        let synced = git::sync(&self.config.url, &self.config.git_ref, &self.paths.checkout).await;
        let result = match synced {
            Ok(_) => self.reindex().await.map(Some),
            Err(err) => Err(RecruitError::Refresh(err)),
        };
        let mut status = self.status.lock().expect("status lock poisoned");
        status.refreshing = false;
        match result {
            Ok(Some(index)) => {
                status.commit = Some(index.commit.clone());
                status.personas = index.personas.len();
                status.fetched_at = Some(chrono::Utc::now());
                status.error = None;
                Ok(status.clone())
            }
            Ok(None) => Ok(status.clone()),
            Err(err) => {
                tracing::warn!(%err, catalog = %self.config.name, "catalog refresh failed; using cache");
                status.error = Some(err.to_string());
                Err(err)
            }
        }
    }

    async fn personas(&self) -> Result<Vec<PersonaSummary>, RecruitError> {
        self.annotated().await
    }

    async fn featured(&self, limit: usize) -> Result<Vec<PersonaSummary>, RecruitError> {
        Ok(featured(
            &self.annotated().await?,
            &self.config.featured,
            limit,
        ))
    }

    async fn draft(
        &self,
        persona: &str,
        base_role: Option<&str>,
        backend: Option<String>,
        model: Option<String>,
    ) -> Result<RoleDraft, RecruitError> {
        let summary = self
            .annotated()
            .await?
            .into_iter()
            .find(|p| p.slug == persona)
            .ok_or_else(|| RecruitError::UnknownPersona(persona.to_string()))?;
        let index = self.current_index()?;
        let body = index
            .get(persona)
            .map(|p| p.body.clone())
            .ok_or_else(|| RecruitError::UnknownPersona(persona.to_string()))?;
        let base_role = base_role
            .map(str::to_string)
            .or_else(|| summary.suggested_base.clone())
            .ok_or_else(|| RecruitError::UnknownRole("(no trusted base role)".into()))?;
        let base = self.trusted_base(&base_role)?;
        let defaults = self.company.snapshot().await.hiring;
        let (backend, model, _) = hire_choice(&defaults, &base, backend, model, None);
        let mut new = RoleDraft {
            id: uuid::Uuid::new_v4().to_string(),
            role_id: summary
                .adopted_role
                .clone()
                .unwrap_or_else(|| summary.slug.clone()),
            persona: summary,
            from_role: None,
            base_role: base.id.clone(),
            backend,
            model,
            base_prompt: base.prompt.clone(),
            original: body,
            persona_text: String::new(),
            findings: Vec::new(),
            revisions: Vec::new(),
            diff: Vec::new(),
            prompt: String::new(),
            project_files: self.project_files().await,
        };
        draft::refresh(&mut new);
        self.drafts
            .lock()
            .expect("drafts lock poisoned")
            .insert(new.id.clone(), new.clone());
        Ok(new)
    }

    async fn draft_from_role(
        &self,
        from_role: &str,
        new_id: &str,
        title: &str,
        backend: Option<String>,
        model: Option<String>,
    ) -> Result<RoleDraft, RecruitError> {
        let role_id = chillax_core::engine::slug(new_id);
        if role_id.is_empty() {
            return Err(
                chillax_core::CommandError::Invalid("give the new role an id".into()).into(),
            );
        }
        let title = title.trim();
        if title.is_empty() {
            return Err(
                chillax_core::CommandError::Invalid("give the new role a title".into()).into(),
            );
        }
        let roles = self.company.roles();
        let reference = roles
            .iter()
            .find(|r| r.id == from_role)
            .ok_or_else(|| RecruitError::UnknownRole(from_role.to_string()))?;
        // A trusted reference is the base itself; an adopted or copied one lends its base.
        let base = match (&reference.source, &reference.base_role) {
            (None, _) => reference.clone(),
            (Some(_), Some(base)) => self.trusted_base(base)?,
            (Some(_), None) => return Err(RecruitError::UnknownRole(from_role.to_string())),
        };
        let original = if reference.source.is_none() {
            reference.prompt.clone()
        } else {
            reference.prompt.split_once(PERSONA_GUARD).map_or_else(
                || reference.prompt.clone(),
                |(_, rest)| rest.trim().to_string(),
            )
        };
        let defaults = self.company.snapshot().await.hiring;
        let (backend, model, _) = hire_choice(&defaults, &base, backend, model, None);
        let mut new = RoleDraft {
            id: uuid::Uuid::new_v4().to_string(),
            role_id,
            from_role: Some(from_role.to_string()),
            persona: PersonaSummary {
                catalog: String::new(),
                slug: from_role.to_string(),
                path: String::new(),
                name: title.to_string(),
                description: reference.description.clone(),
                division: String::new(),
                emoji: String::new(),
                vibe: String::new(),
                hires: 0,
                adopted_role: None,
                suggested_base: Some(base.id.clone()),
            },
            base_role: base.id.clone(),
            backend,
            model,
            base_prompt: base.prompt.clone(),
            original,
            persona_text: String::new(),
            findings: Vec::new(),
            revisions: Vec::new(),
            diff: Vec::new(),
            prompt: String::new(),
            project_files: self.project_files().await,
        };
        draft::refresh(&mut new);
        self.drafts
            .lock()
            .expect("drafts lock poisoned")
            .insert(new.id.clone(), new.clone());
        Ok(new)
    }

    async fn get_draft(&self, id: &str) -> Result<RoleDraft, RecruitError> {
        self.with_draft(id, |d| Ok(d.clone()))
    }

    async fn set_base(&self, id: &str, base_role: &str) -> Result<RoleDraft, RecruitError> {
        let base = self.trusted_base(base_role)?;
        self.with_draft(id, |d| {
            d.base_role = base.id.clone();
            d.base_prompt = base.prompt.clone();
            draft::refresh(d);
            Ok(d.clone())
        })
    }

    async fn revise(&self, id: &str, instruction: &str) -> Result<RoleDraft, RecruitError> {
        let instruction = instruction.trim();
        if instruction.is_empty() {
            return Err(RecruitError::Editor("say what to change".into()));
        }
        self.rewrite(id, instruction, None).await
    }

    async fn tailor(&self, id: &str) -> Result<RoleDraft, RecruitError> {
        let files = self.with_draft(id, |d| Ok(d.project_files.clone()))?;
        let root = self.paths.project.clone();
        let excerpts = tokio::task::spawn_blocking(move || project::excerpts(&root, &files))
            .await
            .map_err(|e| RecruitError::Io(e.to_string()))?
            .ok_or_else(|| {
                RecruitError::Editor(format!(
                    "no project files to tailor to (looked for {})",
                    project::PROJECT_FILES.join(", ")
                ))
            })?;
        self.rewrite(id, project::TAILOR_INSTRUCTION, Some(&excerpts))
            .await
    }

    async fn undo(&self, id: &str) -> Result<RoleDraft, RecruitError> {
        self.with_draft(id, |d| {
            d.revisions.pop().ok_or(RecruitError::NothingToUndo)?;
            draft::refresh(d);
            Ok(d.clone())
        })
    }

    async fn adopt(
        &self,
        id: &str,
        role_id: Option<String>,
        overwrite: bool,
    ) -> Result<Role, RecruitError> {
        let draft = self.get_draft(id).await?;
        let role_id = chillax_core::engine::slug(role_id.as_deref().unwrap_or(&draft.role_id));
        let role = self.build_role(&draft, &role_id)?;
        let markdown = role
            .to_markdown()
            .map_err(|e| RecruitError::Io(e.to_string()))?;
        // Validate and record first; the file is written only for an accepted role.
        self.company
            .send(
                Actor::Stakeholder,
                Command::AdoptRole {
                    markdown: markdown.clone(),
                    revisions: draft
                        .revisions
                        .iter()
                        .map(|r| r.instruction.clone())
                        .collect(),
                    overwrite,
                },
            )
            .await?;
        let path = self.paths.roles.join(format!("{role_id}.md"));
        let write = async {
            tokio::fs::create_dir_all(&self.paths.roles).await?;
            tokio::fs::write(&path, markdown).await
        };
        write
            .await
            .map_err(|e| RecruitError::Io(format!("writing {}: {e}", path.display())))?;
        self.drafts.lock().expect("drafts lock poisoned").remove(id);
        Ok(role)
    }

    async fn discard(&self, id: &str) {
        self.drafts.lock().expect("drafts lock poisoned").remove(id);
    }
}

#[cfg(test)]
mod tests;
