//! The hiring wizard: pick a role or catalog persona → pick the
//! coding agent and model (starting on the hiring defaults) → for a persona, review it
//! (findings, agent revisions, diff) on its suggested trusted base role, which the
//! stakeholder can change → confirm and hire.
//!
//! Key handlers stay synchronous: they queue [`RecruitRequest`]s, which the run loop
//! executes on background tasks, and results come back as [`RecruitReply`]s.

use chillax_core::recruit::search;
use chillax_core::{
    BackendInfo, CatalogStatus, Command, DiffOp, ModelOption, PersonaSummary, Role, RoleDraft,
    Severity,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Widget, Wrap};

use crate::App;
use crate::picker::{AgentPicker, Picked};
use crate::scroll::Sticky;
use crate::view::{centered, wrap};

/// How many search results the picker shows.
const SEARCH_LIMIT: usize = 50;

/// Work for the run loop to do against the recruiter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecruitRequest {
    /// Catalog status, featured personas, and the full list for local search.
    Load,
    Refresh,
    /// `base_role: None` uses the persona's suggested base; `backend`/`model: None`
    /// the hiring defaults.
    Draft {
        persona: String,
        base_role: Option<String>,
        backend: Option<String>,
        model: Option<String>,
    },
    /// A new role copied from `from_role`; the catalog may be off.
    DraftFromRole {
        from_role: String,
        role_id: String,
        title: String,
        backend: Option<String>,
        model: Option<String>,
    },
    SetBase {
        draft: String,
        base_role: String,
    },
    Revise {
        draft: String,
        instruction: String,
    },
    /// Tailor the persona to the project's plan files (an editor revision).
    Tailor {
        draft: String,
    },
    Undo {
        draft: String,
    },
    /// Stop waiting for a revision and resync the draft.
    Cancel {
        draft: String,
    },
    Adopt {
        draft: String,
        role_id: String,
        overwrite: bool,
        name: String,
    },
    Discard {
        draft: String,
    },
    /// The agent backends to pick from.
    Backends,
    /// A backend's models (starts the agent once; no prompt).
    Models {
        backend: String,
    },
    /// Add an installed preset to `chillax.toml`, then list its models.
    Enable {
        backend: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecruitReply {
    Loaded {
        status: Option<CatalogStatus>,
        featured: Vec<PersonaSummary>,
        all: Vec<PersonaSummary>,
        error: Option<String>,
    },
    Refreshed(Result<CatalogStatus, String>),
    Draft(Result<RoleDraft, String>),
    Adopted {
        result: Result<Role, String>,
        name: String,
    },
    Backends(Result<Vec<BackendInfo>, String>),
    Models {
        backend: String,
        result: Result<Vec<ModelOption>, String>,
        /// The backend's effort levels; empty when it has none or listing them failed.
        efforts: Vec<ModelOption>,
    },
    Enabled {
        backend: String,
        result: Result<BackendInfo, String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HireFlow {
    pub step: HireStep,
    pub query: String,
    pub selected: usize,
    pub status: Option<CatalogStatus>,
    pub featured: Vec<PersonaSummary>,
    pub all: Vec<PersonaSummary>,
    /// Why the catalog isn't available (disabled, not fetched yet, …).
    pub note: Option<String>,
    /// What we're waiting for; most keys are ignored meanwhile.
    pub busy: Option<String>,
    pub error: Option<String>,
    /// First row drawn in the pick list and in the base-role list.
    pick_top: Sticky,
    base_top: Sticky,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum HireStep {
    #[default]
    Pick,
    /// Choosing the existing role a new one is copied from.
    Reference {
        query: String,
        selected: usize,
    },
    /// Typing the new role's id and title (`field`: 0 = id, 1 = title).
    NewRole {
        from: String,
        id: String,
        title: String,
        field: usize,
    },
    /// Hire into an existing role.
    Name {
        role: String,
        name: String,
    },
    /// Choosing the coding agent and model.
    Agent {
        target: AgentTarget,
        picker: Box<AgentPicker>,
    },
    /// Waiting for the first draft of a persona.
    Preparing {
        persona: PersonaSummary,
    },
    Review(Box<Review>),
    /// Choosing another base role for the draft under review.
    Base {
        review: Box<Review>,
        selected: usize,
    },
    Confirm(Box<Confirm>),
}

/// Who the agent is being picked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentTarget {
    /// A hire into an existing role.
    Role { role: String, name: String },
    /// A persona about to be reviewed; revisions run on the chosen agent.
    Persona(PersonaSummary),
    /// A new role copied from an existing one, about to be reviewed.
    Copy {
        from: String,
        id: String,
        title: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReviewView {
    /// The persona text with flagged lines marked.
    #[default]
    Persona,
    /// Upstream text → current text.
    Diff,
    /// The whole prompt the employee will get.
    Prompt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Review {
    pub draft: RoleDraft,
    pub view: ReviewView,
    pub scroll: u16,
    /// `Some` while typing a suggestion for the editor agent.
    pub input: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirm {
    pub draft: RoleDraft,
    pub role_id: String,
    pub name: String,
    /// 0 = name, 1 = role id.
    pub field: usize,
}

/// A row in the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Heading(String),
    Role {
        id: String,
        title: String,
        detail: String,
    },
    Persona(PersonaSummary),
    /// "+ New role based on…": the first row while the search is empty.
    NewRole,
}

impl Entry {
    fn selectable(&self) -> bool {
        !matches!(self, Entry::Heading(_))
    }
}

impl HireFlow {
    /// Picker rows: your roles, then featured personas (or search results).
    pub fn entries(&self, roles: &[Role]) -> Vec<Entry> {
        let query = self.query.trim().to_lowercase();
        let mut rows = Vec::new();
        if query.is_empty() {
            rows.push(Entry::NewRole);
        }
        let matching: Vec<&Role> = roles
            .iter()
            .filter(|r| {
                query.is_empty()
                    || query
                        .split_whitespace()
                        .all(|w| r.title.to_lowercase().contains(w) || r.id.contains(w))
            })
            .collect();
        if !matching.is_empty() {
            rows.push(Entry::Heading("Your roles".into()));
            rows.extend(matching.into_iter().map(|r| Entry::Role {
                id: r.id.clone(),
                title: r.title.clone(),
                detail: r.description.clone(),
            }));
        }
        let personas = if query.is_empty() {
            self.featured.clone()
        } else {
            search(&self.all, &query, SEARCH_LIMIT)
        };
        if !personas.is_empty() {
            let heading = match (query.is_empty(), &self.status) {
                (true, Some(s)) => format!("Featured from {}", s.name),
                (true, None) => "Featured".into(),
                (false, _) => format!("{} matching personas", personas.len()),
            };
            rows.push(Entry::Heading(heading));
            rows.extend(personas.into_iter().map(Entry::Persona));
        }
        rows
    }

    fn move_selection(&mut self, roles: &[Role], down: bool) {
        let rows = self.entries(roles);
        let selectable: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, e)| e.selectable())
            .map(|(i, _)| i)
            .collect();
        let pos = selectable
            .iter()
            .position(|i| *i >= self.selected)
            .unwrap_or(0);
        let next = if down {
            (pos + 1).min(selectable.len().saturating_sub(1))
        } else {
            pos.saturating_sub(1)
        };
        self.selected = selectable.get(next).copied().unwrap_or(0);
    }

    /// Keep the selection on a selectable row after the list changes.
    fn snap_selection(&mut self, roles: &[Role]) {
        let rows = self.entries(roles);
        self.selected = rows
            .iter()
            .enumerate()
            .skip(self.selected.min(rows.len()))
            .chain(rows.iter().enumerate())
            .find(|(_, e)| e.selectable())
            .map_or(0, |(i, _)| i);
    }
}

/// Roles a new one can be copied from (every role), narrowed by the typed words.
fn reference_roles<'a>(roles: &'a [Role], query: &str) -> Vec<&'a Role> {
    let query = query.trim().to_lowercase();
    roles
        .iter()
        .filter(|r| {
            query
                .split_whitespace()
                .all(|w| r.title.to_lowercase().contains(w) || r.id.contains(w))
        })
        .collect()
}

/// Roles that can serve as a base: the trusted ones, not other adopted personas.
fn base_roles(roles: &[Role]) -> Vec<&Role> {
    roles.iter().filter(|r| r.source.is_none()).collect()
}

impl App {
    /// Open the wizard and ask the recruiter for the catalog.
    pub(crate) fn open_hire(&mut self) {
        let mut flow = HireFlow {
            busy: Some("Loading catalog…".into()),
            ..HireFlow::default()
        };
        // Without custom (adopted) roles the way to make one is the first thing offered;
        // otherwise every company starts with a PM, so that's the default pick.
        flow.selected = if self.roles.iter().any(|r| r.source.is_some()) {
            flow.entries(&self.roles)
                .iter()
                .position(|e| matches!(e, Entry::Role { id, .. } if id == "project-manager"))
                .unwrap_or(0)
        } else {
            0
        };
        flow.snap_selection(&self.roles);
        self.mode = crate::Mode::Hire(Box::new(flow));
        self.recruit_requests.push(RecruitRequest::Load);
    }

    /// Start picking the agent, on the stakeholder's hiring defaults.
    fn agent_step(&mut self, target: AgentTarget) -> HireStep {
        self.recruit_requests.push(RecruitRequest::Backends);
        let mut picker = AgentPicker::new(self.state.hiring.clone());
        // Only a direct hire can carry an effort: persona drafts have no field for it.
        picker.skip_effort = !matches!(target, AgentTarget::Role { .. });
        HireStep::Agent {
            target,
            picker: Box::new(picker),
        }
    }

    pub(crate) fn hire_key(&mut self, key: KeyEvent, mut flow: Box<HireFlow>) -> Vec<Command> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let step = std::mem::take(&mut flow.step);
        flow.step = match step {
            HireStep::Pick => {
                flow.error = None;
                match key.code {
                    KeyCode::Esc => return Vec::new(),
                    KeyCode::Char('r') if ctrl => {
                        flow.busy = Some("Refreshing catalog…".into());
                        self.recruit_requests.push(RecruitRequest::Refresh);
                    }
                    KeyCode::Up => flow.move_selection(&self.roles, false),
                    KeyCode::Down => flow.move_selection(&self.roles, true),
                    KeyCode::Backspace => {
                        flow.query.pop();
                        flow.selected = 0;
                        flow.snap_selection(&self.roles);
                    }
                    KeyCode::Char(c) if !ctrl => {
                        flow.query.push(c);
                        flow.selected = 0;
                        flow.snap_selection(&self.roles);
                    }
                    KeyCode::Enter => match flow.entries(&self.roles).get(flow.selected).cloned() {
                        Some(Entry::NewRole) => {
                            flow.step = HireStep::Reference {
                                query: String::new(),
                                selected: 0,
                            };
                            self.mode = crate::Mode::Hire(flow);
                            return Vec::new();
                        }
                        Some(Entry::Role { id, .. }) => {
                            // Suggest the next free famous name; the stakeholder may edit it.
                            let title = self
                                .roles
                                .iter()
                                .find(|r| r.id == id)
                                .map_or(id.as_str(), |r| r.title.as_str());
                            let name = chillax_core::names::default_name(&self.state, title);
                            flow.step = HireStep::Name { role: id, name };
                            self.mode = crate::Mode::Hire(flow);
                            return Vec::new();
                        }
                        Some(Entry::Persona(persona)) => {
                            flow.step = self.agent_step(AgentTarget::Persona(persona));
                            self.mode = crate::Mode::Hire(flow);
                            return Vec::new();
                        }
                        _ => {}
                    },
                    _ => {}
                }
                HireStep::Pick
            }
            HireStep::Reference {
                mut query,
                mut selected,
            } => {
                let count = reference_roles(&self.roles, &query).len();
                match key.code {
                    KeyCode::Esc => HireStep::Pick,
                    KeyCode::Up => {
                        selected = selected.saturating_sub(1);
                        HireStep::Reference { query, selected }
                    }
                    KeyCode::Down => {
                        selected = (selected + 1).min(count.saturating_sub(1));
                        HireStep::Reference { query, selected }
                    }
                    KeyCode::Backspace => {
                        query.pop();
                        HireStep::Reference { query, selected: 0 }
                    }
                    KeyCode::Char(c) if !ctrl => {
                        query.push(c);
                        HireStep::Reference { query, selected: 0 }
                    }
                    KeyCode::Enter => match reference_roles(&self.roles, &query).get(selected) {
                        Some(role) => HireStep::NewRole {
                            from: role.id.clone(),
                            id: String::new(),
                            title: String::new(),
                            field: 0,
                        },
                        None => HireStep::Reference { query, selected },
                    },
                    _ => HireStep::Reference { query, selected },
                }
            }
            HireStep::NewRole {
                from,
                mut id,
                mut title,
                mut field,
            } => {
                let text = if field == 0 { &mut id } else { &mut title };
                match key.code {
                    KeyCode::Esc => HireStep::Reference {
                        query: String::new(),
                        selected: 0,
                    },
                    KeyCode::Tab | KeyCode::Up | KeyCode::Down => {
                        field = 1 - field;
                        HireStep::NewRole {
                            from,
                            id,
                            title,
                            field,
                        }
                    }
                    KeyCode::Backspace => {
                        text.pop();
                        HireStep::NewRole {
                            from,
                            id,
                            title,
                            field,
                        }
                    }
                    KeyCode::Char(c) if !ctrl => {
                        text.push(c);
                        HireStep::NewRole {
                            from,
                            id,
                            title,
                            field,
                        }
                    }
                    KeyCode::Enter if id.trim().is_empty() => {
                        flow.error = Some("The role needs an id".into());
                        HireStep::NewRole {
                            from,
                            id,
                            title,
                            field: 0,
                        }
                    }
                    KeyCode::Enter if title.trim().is_empty() => {
                        flow.error = Some("The role needs a title".into());
                        HireStep::NewRole {
                            from,
                            id,
                            title,
                            field: 1,
                        }
                    }
                    KeyCode::Enter => {
                        flow.error = None;
                        self.agent_step(AgentTarget::Copy { from, id, title })
                    }
                    _ => HireStep::NewRole {
                        from,
                        id,
                        title,
                        field,
                    },
                }
            }
            HireStep::Name { role, mut name } => match key.code {
                KeyCode::Esc => HireStep::Pick,
                KeyCode::Backspace => {
                    name.pop();
                    HireStep::Name { role, name }
                }
                KeyCode::Char(c) => {
                    name.push(c);
                    HireStep::Name { role, name }
                }
                KeyCode::Enter => self.agent_step(AgentTarget::Role { role, name }),
                _ => HireStep::Name { role, name },
            },
            HireStep::Agent { target, mut picker } => {
                match picker.key(key, &mut self.recruit_requests) {
                    Picked::Pending => HireStep::Agent { target, picker },
                    Picked::Cancelled => match target {
                        AgentTarget::Role { role, name } => HireStep::Name { role, name },
                        AgentTarget::Persona(_) => HireStep::Pick,
                        AgentTarget::Copy { from, id, title } => HireStep::NewRole {
                            from,
                            id,
                            title,
                            field: 0,
                        },
                    },
                    Picked::Done {
                        backend,
                        model,
                        effort,
                    } => match target {
                        AgentTarget::Role { role, name } => {
                            return vec![Command::HireEmployee {
                                role,
                                // Blank: the engine picks the name.
                                name: Some(name.trim().to_string()).filter(|n| !n.is_empty()),
                                backend,
                                model,
                                effort,
                                keep_defaults: false,
                            }];
                        }
                        AgentTarget::Copy { from, id, title } => {
                            flow.busy = Some("Preparing the review…".into());
                            self.recruit_requests.push(RecruitRequest::DraftFromRole {
                                from_role: from,
                                role_id: id.trim().to_string(),
                                title: title.trim().to_string(),
                                backend,
                                model,
                            });
                            HireStep::Preparing {
                                persona: PersonaSummary {
                                    catalog: String::new(),
                                    slug: String::new(),
                                    path: String::new(),
                                    name: title.trim().to_string(),
                                    description: String::new(),
                                    division: String::new(),
                                    emoji: String::new(),
                                    vibe: String::new(),
                                    hires: 0,
                                    adopted_role: None,
                                    suggested_base: None,
                                },
                            }
                        }
                        AgentTarget::Persona(persona) => {
                            flow.busy = Some("Preparing the review…".into());
                            self.recruit_requests.push(RecruitRequest::Draft {
                                persona: persona.slug.clone(),
                                base_role: None,
                                backend,
                                model,
                            });
                            HireStep::Preparing { persona }
                        }
                    },
                }
            }
            HireStep::Preparing { persona } => {
                if key.code == KeyCode::Esc {
                    // A draft that arrives later is discarded in `on_recruit`.
                    flow.busy = None;
                    flow.error = None;
                    HireStep::Pick
                } else {
                    HireStep::Preparing { persona }
                }
            }
            HireStep::Base {
                review,
                mut selected,
            } => {
                let bases = base_roles(&self.roles);
                match key.code {
                    KeyCode::Esc => {
                        flow.busy = None;
                        flow.error = None;
                        HireStep::Review(review)
                    }
                    _ if flow.busy.is_some() => HireStep::Base { review, selected },
                    KeyCode::Up => {
                        selected = selected.saturating_sub(1);
                        HireStep::Base { review, selected }
                    }
                    KeyCode::Down => {
                        selected = (selected + 1).min(bases.len().saturating_sub(1));
                        HireStep::Base { review, selected }
                    }
                    KeyCode::Enter => match bases.get(selected) {
                        Some(base) if base.id == review.draft.base_role => {
                            flow.error = None;
                            HireStep::Review(review)
                        }
                        Some(base) => {
                            flow.busy = Some("Changing the base role…".into());
                            flow.error = None;
                            self.recruit_requests.push(RecruitRequest::SetBase {
                                draft: review.draft.id.clone(),
                                base_role: base.id.clone(),
                            });
                            HireStep::Base { review, selected }
                        }
                        None => HireStep::Base { review, selected },
                    },
                    _ => HireStep::Base { review, selected },
                }
            }
            HireStep::Review(review) => self.review_key(key, &mut flow, review),
            HireStep::Confirm(confirm) => self.confirm_key(key, &mut flow, confirm),
        };
        self.mode = crate::Mode::Hire(flow);
        Vec::new()
    }

    fn review_key(
        &mut self,
        key: KeyEvent,
        flow: &mut HireFlow,
        mut review: Box<Review>,
    ) -> HireStep {
        let draft_id = review.draft.id.clone();
        if flow.busy.is_some() {
            if key.code == KeyCode::Esc {
                flow.busy = Some("Cancelling…".into());
                self.recruit_requests
                    .push(RecruitRequest::Cancel { draft: draft_id });
            }
            return HireStep::Review(review);
        }
        if let Some(mut input) = review.input.take() {
            match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter if !input.trim().is_empty() => {
                    flow.busy = Some(format!(
                        "Revising with your {} agent (no tools, no repo access)… Esc cancels",
                        review.draft.backend
                    ));
                    flow.error = None;
                    self.recruit_requests.push(RecruitRequest::Revise {
                        draft: draft_id,
                        instruction: input,
                    });
                }
                KeyCode::Backspace => {
                    input.pop();
                    review.input = Some(input);
                }
                KeyCode::Char(c) => {
                    input.push(c);
                    review.input = Some(input);
                }
                _ => review.input = Some(input),
            }
            return HireStep::Review(review);
        }
        flow.error = None;
        match key.code {
            KeyCode::Esc => {
                self.recruit_requests
                    .push(RecruitRequest::Discard { draft: draft_id });
                return HireStep::Pick;
            }
            KeyCode::Char('e') => review.input = Some(String::new()),
            KeyCode::Char('t') => {
                if review.draft.project_files.is_empty() {
                    flow.error =
                        Some("No plan files (plan.md, AGENTS.md, …) in this project".into());
                } else {
                    flow.busy = Some(format!(
                        "Tailoring to the project with your {} agent (no tools)… Esc cancels",
                        review.draft.backend
                    ));
                    self.recruit_requests
                        .push(RecruitRequest::Tailor { draft: draft_id });
                }
            }
            KeyCode::Char('b') => {
                let selected = base_roles(&self.roles)
                    .iter()
                    .position(|r| r.id == review.draft.base_role)
                    .unwrap_or(0);
                return HireStep::Base { review, selected };
            }
            KeyCode::Char('u') => {
                if review.draft.revisions.is_empty() {
                    flow.error = Some("Nothing to undo".into());
                } else {
                    flow.busy = Some("Undoing…".into());
                    self.recruit_requests
                        .push(RecruitRequest::Undo { draft: draft_id });
                }
            }
            KeyCode::Char('v') | KeyCode::Tab => {
                review.view = match review.view {
                    ReviewView::Persona => ReviewView::Diff,
                    ReviewView::Diff => ReviewView::Prompt,
                    ReviewView::Prompt => ReviewView::Persona,
                };
                review.scroll = 0;
            }
            KeyCode::Up | KeyCode::Char('k') => review.scroll = review.scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => review.scroll = review.scroll.saturating_add(1),
            KeyCode::PageUp => review.scroll = review.scroll.saturating_sub(10),
            KeyCode::PageDown => review.scroll = review.scroll.saturating_add(10),
            KeyCode::Char('a') => {
                return HireStep::Confirm(Box::new(Confirm {
                    role_id: review.draft.role_id.clone(),
                    name: String::new(),
                    field: 0,
                    draft: review.draft,
                }));
            }
            _ => {}
        }
        HireStep::Review(review)
    }

    fn confirm_key(
        &mut self,
        key: KeyEvent,
        flow: &mut HireFlow,
        mut confirm: Box<Confirm>,
    ) -> HireStep {
        if flow.busy.is_some() {
            return HireStep::Confirm(confirm);
        }
        let field = if confirm.field == 0 {
            &mut confirm.name
        } else {
            &mut confirm.role_id
        };
        match key.code {
            KeyCode::Esc => {
                flow.error = None;
                return HireStep::Review(Box::new(Review {
                    draft: confirm.draft,
                    view: ReviewView::Persona,
                    scroll: 0,
                    input: None,
                }));
            }
            KeyCode::Tab | KeyCode::Up | KeyCode::Down => confirm.field = 1 - confirm.field,
            KeyCode::Backspace => {
                field.pop();
            }
            KeyCode::Char(c) => field.push(c),
            KeyCode::Enter => {
                if confirm.name.trim().is_empty() {
                    flow.error = Some("Give your new hire a name".into());
                } else if confirm.role_id.trim().is_empty() {
                    flow.error = Some("The role needs an id".into());
                } else {
                    flow.error = None;
                    flow.busy = Some("Adopting the role…".into());
                    self.recruit_requests.push(RecruitRequest::Adopt {
                        draft: confirm.draft.id.clone(),
                        role_id: confirm.role_id.trim().to_string(),
                        overwrite: confirm.draft.persona.adopted_role.as_deref()
                            == Some(confirm.role_id.trim()),
                        name: confirm.name.trim().to_string(),
                    });
                }
            }
            _ => {}
        }
        HireStep::Confirm(confirm)
    }

    /// Apply a recruiter result. May produce commands (hiring after adoption).
    pub fn on_recruit(&mut self, reply: RecruitReply) -> Vec<Command> {
        let picker = match &mut self.mode {
            crate::Mode::Hire(flow) => match &mut flow.step {
                HireStep::Agent { picker, .. } => Some(picker),
                _ => None,
            },
            crate::Mode::Settings(picker) | crate::Mode::EmployeeAgent { picker, .. } => {
                Some(picker)
            }
            _ => None,
        };
        if let Some(picker) = picker
            && picker.on_reply(&reply, &mut self.recruit_requests)
        {
            return Vec::new();
        }
        let crate::Mode::Hire(flow) = &mut self.mode else {
            // The wizard was closed; nothing is waiting for this.
            return Vec::new();
        };
        match reply {
            RecruitReply::Loaded {
                status,
                featured,
                all,
                error,
            } => {
                flow.busy = None;
                flow.status = status;
                flow.featured = featured;
                flow.all = all;
                flow.note = error;
                flow.snap_selection(&self.roles);
            }
            RecruitReply::Refreshed(result) => {
                flow.busy = None;
                match result {
                    Ok(_) => self.recruit_requests.push(RecruitRequest::Load),
                    Err(err) => flow.error = Some(err),
                }
            }
            RecruitReply::Draft(result) => {
                flow.busy = None;
                match (result, std::mem::take(&mut flow.step)) {
                    (Ok(draft), HireStep::Review(mut review)) => {
                        review.draft = draft;
                        flow.step = HireStep::Review(review);
                    }
                    (Ok(draft), HireStep::Base { mut review, .. }) => {
                        review.draft = draft;
                        flow.step = HireStep::Review(review);
                    }
                    (Ok(draft), HireStep::Preparing { .. }) => {
                        flow.step = HireStep::Review(Box::new(Review {
                            draft,
                            view: ReviewView::Persona,
                            scroll: 0,
                            input: None,
                        }));
                    }
                    (Ok(draft), step) => {
                        // The stakeholder moved on; drop the stray draft.
                        self.recruit_requests
                            .push(RecruitRequest::Discard { draft: draft.id });
                        flow.step = step;
                    }
                    (Err(err), step) => {
                        flow.error = Some(err);
                        flow.step = step;
                    }
                }
            }
            RecruitReply::Adopted { result, name } => {
                flow.busy = None;
                match result {
                    Ok(role) => {
                        // Hire on the agent the draft was reviewed with.
                        let (backend, model) = match &flow.step {
                            HireStep::Confirm(c) => {
                                (Some(c.draft.backend.clone()), c.draft.model.clone())
                            }
                            _ => (None, None),
                        };
                        self.mode = crate::Mode::Normal;
                        return vec![Command::HireEmployee {
                            role: role.id,
                            name: Some(name),
                            backend,
                            model,
                            effort: None,
                            keep_defaults: false,
                        }];
                    }
                    Err(err) => flow.error = Some(err),
                }
            }
            // Replies for a picker that has closed since.
            RecruitReply::Backends(_)
            | RecruitReply::Models { .. }
            | RecruitReply::Enabled { .. } => {}
        }
        Vec::new()
    }

    /// Requests queued by key handlers, for the run loop to execute.
    pub fn take_recruit_requests(&mut self) -> Vec<RecruitRequest> {
        std::mem::take(&mut self.recruit_requests)
    }

    pub(crate) fn render_hire(&self, area: Rect, buf: &mut Buffer, flow: &HireFlow) {
        let wide = matches!(flow.step, HireStep::Review(_) | HireStep::Base { .. });
        let popup = if wide {
            centered(
                area,
                area.width.saturating_sub(4).min(110),
                area.height.saturating_sub(2),
            )
        } else {
            centered(
                area,
                area.width.saturating_sub(4).min(84),
                area.height.saturating_sub(4).min(26),
            )
        };
        Clear.render(popup, buf);
        let title = match &flow.step {
            HireStep::Pick => " Hire ".to_string(),
            HireStep::Reference { .. } => " New role · based on ".to_string(),
            HireStep::NewRole { .. } => " New role · id and title ".to_string(),
            HireStep::Name { .. } => " Hire · name ".to_string(),
            HireStep::Agent { target, .. } => match target {
                AgentTarget::Copy { title, .. } => format!(" New role {} · agent ", title.trim()),
                AgentTarget::Role { name, .. } => format!(" Hire {} · agent ", name.trim()),
                AgentTarget::Persona(p) => format!(" Hire {} · agent ", p.name),
            },
            HireStep::Preparing { persona } => format!(" Hire {} ", persona.name),
            HireStep::Base { review, .. } => {
                format!(" Review {} · base role ", review.draft.persona.name)
            }
            HireStep::Review(r) => format!(" Review {} ", r.draft.persona.name),
            HireStep::Confirm(c) => format!(" Hire {} ", c.draft.persona.name),
        };
        let block = Block::bordered()
            .title(title)
            .border_set(border::ROUNDED)
            .border_style(Style::new().cyan());
        let inner = block.inner(popup);
        block.render(popup, buf);
        let [body, notice] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);
        match &flow.step {
            HireStep::Pick => self.render_pick(body, buf, flow),
            HireStep::Reference { query, selected } => {
                let [search, list] =
                    Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(body);
                Line::from(vec!["Based on: ".bold(), Span::raw(format!("{query}▏"))])
                    .render(search, buf);
                let rows = reference_roles(&self.roles, query);
                let height = list.height as usize;
                let start = selected.saturating_sub(height.saturating_sub(1));
                let lines: Vec<Line> = rows
                    .iter()
                    .enumerate()
                    .skip(start)
                    .take(height)
                    .map(|(i, r)| {
                        let marker = if i == *selected { "▶ " } else { "  " };
                        let line = Line::from(vec![
                            Span::raw(format!("{marker}{}", r.title)),
                            Span::raw(format!("  {}", r.description)).dark_gray(),
                        ]);
                        let line = truncate_line(line, list.width as usize);
                        if i == *selected { line.bold() } else { line }
                    })
                    .collect();
                Paragraph::new(lines).render(list, buf);
            }
            HireStep::NewRole {
                from,
                id,
                title,
                field,
            } => {
                let row = |label: &str, value: &str, active: bool| {
                    let value = if active {
                        format!("{value}▏")
                    } else {
                        value.to_string()
                    };
                    let line = Line::from(vec![
                        Span::raw(format!("{label:<9}")).bold(),
                        Span::raw(value),
                    ]);
                    if active { line.cyan() } else { line }
                };
                let from_title = self
                    .roles
                    .iter()
                    .find(|r| r.id == *from)
                    .map_or(from.as_str(), |r| r.title.as_str());
                Paragraph::new(vec![
                    Line::from(vec!["Based on: ".bold(), Span::raw(from_title.to_string())]),
                    Line::from(""),
                    row("Role id:", id, *field == 0),
                    row("Title:", title, *field == 1),
                ])
                .render(body, buf);
            }
            HireStep::Name { role, name } => {
                let title = self
                    .roles
                    .iter()
                    .find(|r| r.id == *role)
                    .map_or(role.as_str(), |r| r.title.as_str());
                Paragraph::new(vec![
                    Line::from(vec!["Role: ".bold(), Span::raw(title.to_string())]),
                    Line::from(""),
                    Line::from(vec!["Name: ".bold(), Span::raw(format!("{name}▏"))]),
                    Line::from(""),
                    Line::from("Leave it blank to let Chillax pick.".dim()),
                ])
                .render(body, buf);
            }
            HireStep::Agent { picker, .. } => picker.render(body, buf, self.tick),
            HireStep::Preparing { persona } => {
                Paragraph::new(format!(
                    "Reviewing {} on its suggested base role. You can change the base on the \
                     next screen.",
                    persona.name
                ))
                .wrap(Wrap { trim: true })
                .render(body, buf);
            }
            HireStep::Base { review, selected } => {
                self.render_base(body, buf, flow, &review.draft, *selected)
            }
            HireStep::Review(review) => render_review(body, buf, review, &self.roles),
            HireStep::Confirm(confirm) => render_confirm(body, buf, confirm),
        }
        let line = match (&flow.error, &flow.busy) {
            (Some(err), _) => Line::from(format!("⚠ {err}")).red(),
            (None, Some(busy)) => {
                Line::from(format!("{} {busy}", crate::spinner::frame(self.tick))).yellow()
            }
            _ => Line::from(""),
        };
        line.render(notice, buf);
    }

    fn render_pick(&self, area: Rect, buf: &mut Buffer, flow: &HireFlow) {
        let [search, list, status] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(area);
        Line::from(vec![
            "Search: ".bold(),
            Span::raw(format!("{}▏", flow.query)),
        ])
        .render(search, buf);

        let rows = flow.entries(&self.roles);
        // Keep the selection in view without moving the window while it stays visible.
        let height = list.height as usize;
        let start = flow.pick_top.reveal(flow.selected, rows.len(), height);
        let width = list.width as usize;
        let lines: Vec<Line> = rows
            .iter()
            .enumerate()
            .skip(start)
            .take(height)
            .map(|(i, row)| {
                let selected = i == flow.selected;
                let marker = if selected { "▶ " } else { "  " };
                let line = match row {
                    Entry::Heading(h) => return Line::from(h.clone()).dark_gray().bold(),
                    Entry::Role { title, detail, .. } => Line::from(vec![
                        Span::raw(format!("{marker}{title}")),
                        Span::raw(format!("  {detail}")).dark_gray(),
                    ]),
                    Entry::NewRole => {
                        let line = Line::from(format!("{marker}+ New role based on…")).green();
                        return truncate_line(line, width);
                    }
                    Entry::Persona(p) => {
                        let mut spans = vec![Span::raw(format!(
                            "{marker}{}{}",
                            if p.emoji.is_empty() {
                                String::new()
                            } else {
                                format!("{} ", p.emoji)
                            },
                            p.name
                        ))];
                        spans.push(Span::raw(format!(" · {}", p.division)).cyan());
                        if let Some(role) = &p.adopted_role {
                            spans.push(Span::raw(format!(" · adopted as {role}")).green());
                        }
                        if p.hires > 0 {
                            spans.push(Span::raw(format!(" · hired {}×", p.hires)).green());
                        }
                        spans.push(Span::raw(format!("  {}", p.description)).dark_gray());
                        Line::from(spans)
                    }
                };
                let line = truncate_line(line, width);
                if selected { line.bold() } else { line }
            })
            .collect();
        Paragraph::new(lines).render(list, buf);

        let status_line = match (&flow.note, &flow.status) {
            (Some(note), _) => Line::from(note.clone()).yellow(),
            (None, Some(s)) => {
                let commit = s.commit.as_deref().map_or("—", |c| &c[..c.len().min(7)]);
                let when = s
                    .fetched_at
                    .map(|t| format!(" · updated {}", t.format("%Y-%m-%d %H:%M")))
                    .unwrap_or_default();
                let mut text = format!("{} @ {commit} · {} personas{when}", s.name, s.personas);
                if let Some(err) = &s.error {
                    text.push_str(&format!(" · offline, using cache ({err})"));
                }
                Line::from(text).dark_gray()
            }
            _ => Line::from(""),
        };
        truncate_line(status_line, status.width as usize).render(status, buf);
    }

    fn render_base(
        &self,
        area: Rect,
        buf: &mut Buffer,
        flow: &HireFlow,
        draft: &RoleDraft,
        selected: usize,
    ) {
        let mut lines = vec![
            Line::from(format!(
                "{} brings expertise text only. The base role decides its tools,",
                draft.persona.name
            )),
            Line::from("permissions and workflow states. Your revisions are kept."),
            Line::from(""),
        ];
        let roles = base_roles(&self.roles);
        let height = (area.height as usize).saturating_sub(lines.len());
        let top = flow.base_top.reveal(selected, roles.len(), height);
        for (i, role) in roles.iter().enumerate().skip(top).take(height) {
            let marker = if i == selected { "▶ " } else { "  " };
            let mut tags = String::new();
            if role.id == draft.base_role {
                tags.push_str(" (current)");
            }
            if draft.persona.suggested_base.as_deref() == Some(role.id.as_str()) {
                tags.push_str(" (suggested)");
            }
            let line = Line::from(vec![
                Span::raw(format!("{marker}{}", role.title)),
                Span::raw(tags).cyan(),
                Span::raw(format!("  tools: {}", list_or_none(&role.mcp_tools))).dark_gray(),
                Span::raw(format!(
                    " · auto-approve: {}",
                    list_or_none(&role.permissions.auto_approve)
                ))
                .dark_gray(),
            ]);
            let line = truncate_line(line, area.width as usize);
            lines.push(if i == selected { line.bold() } else { line });
        }
        Paragraph::new(lines).render(area, buf);
    }
}

