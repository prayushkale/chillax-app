//! Choosing the coding agent, model and effort: a step of the hire wizard and
//! the Settings tab. Starts on the stakeholder's hiring defaults, so Enter twice
//! (three times when the agent offers effort levels) keeps them.
//!
//! The picker never does IO: it queues [`RecruitRequest`]s for the agent directory and
//! takes their replies through [`AgentPicker::on_reply`].

use chillax_core::agents::match_model;
use chillax_core::{BackendInfo, HiringDefaults, ModelOption};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::hire::{RecruitReply, RecruitRequest};
use crate::scroll::Sticky;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PickStage {
    #[default]
    Backend,
    Model,
    /// Only when the agent offers effort levels and the caller wants them.
    Effort,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentPicker {
    /// Where the choice starts.
    pub defaults: HiringDefaults,
    /// `None` while loading.
    pub backends: Option<Vec<BackendInfo>>,
    pub selected: usize,
    pub stage: PickStage,
    /// Models of the selected backend; `None` while the agent is probed.
    pub models: Option<Vec<ModelOption>>,
    /// Row in [`AgentPicker::model_rows`].
    pub model: usize,
    /// A model id typed by hand, for agents that don't list theirs.
    pub custom: String,
    /// Effort levels of the selected backend (for its default model); `None` while the
    /// agent is probed, empty when it has none.
    pub efforts: Option<Vec<ModelOption>>,
    /// Row in [`AgentPicker::effort_rows`].
    pub effort: usize,
    /// Skip the effort step: for flows that cannot carry an effort (persona drafts).
    pub skip_effort: bool,
    /// The model chosen at the Model stage, while the Effort stage is open.
    picked_model: Option<String>,
    pub error: Option<String>,
    /// First row drawn in the agents and models windows.
    agents_top: Sticky,
    models_top: Sticky,
    efforts_top: Sticky,
}

/// What a key press did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Picked {
    Pending,
    Cancelled,
    /// `backend: None` leaves the choice to the engine (no agent list available).
    Done {
        backend: Option<String>,
        model: Option<String>,
        effort: Option<String>,
    },
}

impl AgentPicker {
    /// A picker on `defaults`; queue [`RecruitRequest::Backends`] alongside.
    pub fn new(defaults: HiringDefaults) -> Self {
        Self {
            defaults,
            ..Self::default()
        }
    }

    /// Waiting on the backend list or on a backend's models.
    pub fn is_loading(&self) -> bool {
        self.backends.is_none() || (self.stage == PickStage::Model && self.models.is_none())
    }

    fn backend(&self) -> Option<&BackendInfo> {
        self.backends.as_ref()?.get(self.selected)
    }

    /// `None` is the agent's own default model.
    pub fn model_rows(&self) -> Vec<(Option<String>, String)> {
        let mut rows = vec![(None, "Agent's default".to_string())];
        for m in self.models.iter().flatten().filter(|m| m.id != "default") {
            let label = match &m.description {
                Some(d) if !d.is_empty() => format!("{} — {d}", m.name),
                _ => m.name.clone(),
            };
            rows.push((Some(m.id.clone()), label));
        }
        rows
    }

    /// `None` is the agent's own default effort.
    pub fn effort_rows(&self) -> Vec<(Option<String>, String)> {
        let mut rows = vec![(None, "Agent's default".to_string())];
        for e in self.efforts.iter().flatten().filter(|e| e.id != "default") {
            let label = match &e.description {
                Some(d) if !d.is_empty() => format!("{} — {d}", e.name),
                _ => e.name.clone(),
            };
            rows.push((Some(e.id.clone()), label));
        }
        rows
    }

    /// Whether the defaults' effort was set for this backend and model.
    fn defaults_cover(&self, backend: &str, model: &Option<String>) -> bool {
        if self.defaults.backend.as_deref() != Some(backend) {
            return false;
        }
        match (&self.defaults.model, model) {
            (None, None) => true,
            (Some(wanted), Some(picked)) => {
                let models = self.models.clone().unwrap_or_default();
                match_model(&models, wanted).map_or(wanted == picked, |m| &m.id == picked)
            }
            _ => false,
        }
    }

