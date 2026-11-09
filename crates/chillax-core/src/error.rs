use thiserror::Error;

/// Why a command was rejected, or why the hub could not commit its events.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CommandError {
    #[error("unknown role `{0}`")]
    UnknownRole(String),
    #[error("unknown employee `{0}`")]
    UnknownEmployee(String),
    #[error("unknown ticket `{0}`")]
    UnknownTicket(String),
    #[error("unknown epic `{0}`")]
    UnknownEpic(String),
    #[error("unknown sprint `{0}`")]
    UnknownSprint(String),
    #[error("unknown inbox item `{0}`")]
    UnknownInboxItem(String),
    #[error("unknown workflow state `{0}`")]
    UnknownState(String),
    #[error("transition {from} → {to} is not allowed by the workflow")]
    TransitionNotAllowed { from: String, to: String },
    #[error("{from} → {to} needs {}", missing.join(", "))]
    RequirementsUnmet {
        from: String,
        to: String,
        missing: Vec<String>,
    },
    #[error("only the stakeholder can {0}")]
    StakeholderOnly(&'static str),
    #[error("only an employee can {0}")]
    EmployeeOnly(&'static str),
    #[error("invalid command: {0}")]
    Invalid(String),
    #[error("event log error: {0}")]
    Storage(String),
}
