//! Definitions of the sandbox-contributed tools: Read, Write, Edit, Bash,
//! WebFetch, WebSearch. All are executed by the helper process inside the
//! sandbox.

use serde_json::json;
use silo_core::tool::{ToolAvailability, ToolDef};

pub fn sandbox_tool_defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "Read".into(),
            description: "Read a file. Paths are inside the sandbox: the workspace is \
                          read/write; allowlisted host paths are read-only. Returns the file \
                          content; long files can be paged with offset/limit (in bytes)."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "offset": {"type": "integer", "minimum": 0},
                    "limit": {"type": "integer", "minimum": 1}
                },
                "required": ["path"]
            }),
            availability: ToolAvailability::Both,
        },
        ToolDef {
            name: "Write".into(),
            description: "Create or overwrite a file in the workspace or scratch space.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"},
                    "append": {"type": "boolean", "default": false}
                },
                "required": ["path", "content"]
            }),
            availability: ToolAvailability::Both,
        },
        ToolDef {
            name: "Edit".into(),
            description: "Replace an exact string in a file with another. Fails unless \
                          old_string matches exactly once (or replace_all is set)."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string"},
                    "new_string": {"type": "string"},
                    "replace_all": {"type": "boolean", "default": false}
                },
                "required": ["path", "old_string", "new_string"]
            }),
            availability: ToolAvailability::Both,
        },
        ToolDef {
            name: "Bash".into(),
            description: "Run a shell command inside the sandbox. The workspace is mounted \
                          read/write; network access goes through the egress proxy (HTTPS_PROXY \
                          is preconfigured). Returns stdout, stderr, and the exit code."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "timeout_ms": {"type": "integer", "minimum": 1, "default": 120000}
                },
                "required": ["command"]
            }),
            availability: ToolAvailability::Both,
        },
        ToolDef {
            name: "WebFetch".into(),
            description: "Fetch a URL through the egress proxy (subject to the domain \
                          allowlist) and return the response body as text."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string"},
                    "max_bytes": {"type": "integer", "minimum": 1, "default": 1048576}
                },
                "required": ["url"]
            }),
            availability: ToolAvailability::Both,
        },
        ToolDef {
            name: "WebSearch".into(),
            description: "Search the web and return result titles, URLs, and snippets. The \
                          search request goes through the egress proxy; the search engine \
                          domain must be on the allowlist."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"}
                },
                "required": ["query"]
            }),
            availability: ToolAvailability::Both,
        },
    ]
}