    pub fn key(&mut self, key: KeyEvent, requests: &mut Vec<RecruitRequest>) -> Picked {
        self.error = None;
        match self.stage {
            PickStage::Backend => match key.code {
                KeyCode::Esc => return Picked::Cancelled,
                KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    let last = self
                        .backends
                        .as_ref()
                        .map_or(0, |b| b.len().saturating_sub(1));
                    self.selected = (self.selected + 1).min(last);
                }
                KeyCode::Enter => match (&self.backends, self.backend().cloned()) {
                    // No directory to ask: let the engine use the defaults.
                    (Some(list), _) if list.is_empty() => {
                        return Picked::Done {
                            backend: None,
                            model: None,
                            effort: None,
                        };
                    }
                    (_, Some(backend)) => {
                        self.stage = PickStage::Model;
                        self.models = None;
                        self.efforts = None;
                        self.model = 0;
                        self.effort = 0;
                        self.custom.clear();
                        requests.push(if backend.configured {
                            RecruitRequest::Models {
                                backend: backend.name,
                            }
                        } else {
                            RecruitRequest::Enable {
                                backend: backend.name,
                            }
                        });
                    }
                    _ => {}
                },
                _ => {}
            },
            PickStage::Model => match key.code {
                KeyCode::Esc => self.stage = PickStage::Backend,
                KeyCode::Up => self.model = self.model.saturating_sub(1),
                KeyCode::Down => {
                    self.model = (self.model + 1).min(self.model_rows().len().saturating_sub(1));
                }
                KeyCode::Backspace => {
                    self.custom.pop();
                }
                KeyCode::Char(c) => self.custom.push(c),
                KeyCode::Enter => {
                    let custom = self.custom.trim().to_string();
                    if custom.is_empty() && self.models.is_none() {
                        // Still asking the agent; nothing to choose from yet.
                        return Picked::Pending;
                    }
                    let model = if custom.is_empty() {
                        self.model_rows()
                            .get(self.model)
                            .and_then(|(id, _)| id.clone())
                    } else {
                        Some(custom)
                    };
                    let backend = self.backend().map(|b| b.name.clone());
                    let offered = self.efforts.as_deref().is_some_and(|e| !e.is_empty());
                    if self.skip_effort || !offered {
                        return Picked::Done {
                            backend,
                            model,
                            effort: None,
                        };
                    }
                    self.effort = 0;
                    if let (Some(name), Some(wanted)) = (&backend, &self.defaults.effort)
                        && self.defaults_cover(name, &model)
                        && let Some(e) = match_model(self.efforts.as_deref().unwrap_or(&[]), wanted)
                    {
                        self.effort = self
                            .effort_rows()
                            .iter()
                            .position(|(id, _)| id.as_deref() == Some(e.id.as_str()))
                            .unwrap_or(0);
                    }
                    self.picked_model = model;
                    self.stage = PickStage::Effort;
                }
                _ => {}
            },
            PickStage::Effort => match key.code {
                KeyCode::Esc => self.stage = PickStage::Model,
                KeyCode::Up | KeyCode::Char('k') => self.effort = self.effort.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    self.effort = (self.effort + 1).min(self.effort_rows().len().saturating_sub(1));
                }
                KeyCode::Enter => {
                    return Picked::Done {
                        backend: self.backend().map(|b| b.name.clone()),
                        model: self.picked_model.clone(),
                        effort: self
                            .effort_rows()
                            .get(self.effort)
                            .and_then(|(id, _)| id.clone()),
                    };
                }
                _ => {}
            },
        }
        Picked::Pending
    }

    /// Take a directory reply. Returns `false` if it wasn't for the picker.
    pub fn on_reply(&mut self, reply: &RecruitReply, requests: &mut Vec<RecruitRequest>) -> bool {
        match reply {
            RecruitReply::Backends(result) => {
                match result {
                    Ok(list) => {
                        let wanted = self.defaults.backend.as_deref().unwrap_or("claude");
                        self.selected = list.iter().position(|b| b.name == wanted).unwrap_or(0);
                        self.backends = Some(list.clone());
                    }
                    Err(err) => {
                        self.error = Some(err.clone());
                        self.backends = Some(Vec::new());
                    }
                }
                true
            }
            RecruitReply::Enabled { backend, result } => {
                if self.backend().is_none_or(|b| b.name != *backend) {
                    return true;
                }
                match result {
                    Ok(info) => {
                        if let Some(b) = self
                            .backends
                            .as_mut()
                            .and_then(|list| list.get_mut(self.selected))
                        {
                            *b = info.clone();
                        }
                        requests.push(RecruitRequest::Models {
                            backend: backend.clone(),
                        });
                    }
                    Err(err) => {
                        self.error = Some(err.clone());
                        self.stage = PickStage::Backend;
                    }
                }
                true
            }
            RecruitReply::Models {
                backend,
                result,
                efforts,
            } => {
                if self.stage != PickStage::Model
                    || self.backend().is_none_or(|b| b.name != *backend)
                {
                    return true;
                }
                match result {
                    Ok(models) => {
                        self.models = Some(models.clone());
                        self.efforts = Some(efforts.clone());
                    }
                    Err(err) => {
                        // The stakeholder can still keep the agent's default or type an id.
                        self.error = Some(err.clone());
                        self.models = Some(Vec::new());
                        self.efforts = Some(Vec::new());
                    }
                }
                self.model = 0;
                if self.defaults.backend.as_deref() == Some(backend.as_str())
                    && let Some(wanted) = &self.defaults.model
                {
                    let models = self.models.clone().unwrap_or_default();
                    match match_model(&models, wanted) {
                        Some(m) => {
                            self.model = self
                                .model_rows()
                                .iter()
                                .position(|(id, _)| id.as_deref() == Some(m.id.as_str()))
                                .unwrap_or(0);
                        }
                        // Not in the agent's list: keep it as typed.
                        None => self.custom = wanted.clone(),
                    }
                }
                true
            }
            _ => false,
        }
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer, tick: u64) {
        let width = area.width as usize;
        // Errors wrap: they say what to do (e.g. how to sign the agent in).
        let errors: Vec<String> = self
            .error
            .as_ref()
            .map(|err| crate::view::wrap(&format!("⚠ {err}"), width.max(10)))
            .unwrap_or_default();
        let model_stage = self.stage == PickStage::Model;
        let effort_stage = self.stage == PickStage::Effort;
        // Heading, blank + footer, the model heading, blank and the typed-id line, errors.
        // The effort stage swaps the model section for a one-line summary and a heading.
        let fixed =
            3 + match self.stage {
                PickStage::Backend => 0,
                PickStage::Model => 3,
                PickStage::Effort => 4,
            } + errors.len();
        let budget = (area.height as usize).saturating_sub(fixed).max(2);
        let agent_count = self.backends.as_ref().map_or(1, |b| b.len().max(1));
        let model_count = if model_stage {
            self.models.as_ref().map_or(1, |_| self.model_rows().len())
        } else {
            0
        };
        let model_count = if effort_stage {
            self.effort_rows().len()
        } else {
            model_count
        };
        let (agent_h, model_h) = if agent_count + model_count <= budget {
            (agent_count, model_count)
        } else if !model_stage && !effort_stage {
            (budget, 0)
        } else {
            let agents = (budget / 2).clamp(1, agent_count);
            (agents, (budget - agents).max(1))
        };
        let more = |top: usize, len: usize, h: usize| {
            let mut s = String::new();
            if top > 0 {
                s.push_str(&format!("  ↑ {top} more"));
            }
            if top + h < len {
                s.push_str(&format!("  ↓ {} more", len - top - h));
            }
            Span::raw(s).dark_gray()
        };
        let mut lines: Vec<Line> = Vec::new();
        let mut heading = vec![Span::raw("Coding agent").bold()];
        match &self.backends {
            None => {
                lines.push(Line::default());
                lines.push(
                    Line::from(format!(
                        "  {} Looking for agents…",
                        crate::spinner::frame(tick)
                    ))
                    .dark_gray(),
                );
            }
            Some(list) if list.is_empty() => {
                lines.push(Line::default());
                lines.push(
                    Line::from("  No agent list available; Enter keeps the current default.")
                        .dark_gray(),
                );
            }
            Some(list) => {
                let top = self.agents_top.reveal(self.selected, list.len(), agent_h);
                heading.push(more(top, list.len(), agent_h));
                lines.push(Line::default());
                for (i, b) in list.iter().enumerate().skip(top).take(agent_h) {
                    let here = i == self.selected;
                    let marker = if here { "▶ " } else { "  " };
                    let mut spans = vec![Span::raw(format!("{marker}{} ({})", b.title, b.name))];
                    if self.defaults.backend.as_deref() == Some(b.name.as_str()) {
                        spans.push(Span::raw(" (default)").cyan());
                    }
                    if !b.configured {
                        spans.push(
                            Span::raw(format!(
                                "  installed · adds [agents.{}] to chillax.toml",
                                b.name
                            ))
                            .dark_gray(),
                        );
                    } else if !b.available {
                        spans.push(
                            Span::raw(format!("  `{}` not found on PATH", b.command)).yellow(),
                        );
                    }
                    let line = Line::from(spans);
                    lines.push(if here && self.stage == PickStage::Backend {
                        line.bold()
                    } else {
                        line
                    });
                }
            }
        }
        lines[0] = Line::from(heading);
        if model_stage {
            lines.push(Line::from(""));
            let name = self.backend().map_or("the agent", |b| b.title.as_str());
            let mut heading = vec![Span::raw(format!("Model on {name}")).bold()];
            let mut rows = Vec::new();
            match &self.models {
                None => rows.push(
                    Line::from(format!(
                        "  {} Starting {name} to list its models… (no prompt, no cost)",
                        crate::spinner::frame(tick)
                    ))
                    .dark_gray(),
                ),
                Some(_) => {
                    let all = self.model_rows();
                    let top = self.models_top.reveal(self.model, all.len(), model_h);
                    heading.push(more(top, all.len(), model_h));
                    for (i, (_, label)) in all.iter().enumerate().skip(top).take(model_h) {
                        let here = i == self.model && self.custom.trim().is_empty();
                        let marker = if here { "▶ " } else { "  " };
                        let line = Line::from(format!("{marker}{label}"));
                        rows.push(if here { line.bold() } else { line });
                    }
                }
            }
            lines.push(Line::from(heading));
            lines.extend(rows);
            lines.push(Line::from(vec![
                "Or type a model id: ".dark_gray(),
                Span::raw(format!("{}▏", self.custom)),
            ]));
        }
        if effort_stage {
            lines.push(Line::from(""));
            let name = self.backend().map_or("the agent", |b| b.title.as_str());
            let model = self.picked_model.as_deref().unwrap_or("agent's default");
            lines.push(Line::from(vec![
                Span::raw(format!("Model on {name}: ")).bold(),
                Span::raw(model.to_string()),
            ]));
            lines.push(Line::from(""));
            let all = self.effort_rows();
            let top = self.efforts_top.reveal(self.effort, all.len(), model_h);
            lines.push(Line::from(vec![
                Span::raw("Effort").bold(),
                more(top, all.len(), model_h),
            ]));
            for (i, (_, label)) in all.iter().enumerate().skip(top).take(model_h) {
                let here = i == self.effort;
                let marker = if here { "▶ " } else { "  " };
                let line = Line::from(format!("{marker}{label}"));
                lines.push(if here { line.bold() } else { line });
            }
        }
        lines.push(Line::from(""));
        lines.push(
            Line::from("Your pick becomes the default for new hires and persona revisions.")
                .dark_gray(),
        );
        let mut lines: Vec<Line> = lines
            .into_iter()
            .map(|l| crate::hire::truncate_line(l, width))
            .collect();
        lines.extend(errors.into_iter().map(|part| Line::from(part).red()));
        Paragraph::new(lines).render(area, buf);
    }
}
