//! The hub: serialises commands, persists their events, applies them to the projection,
//! and broadcasts them. Frontends, MCP tools and the agent runtime all go through a
//! [`CompanyHandle`].

use std::sync::{Arc, Mutex as StdMutex, RwLock};

use async_trait::async_trait;
use chrono::Utc;
use tokio::sync::{Mutex, broadcast};

use crate::command::Command;
use crate::config::TeamConfig;
use crate::engine::{Engine, State};
use crate::error::CommandError;
use crate::event::{Actor, Event, EventKind};
use crate::role::{Role, RoleCatalog};
use crate::workflow::Workflow;

/// Append-only event storage. `chillax-store` implements it over SQLite.
#[async_trait]
pub trait EventLog: Send + Sync {
    async fn append(&self, events: &[Event]) -> Result<(), CommandError>;
    async fn load_all(&self) -> Result<Vec<Event>, CommandError>;
    async fn since(&self, seq: u64) -> Result<Vec<Event>, CommandError>;
}

/// In-memory log for tests and headless experiments.
#[derive(Debug, Default)]
pub struct MemoryLog {
    events: StdMutex<Vec<Event>>,
}

#[async_trait]
impl EventLog for MemoryLog {
    async fn append(&self, events: &[Event]) -> Result<(), CommandError> {
        self.events
            .lock()
            .expect("memory log lock poisoned")
            .extend_from_slice(events);
        Ok(())
    }

    async fn load_all(&self) -> Result<Vec<Event>, CommandError> {
        Ok(self
            .events
            .lock()
            .expect("memory log lock poisoned")
            .clone())
    }

    async fn since(&self, seq: u64) -> Result<Vec<Event>, CommandError> {
        let events = self.events.lock().expect("memory log lock poisoned");
        Ok(events.iter().filter(|e| e.seq > seq).cloned().collect())
    }
}

/// What frontends and tools need from the company.
#[async_trait]
pub trait CompanyHandle: Send + Sync {
    /// Validate and commit a command; returns the committed events.
    async fn send(&self, actor: Actor, cmd: Command) -> Result<Vec<Event>, CommandError>;
    /// Live events committed after this call.
    fn subscribe(&self) -> broadcast::Receiver<Event>;
    /// A copy of the current projection.
    async fn snapshot(&self) -> State;
    /// Persisted events with `seq` greater than the argument (for replay/catch-up).
    async fn events_since(&self, seq: u64) -> Result<Vec<Event>, CommandError>;
    /// The role catalog.
    fn roles(&self) -> Vec<Role>;
    /// Who may hire and the team size cap.
    fn team_policy(&self) -> TeamConfig {
        TeamConfig::default()
    }
    /// A copy of the engine, for read-only digests that need its roles and workflow.
    /// The default rebuilds one from the snapshot and the default workflow.
    async fn engine_snapshot(&self) -> Engine {
        let state = self.snapshot().await;
        let mut engine = Engine::new(
            state.company.clone(),
            RoleCatalog::new(self.roles()),
            Workflow::default(),
        )
        .with_team(self.team_policy());
        engine.state = state;
        engine
    }
}

pub struct Hub {
    engine: Mutex<Engine>,
    log: Arc<dyn EventLog>,
    tx: broadcast::Sender<Event>,
    /// Copy of the engine's catalog for the sync [`CompanyHandle::roles`]; refreshed
    /// when a role is adopted.
    roles: RwLock<Vec<Role>>,
    team: TeamConfig,
}

impl Hub {
    /// Build a hub and rebuild state by replaying the log.
    ///
    /// Roles the engine was built with (built-ins and `.chillax/roles/` files) win over
    /// roles adopted by earlier events, so hand edits to a role file take effect on the
    /// next start. Adopted roles whose file is gone still come back from the log.
    pub async fn open(mut engine: Engine, log: Arc<dyn EventLog>) -> Result<Self, CommandError> {
        let on_disk: Vec<Role> = engine.roles.iter().cloned().collect();
        for event in log.load_all().await? {
            engine.apply(&event);
        }
        engine.roles.extend(on_disk);
        let (tx, _) = broadcast::channel(4096);
        let roles = RwLock::new(engine.roles.iter().cloned().collect());
        let team = engine.team.clone();
        Ok(Self {
            engine: Mutex::new(engine),
            log,
            tx,
            roles,
            team,
        })
    }

    /// A copy of the engine: state plus the roles and workflow its rules need. The
    /// dispatcher decides what to do next from this.
    pub async fn engine(&self) -> Engine {
        self.engine.lock().await.clone()
    }

