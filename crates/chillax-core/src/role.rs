//! Role files: YAML frontmatter + a markdown system prompt.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Role {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default = "one")]
    pub max_parallel_tasks: u32,
    /// Free-form tags workflow owners can refer to, e.g. `developer`.
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub owns_states: Vec<String>,
    /// Chillax MCP tools this role may call.
    #[serde(default)]
    pub mcp_tools: Vec<String>,
    #[serde(default)]
    pub permissions: Permissions,
    /// The trusted role whose policy this role inherits (set for catalog personas,
    /// whose upstream text never grants tools or permissions; ADR 0003).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_role: Option<String>,
    /// Where the persona text came from, when it was adopted from a catalog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<RoleSource>,
    /// The system prompt (markdown body after the frontmatter).
    #[serde(skip)]
    pub prompt: String,
}

fn one() -> u32 {
    1
}

/// Which ACP permission requests the role may approve on its own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Permissions {
    #[serde(default)]
    pub auto_approve: Vec<String>,
    #[serde(default)]
    pub escalate: Vec<String>,
}

/// Provenance of a role adopted from a persona catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleSource {
    /// Catalog name from `[catalog]`, e.g. `agency-agents`.
    pub catalog: String,
    pub repo: String,
    /// Commit the persona was read at.
    pub commit: String,
    /// Path of the persona file inside the catalog repo.
    pub path: String,
    /// Git blob id of the upstream file, so the original text can be verified.
    pub blob: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// Whether the stakeholder had the persona text revised before adopting it.
    #[serde(default)]
    pub edited: bool,
    /// Set when the role is a copy of another role (ADR 0003 amendment); the other
    /// fields are then empty. A copy is never a trusted base.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_of: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum RoleError {
    #[error("role file must start with `---` frontmatter")]
    MissingFrontmatter,
    #[error("invalid role frontmatter: {0}")]
    Frontmatter(#[from] serde_yaml::Error),
}

impl Role {
    /// Parse a role file: `---\n<yaml>\n---\n<prompt>`.
    pub fn parse(source: &str) -> Result<Self, RoleError> {
        let source = source.trim_start_matches('\u{feff}');
        let rest = source
            .strip_prefix("---")
            .ok_or(RoleError::MissingFrontmatter)?;
        let end = rest.find("\n---").ok_or(RoleError::MissingFrontmatter)?;
        let (yaml, body) = rest.split_at(end);
        let body = body.trim_start_matches("\n---");
        let mut role: Role = serde_yaml::from_str(yaml)?;
        role.prompt = body.trim().to_string();
        Ok(role)
    }

    /// Serialise back to the role file format (frontmatter + prompt).
    pub fn to_markdown(&self) -> Result<String, RoleError> {
        let yaml = serde_yaml::to_string(self)?;
        Ok(format!("---\n{yaml}---\n\n{}\n", self.prompt.trim()))
    }

    pub fn may_use_tool(&self, tool: &str) -> bool {
        self.mcp_tools.iter().any(|t| t == tool)
    }

    /// Whether the role's policy auto-approves an ACP tool kind (`read`, `edit`,
    /// `execute`, `fetch`, …). Policy names map onto ACP kinds; `run_tests` is the
    /// friendly name for `execute`.
    pub fn auto_approves(&self, acp_kind: &str) -> bool {
        self.permissions.auto_approve.iter().any(|p| {
            let p = p.as_str();
            p == acp_kind
                || matches!(
                    (p, acp_kind),
                    ("run_tests", "execute") | ("network", "fetch") | ("delete", "delete")
                )
        })
    }

    /// The prompt with `{{company}}`, `{{name}}` and `{{role}}` filled in.
    pub fn render_prompt(&self, company: &str, name: &str) -> String {
        self.prompt
            .replace("{{company}}", company)
            .replace("{{name}}", name)
            .replace("{{role}}", &self.title)
    }
}

/// Built-in roles plus project overrides (same `id` replaces, new ids add).
#[derive(Debug, Clone, Default)]
pub struct RoleCatalog {
    roles: BTreeMap<String, Role>,
}

impl RoleCatalog {
    pub fn new(roles: impl IntoIterator<Item = Role>) -> Self {
        let mut catalog = Self::default();
        for role in roles {
            catalog.insert(role);
        }
        catalog
    }

    pub fn insert(&mut self, role: Role) {
        self.roles.insert(role.id.clone(), role);
    }

    pub fn get(&self, id: &str) -> Option<&Role> {
        self.roles.get(id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Role> {
        self.roles.values()
    }
}

impl Extend<Role> for RoleCatalog {
    fn extend<I: IntoIterator<Item = Role>>(&mut self, roles: I) {
        for role in roles {
            self.insert(role);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PM: &str = "---\nid: project-manager\ntitle: Project Manager\nmcp_tools: [create_epic, create_story]\npermissions:\n  auto_approve: [read, run_tests]\n---\n\nYou are {{name}}, PM at {{company}}.\n";

    #[test]
    fn parses_frontmatter_and_body() {
        let role = Role::parse(PM).expect("valid role");
        assert_eq!(role.id, "project-manager");
        assert_eq!(role.max_parallel_tasks, 1);
        assert!(role.may_use_tool("create_epic"));
        assert!(!role.may_use_tool("assign"));
        assert_eq!(
            role.render_prompt("Acme", "Pat"),
            "You are Pat, PM at Acme."
        );
    }

    #[test]
    fn policy_maps_friendly_names_to_acp_kinds() {
        let role = Role::parse(PM).expect("valid role");
        assert!(role.auto_approves("read"));
        assert!(role.auto_approves("execute"));
        assert!(!role.auto_approves("edit"));
    }

    #[test]
    fn markdown_round_trips_with_source() {
        let mut role = Role::parse(PM).expect("valid role");
        role.base_role = Some("project-manager".into());
        role.source = Some(RoleSource {
            catalog: "agency-agents".into(),
            repo: "https://example.com/agents.git".into(),
            commit: "abc123".into(),
            path: "pm/senior.md".into(),
            blob: "def456".into(),
            license: Some("MIT".into()),
            edited: true,
            copy_of: None,
        });
        let back = Role::parse(&role.to_markdown().expect("serialises")).expect("parses");
        assert_eq!(back, role);
    }

    #[test]
    fn missing_frontmatter_is_an_error() {
        assert!(matches!(
            Role::parse("just text"),
            Err(RoleError::MissingFrontmatter)
        ));
    }

    #[test]
    fn overrides_replace_by_id() {
        let mut catalog = RoleCatalog::new([Role::parse(PM).expect("valid role")]);
        let mut custom = Role::parse(PM).expect("valid role");
        custom.title = "Product Owner".into();
        catalog.insert(custom);
        assert_eq!(catalog.iter().count(), 1);
        assert_eq!(
            catalog.get("project-manager").map(|r| r.title.as_str()),
            Some("Product Owner")
        );
    }
}
