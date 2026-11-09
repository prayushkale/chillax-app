//! Chillax domain core.
//!
//! Frontends and agents send [`Command`]s; the [`Engine`] validates them against the
//! current [`State`] and turns them into [`Event`]s, which are persisted and broadcast by
//! the [`Hub`]. This crate has no UI, HTTP, SQL or process-spawning dependencies
//! (AGENTS.md rule 1).

pub mod agents;
pub mod apps;
pub mod attachment;
pub mod checkin;
pub mod command;
pub mod config;
pub mod dispatch;
pub mod engine;
pub mod env;
pub mod error;
pub mod event;
pub mod hub;
pub mod instance;
pub mod intake;
pub mod limits;
pub mod model;
pub mod names;
pub mod notify;
pub mod planning;
pub mod pricing;
pub mod problems;
pub mod recruit;
pub mod release;
pub mod role;
pub mod stall;
pub mod task;
pub mod transcript;
pub mod triage;
pub mod usage;
pub mod vcs;
pub mod workflow;

pub use agents::{AgentDirectory, AgentDirectoryError, BackendInfo, ModelOption};
pub use apps::{
    AppInfo, AppSource, AppStatus, AppTarget, AppWant, ReportedApp, RunningApp, RunningAppView,
};
pub use attachment::{Attachment, AttachmentError, AttachmentStore};
pub use command::Command;
pub use config::Config;
pub use engine::Engine;
pub use env::{EnvError, EnvRun, Environment};
pub use error::CommandError;
pub use event::{Actor, Event, EventKind};
pub use hub::{CompanyHandle, EventLog, Hub, MemoryLog};
pub use instance::{InstanceInfo, InstanceSource};
pub use model::*;
pub use notify::{AgentState, AgentSummary, HireState, HireSummary, Level, Notification, Notifier};
pub use pricing::{ModelPrice, PriceTable};
pub use recruit::{
    CatalogStatus, DiffLine, DiffOp, Finding, PersonaSummary, PromptEditor, RecruitError,
    Recruiter, Revision, Rewrite, RoleDraft, Severity,
};
pub use role::{Role, RoleCatalog, RoleSource};
pub use transcript::{ToolStatus, TranscriptItem};
pub use triage::{Failure, FailureClass, TriageStatus};
pub use usage::{
    ContextFill, EmployeeUsage, UsageComment, UsageInput, UsageSample, UsageStats, UsageTotals,
    format_usage_comment,
};
pub use vcs::{FastForward, MergeOutcome, RebaseResult, VcsError, VcsHost};
pub use workflow::Workflow;

#[cfg(test)]
mod agent_limit_tests;
#[cfg(test)]
mod apps_tests;
#[cfg(test)]
mod checkin_tests;
#[cfg(test)]
mod dev_loop_tests;
#[cfg(test)]
mod problems_tests;
#[cfg(test)]
mod qa_release_tests;
#[cfg(test)]
mod reported_apps_tests;
#[cfg(test)]
mod shared_worktree_tests;
#[cfg(test)]
mod stall_tests;
#[cfg(test)]
mod suggestion_tests;
#[cfg(test)]
mod usage_tests;
#[cfg(test)]
mod ws5_tests;
#[cfg(test)]
mod ws6_tests;
