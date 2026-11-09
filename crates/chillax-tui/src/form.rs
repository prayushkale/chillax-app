//! The create / edit form for Board work items. It only collects and validates input;
//! saving yields the same [`Command`]s the MCP planning tools send.

use chillax_core::{Command, Ticket, TicketKind};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// What saving the form does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormTarget {
    Create,
    Edit { id: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Kind,
    Title,
    Description,
    Criteria,
    Estimate,
    Epic,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TicketForm {
    pub target: FormTarget,
    pub kind: TicketKind,
    pub title: String,
    pub description: String,
    /// One acceptance criterion per line.
    pub criteria: String,
    pub estimate: String,
    /// The epics to choose from, as (id, title). The Epic field exists only when there are any.
    pub epics: Vec<(String, String)>,
    /// The chosen epic.
    pub epic: Option<String>,
    /// The epic the ticket had when editing began.
    original_epic: Option<String>,
    pub focus: Field,
    /// Validation message shown inside the form.
    pub error: Option<String>,
    /// First row drawn when the form is taller than its popup.
    pub(crate) top: crate::scroll::Sticky,
}

/// What a key press did to the form.
pub enum FormOutcome {
    Open,
    Cancel,
    Save(Command),
}

const KINDS: [TicketKind; 3] = [TicketKind::Story, TicketKind::Bug, TicketKind::Task];

impl TicketForm {
    pub fn create() -> Self {
        Self {
            target: FormTarget::Create,
            kind: TicketKind::Story,
            title: String::new(),
            description: String::new(),
            criteria: String::new(),
            estimate: String::new(),
            epics: Vec::new(),
            epic: None,
            original_epic: None,
            focus: Field::Title,
            error: None,
            top: Default::default(),
        }
    }

    /// Offer these epics (id, title) in the Epic field.
    pub fn with_epics(mut self, epics: Vec<(String, String)>) -> Self {
        self.epics = epics;
        self
    }

    pub fn edit(ticket: &Ticket) -> Self {
        Self {
            target: FormTarget::Edit {
                id: ticket.id.clone(),
            },
            kind: ticket.kind,
            title: ticket.title.clone(),
            description: ticket.description.clone(),
            criteria: ticket.acceptance_criteria.join("\n"),
            estimate: ticket.estimate.map(|e| e.to_string()).unwrap_or_default(),
            epics: Vec::new(),
            epic: ticket.epic.clone(),
            original_epic: ticket.epic.clone(),
            focus: Field::Title,
            error: None,
            top: Default::default(),
        }
    }

    /// Fields in tab order. The kind is fixed once a ticket exists.
    pub fn fields(&self) -> Vec<Field> {
        let mut fields = Vec::new();
        if self.target == FormTarget::Create {
            fields.push(Field::Kind);
        }
        fields.extend([
            Field::Title,
            Field::Description,
            Field::Criteria,
            Field::Estimate,
        ]);
        if !self.epics.is_empty() {
            fields.push(Field::Epic);
        }
        fields
    }

    fn move_focus(&mut self, forward: bool) {
        let fields = self.fields();
        let at = fields.iter().position(|f| *f == self.focus).unwrap_or(0);
        let next = if forward {
            (at + 1) % fields.len()
        } else {
            (at + fields.len() - 1) % fields.len()
        };
        self.focus = fields[next];
    }

    fn text_mut(&mut self) -> Option<&mut String> {
        match self.focus {
            Field::Kind | Field::Epic => None,
            Field::Title => Some(&mut self.title),
            Field::Description => Some(&mut self.description),
            Field::Criteria => Some(&mut self.criteria),
            Field::Estimate => Some(&mut self.estimate),
        }
    }

    /// Pasted text goes into the focused text field; single-line fields get the lines joined.
    pub fn paste(&mut self, pasted: &str) {
        let multiline = matches!(self.focus, Field::Description | Field::Criteria);
        if let Some(text) = self.text_mut() {
            if multiline {
                text.push_str(pasted);
            } else {
                text.push_str(&pasted.replace('\n', " "));
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> FormOutcome {
        self.error = None;
        let multiline = matches!(self.focus, Field::Description | Field::Criteria);
        match key.code {
            KeyCode::Esc => return FormOutcome::Cancel,
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return self.save();
            }
            KeyCode::Tab | KeyCode::Down if !multiline || key.code == KeyCode::Tab => {
                self.move_focus(true)
            }
            KeyCode::BackTab => self.move_focus(false),
            KeyCode::Up if !multiline => self.move_focus(false),
            KeyCode::Left | KeyCode::Right if self.focus == Field::Kind => {
                let at = KINDS.iter().position(|k| *k == self.kind).unwrap_or(0);
                let next = if key.code == KeyCode::Right {
                    (at + 1) % KINDS.len()
                } else {
                    (at + KINDS.len() - 1) % KINDS.len()
                };
                self.kind = KINDS[next];
            }
            KeyCode::Left | KeyCode::Right if self.focus == Field::Epic => {
                let options: Vec<Option<&String>> = std::iter::once(None)
                    .chain(self.epics.iter().map(|(id, _)| Some(id)))
                    .collect();
                let at = options
                    .iter()
                    .position(|o| o.map(String::as_str) == self.epic.as_deref())
                    .unwrap_or(0);
                let next = if key.code == KeyCode::Right {
                    (at + 1) % options.len()
                } else {
                    (at + options.len() - 1) % options.len()
                };
                self.epic = options[next].cloned();
            }
            KeyCode::Enter if multiline => {
                if let Some(text) = self.text_mut() {
                    text.push('\n');
                }
            }
            KeyCode::Enter if matches!(self.focus, Field::Estimate | Field::Epic) => {
                return self.save();
            }
            KeyCode::Enter => self.move_focus(true),
            KeyCode::Backspace => {
                if let Some(text) = self.text_mut() {
                    text.pop();
                }
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(text) = self.text_mut() {
                    text.push(c);
                }
            }
            _ => {}
        }
        FormOutcome::Open
    }

    /// Validate and build the command; on failure the message lands in `error`.
    fn save(&mut self) -> FormOutcome {
        let title = self.title.trim().to_string();
        if title.is_empty() {
            self.error = Some("Title is required".into());
            self.focus = Field::Title;
            return FormOutcome::Open;
        }
        let estimate = match self.estimate.trim() {
            "" => None,
            s => match s.parse::<u32>() {
                Ok(n) => Some(n),
                Err(_) => {
                    self.error = Some("Estimate must be a whole number".into());
                    self.focus = Field::Estimate;
                    return FormOutcome::Open;
                }
            },
        };
        let description = self.description.trim().to_string();
        let acceptance_criteria: Vec<String> = self
            .criteria
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect();
        FormOutcome::Save(match &self.target {
            FormTarget::Create => Command::CreateTicket {
                kind: self.kind,
                title,
                description,
                acceptance_criteria,
                estimate,
                epic: self.epic.clone(),
                external: None,
            },
            // `UpdateTicket` cannot clear an estimate or an epic, so a blank estimate and
            // "no epic" leave them as they are.
            FormTarget::Edit { id } => Command::UpdateTicket {
                id: id.clone(),
                title: Some(title),
                description: Some(description),
                acceptance_criteria: Some(acceptance_criteria),
                estimate,
                epic: if self.epic != self.original_epic {
                    self.epic.clone()
                } else {
                    None
                },
            },
        })
    }
}