fn render_review(area: Rect, buf: &mut Buffer, review: &Review, roles: &[Role]) {
    let draft = &review.draft;
    let warns: Vec<_> = draft
        .findings
        .iter()
        .filter(|f| f.severity == Severity::Warn)
        .collect();
    let notes = draft.findings.len() - warns.len();
    let shown_warns = warns.len().min(4);
    let header_height = 3 + shown_warns as u16 + u16::from(!draft.revisions.is_empty());
    let [header, body, input] = Layout::vertical([
        Constraint::Length(header_height),
        Constraint::Min(0),
        Constraint::Length(if review.input.is_some() { 2 } else { 0 }),
    ])
    .areas(area);

    let width = header.width as usize;
    let mut lines = vec![truncate_line(
        Line::from(vec![
            "Base role: ".bold(),
            Span::raw(
                roles
                    .iter()
                    .find(|r| r.id == draft.base_role)
                    .map_or_else(|| draft.base_role.clone(), |r| r.title.clone()),
            ),
            Span::raw(
                if draft.persona.suggested_base.as_deref() == Some(draft.base_role.as_str()) {
                    " (suggested · b to change)"
                } else {
                    " (b to change)"
                },
            )
            .dark_gray(),
            " · agent: ".bold(),
            Span::raw(agent_label(draft)),
            " · from: ".bold(),
            Span::raw(draft.persona.path.clone()).dark_gray(),
        ]),
        width,
    )];
    let summary = match (warns.len(), notes) {
        (0, 0) => {
            Line::from("✓ No suspicious instructions found. Still, read it before hiring.").green()
        }
        (0, n) => Line::from(format!("✓ No warnings · {n} notes (links, installs)")).green(),
        (w, n) => Line::from(format!(
            "⚠ {w} {} · {n} notes: read the flagged lines",
            if w == 1 { "warning" } else { "warnings" }
        ))
        .yellow(),
    };
    lines.push(summary);
    for f in warns.iter().take(shown_warns) {
        lines.push(truncate_line(
            Line::from(format!("  L{} {}: {}", f.line, f.message, f.excerpt)).yellow(),
            width,
        ));
    }
    if let Some(last) = draft.revisions.last() {
        let summary = last
            .summary
            .lines()
            .next()
            .unwrap_or("")
            .trim_start_matches("- ");
        lines.push(truncate_line(
            Line::from(format!(
                "✎ {} revision(s) · last: \"{}\" → {summary}",
                draft.revisions.len(),
                last.instruction
            ))
            .cyan(),
            width,
        ));
    }
    let tabs = [
        (ReviewView::Persona, "Persona"),
        (ReviewView::Diff, "Changes"),
        (ReviewView::Prompt, "Full prompt"),
    ];
    let mut tab_spans = vec![];
    for (view, name) in tabs {
        let span = Span::raw(format!(" {name} "));
        tab_spans.push(if view == review.view {
            span.reversed()
        } else {
            span.dark_gray()
        });
    }
    lines.push(Line::from(tab_spans));
    Paragraph::new(lines).render(header, buf);

    let text_width = body.width.saturating_sub(6) as usize;
    let mut text: Vec<Line> = Vec::new();
    match review.view {
        ReviewView::Persona => {
            for (i, raw) in draft.persona_text.lines().enumerate() {
                let flagged = draft
                    .findings
                    .iter()
                    .filter(|f| f.line == i + 1)
                    .map(|f| f.severity)
                    .max();
                let (gutter, style) = match flagged {
                    Some(Severity::Warn) => ("⚠", Style::new().yellow()),
                    Some(Severity::Info) => ("·", Style::new().cyan()),
                    None => (" ", Style::new()),
                };
                for (j, part) in wrap(raw, text_width).into_iter().enumerate() {
                    let num = if j == 0 {
                        format!("{:>3}", i + 1)
                    } else {
                        "   ".into()
                    };
                    text.push(Line::from(vec![
                        Span::styled(format!("{gutter}{num} "), style),
                        Span::styled(
                            part,
                            if flagged == Some(Severity::Warn) {
                                style
                            } else {
                                Style::new()
                            },
                        ),
                    ]));
                }
            }
        }
        ReviewView::Diff => {
            if draft.revisions.is_empty() {
                text.push(Line::from("No changes yet. Press e to suggest one.").dark_gray());
            }
            for line in draft
                .diff
                .iter()
                .filter(|l| l.op != DiffOp::Same || !draft.revisions.is_empty())
            {
                let (sign, style) = match line.op {
                    DiffOp::Same => (" ", Style::new().dark_gray()),
                    DiffOp::Added => ("+", Style::new().green()),
                    DiffOp::Removed => ("-", Style::new().red()),
                };
                for part in wrap(&line.text, text_width) {
                    text.push(Line::styled(format!("{sign} {part}"), style));
                }
            }
        }
        ReviewView::Prompt => {
            for raw in draft.prompt.lines() {
                for part in wrap(raw, text_width) {
                    text.push(Line::from(part));
                }
            }
        }
    }
    let max_scroll = (text.len() as u16).saturating_sub(body.height);
    Paragraph::new(text)
        .scroll((review.scroll.min(max_scroll), 0))
        .render(body, buf);

    if let Some(typed) = &review.input {
        Paragraph::new(vec![
            Line::from("Suggest a change; an agent rewrites the persona and you'll see the diff:")
                .dark_gray(),
            Line::from(vec!["› ".cyan().bold(), Span::raw(format!("{typed}▏"))]),
        ])
        .wrap(Wrap { trim: false })
        .render(input, buf);
    }
}