    /// Commit runtime observations (agent output, permission requests). The closure
    /// sees the current state under the lock, so it can allocate ids safely.
    pub async fn record_with(
        &self,
        actor: Actor,
        build: impl FnOnce(&State) -> Vec<EventKind> + Send,
    ) -> Result<Vec<Event>, CommandError> {
        let mut engine = self.engine.lock().await;
        let kinds = build(&engine.state);
        self.commit(&mut engine, actor, kinds).await
    }

    pub async fn record(&self, actor: Actor, kind: EventKind) -> Result<Vec<Event>, CommandError> {
        self.record_with(actor, |_| vec![kind]).await
    }

    async fn commit(
        &self,
        engine: &mut Engine,
        actor: Actor,
        kinds: Vec<EventKind>,
    ) -> Result<Vec<Event>, CommandError> {
        let ts = Utc::now();
        let first = engine.state.last_seq + 1;
        let events: Vec<Event> = kinds
            .into_iter()
            .enumerate()
            .map(|(i, kind)| Event {
                seq: first + i as u64,
                ts,
                actor: actor.clone(),
                kind,
            })
            .collect();
        if events.is_empty() {
            return Ok(events);
        }
        self.log.append(&events).await?;
        let mut roles_changed = false;
        for event in &events {
            engine.apply(event);
            roles_changed |= matches!(event.kind, EventKind::RoleAdopted { .. });
        }
        // Update the role copy before broadcasting, so listeners that react to
        // `role_adopted` already see the new role.
        if roles_changed {
            *self.roles.write().expect("roles lock poisoned") =
                engine.roles.iter().cloned().collect();
        }
        for event in &events {
            // No receivers is fine: nobody is watching yet.
            let _ = self.tx.send(event.clone());
        }
        Ok(events)
    }
}

#[async_trait]
impl CompanyHandle for Hub {
    async fn send(&self, actor: Actor, cmd: Command) -> Result<Vec<Event>, CommandError> {
        let mut engine = self.engine.lock().await;
        let kinds = engine.decide(&actor, cmd)?;
        self.commit(&mut engine, actor, kinds).await
    }

    fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    async fn snapshot(&self) -> State {
        self.engine.lock().await.state.clone()
    }

    async fn events_since(&self, seq: u64) -> Result<Vec<Event>, CommandError> {
        self.log.since(seq).await
    }

    fn roles(&self) -> Vec<Role> {
        self.roles.read().expect("roles lock poisoned").clone()
    }

    fn team_policy(&self) -> TeamConfig {
        self.team.clone()
    }

