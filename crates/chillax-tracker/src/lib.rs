//! The `Tracker` adapter trait and the built-in `local` tracker.
//!
//! Chillax MCP tools talk to a `dyn Tracker`, never to a specific tracker. The local
//! tracker keeps work items as events in Chillax's own log (ADR 0002). Other trackers
//! plug in by implementing the trait.

use std::sync::Arc;

use async_trait::async_trait;
use chillax_core::{
    Actor, Command, CommandError, CompanyHandle, EmployeeId, Epic, EpicId, Event, EventKind,
    Sprint, Ticket, TicketId, TicketKind,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum TrackerError {
    #[error(transparent)]
    Rejected(#[from] CommandError),
    #[error("{0} is not supported by the {1} tracker")]
    Unsupported(&'static str, &'static str),
    #[error("tracker returned no {0}")]
    Missing(&'static str),
    #[error("cannot reach the tracker: {0}")]
    Http(String),
}

impl TrackerError {
    /// Worth retrying later (network, rate limits, server errors), as opposed to a
    /// request the tracker will keep refusing.
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Http(_))
    }
}

pub type Result<T> = std::result::Result<T, TrackerError>;

/// Who a work item is assigned to. Chillax owns agent identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum Assignee {
    Agent(EmployeeId),
    Human(String),
    Unassigned,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewEpic {
    pub title: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewWorkItem {
    pub kind: TicketKind,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub estimate: Option<u32>,
    #[serde(default)]
    pub epic: Option<EpicId>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkItemPatch {
    pub title: Option<String>,
    pub description: Option<String>,
    pub acceptance_criteria: Option<Vec<String>>,
    pub estimate: Option<u32>,
    #[serde(default)]
    pub epic: Option<EpicId>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewSprint {
    pub name: String,
    #[serde(default)]
    pub goal: String,
    pub items: Vec<TicketId>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemFilter {
    pub state: Option<String>,
    pub assignee: Option<EmployeeId>,
    pub epic: Option<EpicId>,
}

#[async_trait]
pub trait Tracker: Send + Sync {
    fn kind(&self) -> &'static str;
    async fn create_epic(&self, by: &Actor, epic: NewEpic) -> Result<Epic>;
    async fn list_epics(&self) -> Result<Vec<Epic>>;
    async fn create_item(&self, by: &Actor, item: NewWorkItem) -> Result<Ticket>;
    async fn update_item(&self, by: &Actor, id: &str, patch: WorkItemPatch) -> Result<()>;
    async fn get_item(&self, id: &str) -> Result<Ticket>;
    async fn list_items(&self, filter: ItemFilter) -> Result<Vec<Ticket>>;
    async fn transition(&self, by: &Actor, id: &str, to: &str) -> Result<()>;
    async fn assign(&self, by: &Actor, id: &str, to: Assignee) -> Result<()>;
    async fn comment(&self, by: &Actor, id: &str, body: &str) -> Result<()>;
    /// Comment with images. Additive: a tracker without support refuses.
    async fn comment_with(
        &self,
        by: &Actor,
        id: &str,
        body: &str,
        attachments: Vec<chillax_core::Attachment>,
    ) -> Result<()> {
        if attachments.is_empty() {
            return self.comment(by, id, body).await;
        }
        Err(TrackerError::Unsupported("attachments", self.kind()))
    }
    async fn create_sprint(&self, by: &Actor, sprint: NewSprint) -> Result<Sprint>;
    /// Won't do: take the item out of play. Additive; a tracker that cannot
    /// say so refuses.
    async fn drop_item(&self, _by: &Actor, _id: &str, _reason: &str) -> Result<()> {
        Err(TrackerError::Unsupported(
            "dropping work items",
            self.kind(),
        ))
    }
}

/// Work items live in Chillax's event log.
pub struct LocalTracker {
    company: Arc<dyn CompanyHandle>,
}

impl LocalTracker {
    pub fn new(company: Arc<dyn CompanyHandle>) -> Self {
        Self { company }
    }

    async fn send(&self, by: &Actor, cmd: Command) -> Result<Vec<Event>> {
        Ok(self.company.send(by.clone(), cmd).await?)
    }
}

#[async_trait]
impl Tracker for LocalTracker {
    fn kind(&self) -> &'static str {
        "local"
    }

    async fn create_epic(&self, by: &Actor, epic: NewEpic) -> Result<Epic> {
        let events = self
            .send(
                by,
                Command::CreateEpic {
                    title: epic.title,
                    description: epic.description,
                },
            )
            .await?;
        events
            .into_iter()
            .find_map(|e| match e.kind {
                EventKind::EpicCreated { epic } => Some(epic),
                _ => None,
            })
            .ok_or(TrackerError::Missing("epic"))
    }

    async fn list_epics(&self) -> Result<Vec<Epic>> {
        Ok(self.company.snapshot().await.epics)
    }

    async fn create_item(&self, by: &Actor, item: NewWorkItem) -> Result<Ticket> {
        let events = self
            .send(
                by,
                Command::CreateTicket {
                    kind: item.kind,
                    title: item.title,
                    description: item.description,
                    acceptance_criteria: item.acceptance_criteria,
                    estimate: item.estimate,
                    epic: item.epic,
                    external: None,
                },
            )
            .await?;
        events
            .into_iter()
            .find_map(|e| match e.kind {
                EventKind::TicketCreated { ticket } => Some(ticket),
                _ => None,
            })
            .ok_or(TrackerError::Missing("work item"))
    }

    async fn update_item(&self, by: &Actor, id: &str, patch: WorkItemPatch) -> Result<()> {
        self.send(
            by,
            Command::UpdateTicket {
                id: id.to_string(),
                title: patch.title,
                description: patch.description,
                acceptance_criteria: patch.acceptance_criteria,
                estimate: patch.estimate,
                epic: patch.epic,
            },
        )
        .await?;
        Ok(())
    }

    async fn get_item(&self, id: &str) -> Result<Ticket> {
        self.company
            .snapshot()
            .await
            .ticket(id)
            .cloned()
            .ok_or_else(|| CommandError::UnknownTicket(id.to_string()).into())
    }

    async fn list_items(&self, filter: ItemFilter) -> Result<Vec<Ticket>> {
        let state = self.company.snapshot().await;
        Ok(state
            .tickets
            .into_iter()
            .filter(|t| t.dropped.is_none())
            .filter(|t| filter.state.as_ref().is_none_or(|s| &t.state == s))
            .filter(|t| filter.assignee.is_none() || t.assignee == filter.assignee)
            .filter(|t| filter.epic.is_none() || t.epic == filter.epic)
            .collect())
    }

    async fn transition(&self, by: &Actor, id: &str, to: &str) -> Result<()> {
        self.send(
            by,
            Command::TransitionTicket {
                id: id.to_string(),
                to: to.to_string(),
            },
        )
        .await?;
        Ok(())
    }

    async fn assign(&self, by: &Actor, id: &str, to: Assignee) -> Result<()> {
        let assignee = match to {
            Assignee::Agent(employee) => Some(employee),
            Assignee::Unassigned => None,
            Assignee::Human(_) => {
                return Err(TrackerError::Unsupported("assigning humans", "local"));
            }
        };
        self.send(
            by,
            Command::AssignTicket {
                id: id.to_string(),
                assignee,
            },
        )
        .await?;
        Ok(())
    }

    async fn comment(&self, by: &Actor, id: &str, body: &str) -> Result<()> {
        self.comment_with(by, id, body, Vec::new()).await
    }

    async fn comment_with(
        &self,
        by: &Actor,
        id: &str,
        body: &str,
        attachments: Vec<chillax_core::Attachment>,
    ) -> Result<()> {
        self.send(
            by,
            Command::CommentTicket {
                id: id.to_string(),
                body: body.to_string(),
                external_id: None,
                attachments,
            },
        )
        .await?;
        Ok(())
    }

    async fn drop_item(&self, by: &Actor, id: &str, reason: &str) -> Result<()> {
        self.send(
            by,
            Command::DropTicket {
                id: id.to_string(),
                reason: reason.to_string(),
            },
        )
        .await?;
        Ok(())
    }

    async fn create_sprint(&self, by: &Actor, sprint: NewSprint) -> Result<Sprint> {
        let events = self
            .send(
                by,
                Command::ProposeSprint {
                    name: sprint.name,
                    goal: sprint.goal,
                    tickets: sprint.items,
                },
            )
            .await?;
        events
            .into_iter()
            .find_map(|e| match e.kind {
                EventKind::SprintProposed { sprint, .. } => Some(sprint),
                _ => None,
            })
            .ok_or(TrackerError::Missing("sprint"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chillax_core::{Engine, Hub, MemoryLog, RoleCatalog, SprintStatus, Workflow};

    async fn tracker() -> LocalTracker {
        let engine = Engine::new("Acme", RoleCatalog::default(), Workflow::default());
        let hub = Hub::open(engine, Arc::new(MemoryLog::default()))
            .await
            .expect("hub opens");
        LocalTracker::new(Arc::new(hub))
    }

    #[tokio::test]
    async fn plans_a_sprint_end_to_end() {
        let t = tracker().await;
        let pm = Actor::Employee("pat".into());
        let epic = t
            .create_epic(
                &pm,
                NewEpic {
                    title: "Auth".into(),
                    description: String::new(),
                },
            )
            .await
            .expect("epic");
        let item = t
            .create_item(
                &pm,
                NewWorkItem {
                    kind: TicketKind::Story,
                    title: "Login".into(),
                    description: "As a user…".into(),
                    acceptance_criteria: vec!["valid creds log in".into()],
                    estimate: Some(3),
                    epic: Some(epic.id.clone()),
                },
            )
            .await
            .expect("item");
        assert_eq!((item.id.as_str(), item.state.as_str()), ("T-1", "backlog"));

        t.comment(&pm, "T-1", "ready for planning")
            .await
            .expect("comment");
        assert_eq!(
            t.get_item("T-1").await.expect("get").comments[0].author,
            "pat"
        );

        let sprint = t
            .create_sprint(
                &pm,
                NewSprint {
                    name: "S1".into(),
                    goal: String::new(),
                    items: vec!["T-1".into()],
                },
            )
            .await
            .expect("sprint");
        assert_eq!(sprint.status, SprintStatus::Proposed);

        let filtered = t
            .list_items(ItemFilter {
                epic: Some(epic.id),
                ..ItemFilter::default()
            })
            .await
            .expect("list");
        assert_eq!(filtered.len(), 1);
        assert!(matches!(
            t.assign(&pm, "T-1", Assignee::Human("bob".into())).await,
            Err(TrackerError::Unsupported(..))
        ));
        assert!(matches!(
            t.transition(&pm, "T-1", "done").await,
            Err(TrackerError::Rejected(
                CommandError::TransitionNotAllowed { .. }
            ))
        ));
    }

    #[tokio::test]
    async fn dropping_hides_an_item_from_listings_and_agents_cannot_do_it() {
        let t = tracker().await;
        let pm = Actor::Employee("pat".into());
        let item = |title: &str| NewWorkItem {
            kind: TicketKind::Task,
            title: title.into(),
            description: String::new(),
            acceptance_criteria: vec![],
            estimate: None,
            epic: None,
        };
        t.create_item(&pm, item("Keep")).await.expect("item");
        t.create_item(&pm, item("Toss")).await.expect("item");
        // Only the stakeholder drops work.
        assert!(matches!(
            t.drop_item(&pm, "T-2", "stale").await,
            Err(TrackerError::Rejected(_))
        ));
        t.drop_item(&Actor::Stakeholder, "T-2", "stale")
            .await
            .expect("dropped");
        let open = t.list_items(ItemFilter::default()).await.expect("list");
        assert_eq!(open.len(), 1);
        assert_eq!(
            t.get_item("T-2").await.expect("get").dropped.as_deref(),
            Some("stale")
        );
    }
}
