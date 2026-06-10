//! Tool definitions and the registry the harness uses to route tool calls.

use serde::{Deserialize, Serialize};

use crate::conversation::AgentKind;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolAvailability {
    TopLevelOnly,
    SubagentOnly,
    Both,
}

impl ToolAvailability {
    pub fn available_to(self, kind: AgentKind) -> bool {
        matches!(
            (self, kind),
            (ToolAvailability::Both, _)
                | (ToolAvailability::TopLevelOnly, AgentKind::TopLevel)
                | (ToolAvailability::SubagentOnly, AgentKind::Subagent)
        )
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool input.
    pub input_schema: serde_json::Value,
    pub availability: ToolAvailability,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider-issued tool-use id, echoed back in the tool result.
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

/// Tool-result content recorded for tool calls ended by a user interrupt:
/// cancelled calls and questions resolved by an interrupt.
pub const INTERRUPTED_BY_USER: &str = "[interrupted by the user]";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        ToolOutput {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        ToolOutput {
            content: content.into(),
            is_error: true,
        }
    }
}

/// Which component executes a given tool. The harness routes tool calls to
/// the owning component.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOwner {
    /// Read/Write/Edit, Bash, WebFetch, WebSearch — executed by the
    /// sandboxed helper process.
    Sandbox,
    /// AskUserQuestion, SendUserFile, Exit — executed by the frontend.
    Frontend,
    /// Agent — executed by the harness itself (spawns a subagent).
    Harness,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RegisteredTool {
    pub def: ToolDef,
    pub owner: ToolOwner,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolRegistry {
    entries: Vec<RegisteredTool>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, def: ToolDef, owner: ToolOwner) {
        self.entries.push(RegisteredTool { def, owner });
    }

    pub fn add_all(&mut self, defs: Vec<ToolDef>, owner: ToolOwner) {
        for def in defs {
            self.add(def, owner);
        }
    }

    pub fn entries(&self) -> &[RegisteredTool] {
        &self.entries
    }

    /// Tool definitions visible to an agent of the given kind.
    pub fn defs_for(&self, kind: AgentKind) -> Vec<ToolDef> {
        self.entries
            .iter()
            .filter(|e| e.def.availability.available_to(kind))
            .map(|e| e.def.clone())
            .collect()
    }

    pub fn owner_of(&self, name: &str) -> Option<ToolOwner> {
        self.entries
            .iter()
            .find(|e| e.def.name == name)
            .map(|e| e.owner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn def(name: &str, availability: ToolAvailability) -> ToolDef {
        ToolDef {
            name: name.to_string(),
            description: String::new(),
            input_schema: json!({"type": "object"}),
            availability,
        }
    }

    #[test]
    fn registry_filters_by_agent_kind() {
        let mut reg = ToolRegistry::new();
        reg.add(
            def("Exit", ToolAvailability::TopLevelOnly),
            ToolOwner::Frontend,
        );
        reg.add(def("Bash", ToolAvailability::Both), ToolOwner::Sandbox);

        let top = reg.defs_for(AgentKind::TopLevel);
        assert_eq!(top.len(), 2);
        let sub = reg.defs_for(AgentKind::Subagent);
        assert_eq!(sub.len(), 1);
        assert_eq!(sub[0].name, "Bash");
        assert_eq!(reg.owner_of("Bash"), Some(ToolOwner::Sandbox));
        assert_eq!(reg.owner_of("nope"), None);
    }
}