fn render_confirm(area: Rect, buf: &mut Buffer, confirm: &Confirm) {
    let field = |label: &str, value: &str, active: bool| {
        let value = if active {
            format!("{value}▏")
        } else {
            value.to_string()
        };
        let line = Line::from(vec![
            Span::raw(format!("{label:<9}")).bold(),
            Span::raw(value),
        ]);
        if active { line.cyan() } else { line }
    };
    let draft = &confirm.draft;
    let mut lines = vec![
        Line::from(format!(
            "Adopt this prompt as a project role and hire someone into it. It is saved to \
             .chillax/roles/{}.md{}.",
            confirm.role_id.trim(),
            if draft.revisions.is_empty() {
                String::new()
            } else {
                format!(" with your {} revision(s)", draft.revisions.len())
            }
        )),
        Line::from(""),
        field("Name:", &confirm.name, confirm.field == 0),
        field("Role id:", &confirm.role_id, confirm.field == 1),
        Line::from(""),
        Line::from(format!(
            "Policy from {}; agent {}.",
            draft.base_role,
            agent_label(draft)
        ))
        .dark_gray(),
    ];
    if draft.persona.adopted_role.as_deref() == Some(confirm.role_id.trim()) {
        lines.push(
            Line::from(format!(
                "Replaces the role you adopted earlier as `{}`; its employees get the new prompt \
                 in their next session.",
                confirm.role_id.trim()
            ))
            .yellow(),
        );
    }
    Paragraph::new(lines)
        .wrap(Wrap { trim: true })
        .render(area, buf);
}