    async fn engine_snapshot(&self) -> Engine {
        self.engine().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::role::RoleCatalog;
    use crate::workflow::Workflow;

    fn engine() -> Engine {
        let pm = Role::parse("---\nid: pm\ntitle: PM\n---\nPM\n").expect("valid role");
        Engine::new("Acme", RoleCatalog::new([pm]), Workflow::default())
    }

    fn hire() -> Command {
        Command::HireEmployee {
            role: "pm".into(),
            name: Some("Pat".into()),
            backend: None,
            model: None,
            effort: None,
            keep_defaults: false,
        }
    }

    #[tokio::test]
    async fn commits_persist_broadcast_and_replay() {
        let log = Arc::new(MemoryLog::default());
        let hub = Hub::open(engine(), log.clone()).await.expect("opens");
        let mut rx = hub.subscribe();

        let events = hub
            .send(Actor::Stakeholder, hire())
            .await
            .expect("accepted");
        assert_eq!(events[0].seq, 1);
        assert_eq!(rx.recv().await.expect("broadcast").seq, 1);

        hub.record(
            Actor::Employee("pat".into()),
            EventKind::ProgressReported {
                employee: "pat".into(),
                summary: "planning".into(),
            },
        )
        .await
        .expect("recorded");
        assert_eq!(hub.events_since(1).await.expect("reads").len(), 1);

        // A new hub over the same log rebuilds the same state.
        let again = Hub::open(engine(), log).await.expect("reopens");
        assert_eq!(again.snapshot().await, hub.snapshot().await);
        assert_eq!(again.snapshot().await.last_seq, 2);
    }

    #[tokio::test]
    async fn a_teammate_hire_is_logged_with_the_employee_as_actor() {
        let dev = Role::parse("---\nid: dev\ntitle: Dev\n---\nDev\n").expect("valid role");
        let mut engine = engine().with_team(TeamConfig {
            hiring_roles: vec!["pm".into()],
            ..TeamConfig::default()
        });
        engine.roles.insert(dev);
        let log = Arc::new(MemoryLog::default());
        let hub = Hub::open(engine, log.clone()).await.expect("opens");
        assert_eq!(hub.team_policy().hiring_roles, ["pm"]);
        hub.send(Actor::Stakeholder, hire()).await.expect("hired");
        let events = hub
            .send(
                Actor::Employee("pat".into()),
                Command::HireTeammate {
                    role: "dev".into(),
                    name: None,
                    reason: "T-1".into(),
                },
            )
            .await
            .expect("accepted");
        assert_eq!(events.len(), 2);
        let stored = log.since(1).await.expect("reads");
        assert!(
            stored
                .iter()
                .all(|e| e.actor == Actor::Employee("pat".into()))
        );
    }

    #[tokio::test]
    async fn stopping_stale_agents_survives_a_reopen() {
        let log = Arc::new(MemoryLog::default());
        let hub = Hub::open(engine(), log.clone()).await.expect("opens");
        hub.send(Actor::Stakeholder, hire()).await.expect("hired");
        for kind in [
            EventKind::AgentStarted {
                employee: "pat".into(),
                agent: None,
            },
            EventKind::AgentMessageChunk {
                employee: "pat".into(),
                text: "hi".into(),
            },
        ] {
            hub.record(Actor::System, kind).await.expect("recorded");
        }
        let again = Hub::open(engine(), log.clone()).await.expect("reopens");
        let stale = again.snapshot().await.stale_agents();
        assert_eq!(stale, ["pat"]);
        for employee in stale {
            again
                .record(Actor::System, EventKind::AgentStopped { employee })
                .await
                .expect("recorded");
        }
        let third = Hub::open(engine(), log).await.expect("reopens again");
        let state = third.snapshot().await;
        assert!(state.stale_agents().is_empty());
        let pat = state.employee("pat").expect("pat");
        assert_eq!(pat.status, crate::model::EmployeeStatus::Offline);
        assert!(!pat.turn_open);
    }

    fn adopt(id: &str, prompt: &str, overwrite: bool) -> Command {
        let mut role = Role::parse(&format!("---\nid: {id}\ntitle: Adopted\n---\n{prompt}\n"))
            .expect("valid role");
        role.base_role = Some("pm".into());
        role.source = Some(crate::role::RoleSource {
            catalog: "agency".into(),
            repo: "r".into(),
            commit: "c".into(),
            path: format!("eng/{id}.md"),
            blob: "b".into(),
            license: None,
            edited: false,
            copy_of: None,
        });
        Command::AdoptRole {
            markdown: role.to_markdown().expect("serialises"),
            revisions: vec![],
            overwrite,
        }
    }

    #[tokio::test]
    async fn adopted_roles_are_live_and_replayed_but_files_win() {
        let log = Arc::new(MemoryLog::default());
        let hub = Hub::open(engine(), log.clone()).await.expect("opens");
        hub.send(Actor::Stakeholder, adopt("be", "v1", false))
            .await
            .expect("adopted");
        assert!(hub.roles().iter().any(|r| r.id == "be"));

        // Same id again needs overwrite; hand-written roles are never replaced.
        assert!(
            hub.send(Actor::Stakeholder, adopt("be", "v2", false))
                .await
                .is_err()
        );
        assert!(
            hub.send(Actor::Stakeholder, adopt("pm", "x", true))
                .await
                .is_err()
        );
        hub.send(Actor::Stakeholder, adopt("be", "v2", true))
            .await
            .expect("overwritten");
        let prompt = |hub: &Hub| {
            hub.roles()
                .into_iter()
                .find(|r| r.id == "be")
                .map(|r| r.prompt)
        };
        assert_eq!(prompt(&hub).as_deref(), Some("v2"));

        // Replay restores the adopted role when no file exists…
        let again = Hub::open(engine(), log.clone()).await.expect("reopens");
        assert_eq!(prompt(&again).as_deref(), Some("v2"));

        // …but a role file on disk (hand-edited) wins over old events.
        let mut on_disk = engine();
        let mut edited = Role::parse("---\nid: be\ntitle: Adopted\n---\nedited\n").expect("role");
        edited.source = Some(crate::role::RoleSource {
            catalog: "agency".into(),
            repo: "r".into(),
            commit: "c".into(),
            path: "eng/be.md".into(),
            blob: "b".into(),
            license: None,
            edited: true,
            copy_of: None,
        });
        on_disk.roles.insert(edited);
        let again = Hub::open(on_disk, log).await.expect("reopens");
        assert_eq!(prompt(&again).as_deref(), Some("edited"));
    }

    #[tokio::test]
    async fn rejected_commands_leave_no_trace() {
        let log = Arc::new(MemoryLog::default());
        let hub = Hub::open(engine(), log.clone()).await.expect("opens");
        let err = hub
            .send(Actor::System, Command::ApproveSprint { id: "SP-9".into() })
            .await;
        assert!(err.is_err());
        assert!(log.load_all().await.expect("reads").is_empty());
    }
}
