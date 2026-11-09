//! The company workflow: states, allowed transitions and gates, loaded from
//! `workflow.yaml`. Data, not code (AGENTS.md rule 6).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::error::CommandError;
use crate::role::Role;

/// States the dev loop moves tickets through. These are the defaults of the built-in
/// `workflow.yaml`; a workflow without them cannot run the loop.
pub const IN_PROGRESS: &str = "in_progress";
pub const IN_REVIEW: &str = "in_review";
pub const IN_QA: &str = "in_qa";

/// The built-in workflow, written to `.chillax/workflow.yaml` by `chillax init`.
pub const DEFAULT_WORKFLOW_YAML: &str = include_str!("../defaults/workflow.yaml");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workflow {
    pub states: Vec<StateDef>,
    pub transitions: Vec<Transition>,
    #[serde(default)]
    pub gates: BTreeMap<String, Gate>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateDef {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transition {
    pub from: String,
    pub to: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    /// Chillax drives this move itself (dispatcher, review, QA); the stakeholder is not
    /// offered it as a manual state change.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub chillax_owned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gate {
    pub approver: String,
}

impl Transition {
    /// The `requires` facts the ticket's recorded `checks` do not yet cover.
    pub fn unmet(&self, checks: &BTreeSet<String>) -> Vec<String> {
        self.requires
            .iter()
            .filter(|r| !checks.contains(*r))
            .cloned()
            .collect()
    }
}

/// Facts Chillax can record about a ticket. A transition may only `require` these:
/// anything else could never be satisfied, so `workflow.yaml` is rejected at load.
pub const KNOWN_CHECKS: [&str; 4] = [
    "tests_pass",
    "review_approved",
    "merged_to_dev",
    "qa_passed",
];

#[derive(Debug, thiserror::Error)]
pub enum WorkflowError {
    #[error("invalid workflow.yaml: {0}")]
    Parse(#[from] serde_yaml::Error),
    #[error("workflow has no states")]
    NoStates,
    #[error("transition references unknown state `{0}`")]
    UnknownState(String),
    #[error(
        "transition requires unknown check `{0}` (known: tests_pass, review_approved, merged_to_dev, qa_passed)"
    )]
    UnknownRequirement(String),
}

impl Workflow {
    pub fn from_yaml(yaml: &str) -> Result<Self, WorkflowError> {
        let workflow: Self = serde_yaml::from_str(yaml)?;
        if workflow.states.is_empty() {
            return Err(WorkflowError::NoStates);
        }
        for t in &workflow.transitions {
            for key in [&t.from, &t.to] {
                if !workflow.has_state(key) {
                    return Err(WorkflowError::UnknownState(key.clone()));
                }
            }
            if let Some(unknown) = t
                .requires
                .iter()
                .find(|r| !KNOWN_CHECKS.contains(&r.as_str()))
            {
                return Err(WorkflowError::UnknownRequirement(unknown.clone()));
            }
        }
        Ok(workflow)
    }

    /// New tickets start in the first state.
    pub fn initial_state(&self) -> &str {
        &self.states[0].key
    }

    /// The last state: finished work ends here.
    pub fn terminal_state(&self) -> &str {
        &self.states[self.states.len() - 1].key
    }

    /// The state an approved sprint moves its tickets into: the first transition target
    /// out of the initial state.
    pub fn planned_state(&self) -> Option<&str> {
        let initial = self.initial_state();
        self.transitions
            .iter()
            .find(|t| t.from == initial)
            .map(|t| t.to.as_str())
    }

    /// True when `role` works tickets in `state`: the role lists the state in
    /// `owns_states`, or the workflow names the role's id or one of its tags as owner.
    pub fn role_owns(&self, role: &Role, state: &str) -> bool {
        role.owns_states.iter().any(|s| s == state)
            || self
                .states
                .iter()
                .find(|s| s.key == state)
                .and_then(|s| s.owner.as_deref())
                .is_some_and(|owner| owner == role.id || role.tags.iter().any(|t| t == owner))
    }

    pub fn has_state(&self, key: &str) -> bool {
        self.states.iter().any(|s| s.key == key)
    }

    pub fn check_transition(&self, from: &str, to: &str) -> Result<&Transition, CommandError> {
        if !self.has_state(to) {
            return Err(CommandError::UnknownState(to.to_string()));
        }
        self.transitions
            .iter()
            .find(|t| t.from == from && t.to == to)
            .ok_or_else(|| CommandError::TransitionNotAllowed {
                from: from.to_string(),
                to: to.to_string(),
            })
    }
}

impl Default for Workflow {
    fn default() -> Self {
        Self::from_yaml(DEFAULT_WORKFLOW_YAML).expect("built-in workflow.yaml is valid")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_workflow_parses() {
        let wf = Workflow::default();
        assert_eq!(wf.initial_state(), "backlog");
        assert_eq!(wf.planned_state(), Some("ready"));
        assert!(wf.gates.contains_key("sprint_start"));
    }

    #[test]
    fn allowed_and_forbidden_transitions() {
        let wf = Workflow::default();
        assert!(wf.check_transition("backlog", "ready").is_ok());
        assert_eq!(
            wf.check_transition("backlog", "done"),
            Err(CommandError::TransitionNotAllowed {
                from: "backlog".into(),
                to: "done".into()
            })
        );
        assert_eq!(
            wf.check_transition("backlog", "nowhere"),
            Err(CommandError::UnknownState("nowhere".into()))
        );
    }

    #[test]
    fn unmet_lists_the_missing_facts() {
        let wf = Workflow::default();
        let t = wf.check_transition("in_review", "in_qa").expect("allowed");
        let mut checks = BTreeSet::new();
        assert_eq!(t.unmet(&checks), ["review_approved", "merged_to_dev"]);
        checks.insert("review_approved".to_string());
        assert_eq!(t.unmet(&checks), ["merged_to_dev"]);
        checks.insert("merged_to_dev".to_string());
        assert!(t.unmet(&checks).is_empty());
    }

    #[test]
    fn rejects_unknown_requirement() {
        let yaml =
            "states: [{key: a}, {key: b}]\ntransitions: [{from: a, to: b, requires: [vibes]}]\n";
        assert!(matches!(
            Workflow::from_yaml(yaml),
            Err(WorkflowError::UnknownRequirement(r)) if r == "vibes"
        ));
    }

    #[test]
    fn rejects_transition_to_unknown_state() {
        let yaml = "states: [{key: a}]\ntransitions: [{from: a, to: b}]\n";
        assert!(matches!(
            Workflow::from_yaml(yaml),
            Err(WorkflowError::UnknownState(s)) if s == "b"
        ));
    }
}