/// `claude`, or `claude · sonnet` when a model was picked.
fn agent_label(draft: &RoleDraft) -> String {
    match &draft.model {
        Some(model) => format!("{} · {model}", draft.backend),
        None => draft.backend.clone(),
    }
}

fn list_or_none(items: &[String]) -> String {
    if items.is_empty() {
        "none".into()
    } else {
        items.join(", ")
    }
}

/// Cut a line to `width` display columns, ending in `…` when cut.
pub(crate) fn truncate_line(line: Line<'static>, width: usize) -> Line<'static> {
    use unicode_width::UnicodeWidthStr;
    if line.width() <= width {
        return line;
    }
    let style = line.style;
    let mut used = 0;
    let mut spans = Vec::new();
    for span in line.spans {
        let mut content = String::new();
        for c in span.content.chars() {
            let w = c.to_string().width();
            if used + w > width.saturating_sub(1) {
                break;
            }
            used += w;
            content.push(c);
        }
        let done = content.chars().count() < span.content.chars().count();
        spans.push(Span::styled(content, span.style));
        if done {
            spans.push(Span::raw("…"));
            break;
        }
    }
    Line::from(spans).style(style)
}

/// Run one request against the recruiter and send back the reply.
pub(crate) async fn execute(
    recruiter: Option<std::sync::Arc<dyn chillax_core::Recruiter>>,
    agents: Option<std::sync::Arc<dyn chillax_core::AgentDirectory>>,
    request: RecruitRequest,
    replies: tokio::sync::mpsc::UnboundedSender<RecruitReply>,
) {
    let no_directory = || "No agent directory here; Enter keeps the defaults.".to_string();
    let reply = match &request {
        RecruitRequest::Backends => Some(RecruitReply::Backends(match &agents {
            Some(a) => Ok(a.backends().await),
            None => Err(no_directory()),
        })),
        RecruitRequest::Models { backend } => {
            let result = match &agents {
                Some(a) => a.models(backend).await.map_err(|e| e.to_string()),
                None => Err(no_directory()),
            };
            let efforts = match (&agents, &result) {
                (Some(a), Ok(_)) => a.efforts(backend).await.unwrap_or_default(),
                _ => Vec::new(),
            };
            Some(RecruitReply::Models {
                backend: backend.clone(),
                result,
                efforts,
            })
        }
        RecruitRequest::Enable { backend } => Some(RecruitReply::Enabled {
            backend: backend.clone(),
            result: match &agents {
                Some(a) => a.enable(backend).await.map_err(|e| e.to_string()),
                None => Err(no_directory()),
            },
        }),
        _ => None,
    };
    if let Some(reply) = reply {
        let _ = replies.send(reply);
        return;
    }
    let Some(r) = recruiter else {
        if request == RecruitRequest::Load {
            let _ = replies.send(RecruitReply::Loaded {
                status: None,
                featured: Vec::new(),
                all: Vec::new(),
                error: Some(
                    "The persona catalog is off ([catalog] enabled = false); built-in roles only."
                        .into(),
                ),
            });
        }
        return;
    };
    let reply = match request {
        RecruitRequest::Load => {
            // A startup refresh may still be running; wait for it (bounded) so the
            // first open after a fresh install shows the catalog.
            for _ in 0..600 {
                if !r.status().await.refreshing {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            let status = r.status().await;
            match (r.featured(10).await, r.personas().await) {
                (Ok(featured), Ok(all)) => RecruitReply::Loaded {
                    status: Some(status),
                    featured,
                    all,
                    error: None,
                },
                (Err(err), _) | (_, Err(err)) => RecruitReply::Loaded {
                    error: Some(match &status.error {
                        Some(refresh) => format!("{err} ({refresh})"),
                        None => err.to_string(),
                    }),
                    status: Some(status),
                    featured: Vec::new(),
                    all: Vec::new(),
                },
            }
        }
        RecruitRequest::Refresh => {
            RecruitReply::Refreshed(r.refresh().await.map_err(|e| e.to_string()))
        }
        RecruitRequest::Draft {
            persona,
            base_role,
            backend,
            model,
        } => RecruitReply::Draft(
            r.draft(&persona, base_role.as_deref(), backend, model)
                .await
                .map_err(|e| e.to_string()),
        ),
        RecruitRequest::DraftFromRole {
            from_role,
            role_id,
            title,
            backend,
            model,
        } => RecruitReply::Draft(
            r.draft_from_role(&from_role, &role_id, &title, backend, model)
                .await
                .map_err(|e| e.to_string()),
        ),
        RecruitRequest::SetBase { draft, base_role } => RecruitReply::Draft(
            r.set_base(&draft, &base_role)
                .await
                .map_err(|e| e.to_string()),
        ),
        RecruitRequest::Revise { draft, instruction } => RecruitReply::Draft(
            r.revise(&draft, &instruction)
                .await
                .map_err(|e| e.to_string()),
        ),
        RecruitRequest::Tailor { draft } => {
            RecruitReply::Draft(r.tailor(&draft).await.map_err(|e| e.to_string()))
        }
        RecruitRequest::Undo { draft } => {
            RecruitReply::Draft(r.undo(&draft).await.map_err(|e| e.to_string()))
        }
        RecruitRequest::Cancel { draft } => {
            RecruitReply::Draft(r.get_draft(&draft).await.map_err(|e| e.to_string()))
        }
        RecruitRequest::Adopt {
            draft,
            role_id,
            overwrite,
            name,
        } => RecruitReply::Adopted {
            result: r
                .adopt(&draft, Some(role_id), overwrite)
                .await
                .map_err(|e| e.to_string()),
            name,
        },
        RecruitRequest::Discard { draft } => {
            r.discard(&draft).await;
            return;
        }
        RecruitRequest::Backends
        | RecruitRequest::Models { .. }
        | RecruitRequest::Enable { .. } => {
            return;
        }
    };
    let _ = replies.send(reply);
}
