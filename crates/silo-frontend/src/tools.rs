//! Tool definitions contributed by the frontends, and parsing of their
//! call inputs.

use serde_json::{json, Value};

use silo_core::event::UserQuestion;
use silo_core::tool::{ToolAvailability, ToolDef};

pub(crate) fn ask_user_question_def() -> ToolDef {
    ToolDef {
        name: "AskUserQuestion".into(),
        description: "Ask the user a question and wait for the answer. The question is \
                      shown on every connected client; the first answer received is \
                      returned."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question to show the user."
                },
                "options": {
                    "type": "array",
                    "description": "Suggested answers the user can pick from.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "label": {"type": "string"},
                            "description": {"type": "string"}
                        },
                        "required": ["label"]
                    }
                },
                "multi_select": {
                    "type": "boolean",
                    "description": "Whether the user may pick more than one option."
                },
                "allow_free_text": {
                    "type": "boolean",
                    "description": "Whether the user may answer with free text instead of picking an option."
                }
            },
            "required": ["question"]
        }),
        availability: ToolAvailability::TopLevelOnly,
    }
}

pub(crate) fn send_user_file_def() -> ToolDef {
    ToolDef {
        name: "SendUserFile".into(),
        description: "Send a file from the workspace to the user. The file is shown on \
                      every connected client."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Workspace path of the file to send."
                },
                "caption": {
                    "type": "string",
                    "description": "Optional short caption shown with the file."
                }
            },
            "required": ["path"]
        }),
        availability: ToolAvailability::TopLevelOnly,
    }
}

pub(crate) fn exit_def() -> ToolDef {
    ToolDef {
        name: "Exit".into(),
        description: "End the session. Call this when the task is complete, with a final \
                      report message for the user."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "Final report shown to the user."
                }
            },
            "required": ["message"]
        }),
        availability: ToolAvailability::TopLevelOnly,
    }
}

/// Parses AskUserQuestion input into a [`UserQuestion`].
pub(crate) fn parse_question(input: &Value) -> Result<UserQuestion, String> {
    serde_json::from_value(input.clone()).map_err(|e| format!("invalid AskUserQuestion input: {e}"))
}

pub(crate) struct FileToSend {
    pub name: String,
    pub content_b64: String,
}

/// Parses SendUserFile input. The harness injects `content_b64` (the file
/// bytes read via the sandbox) before forwarding the call.
pub(crate) fn parse_send_user_file(input: &Value) -> Result<FileToSend, String> {
    let path = input
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| "SendUserFile requires a 'path' string".to_string())?;
    let name = std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| format!("the path {path:?} has no file name"))?;
    let content_b64 = input
        .get("content_b64")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            "SendUserFile input is missing 'content_b64' (the harness injects the file content)"
                .to_string()
        })?
        .to_string();
    Ok(FileToSend { name, content_b64 })
}

/// Parses Exit input, returning the required report message.
pub(crate) fn parse_exit_message(input: &Value) -> Result<String, String> {
    input
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "the Exit tool requires a 'message' string".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_defs_are_top_level_only_with_expected_names() {
        for (def, name) in [
            (ask_user_question_def(), "AskUserQuestion"),
            (send_user_file_def(), "SendUserFile"),
            (exit_def(), "Exit"),
        ] {
            assert_eq!(def.name, name);
            assert_eq!(def.availability, ToolAvailability::TopLevelOnly);
            assert_eq!(def.input_schema["type"], "object");
        }
        assert_eq!(
            ask_user_question_def().input_schema["required"],
            json!(["question"])
        );
        assert_eq!(
            send_user_file_def().input_schema["required"],
            json!(["path"])
        );
        assert_eq!(exit_def().input_schema["required"], json!(["message"]));
    }

    #[test]
    fn question_parsing_accepts_full_and_minimal_inputs() {
        let full = parse_question(&json!({
            "question": "Which color?",
            "options": [
                {"label": "red", "description": "warm"},
                {"label": "blue"}
            ],
            "multi_select": true,
            "allow_free_text": true
        }))
        .unwrap();
        assert_eq!(full.question, "Which color?");
        assert_eq!(full.options.len(), 2);
        assert_eq!(full.options[1].description, "");
        assert!(full.multi_select);
        assert!(full.allow_free_text);

        let minimal = parse_question(&json!({"question": "Proceed?"})).unwrap();
        assert!(minimal.options.is_empty());
        assert!(!minimal.multi_select);
        assert!(!minimal.allow_free_text);

        assert!(parse_question(&json!({"options": []})).is_err());
    }

    #[test]
    fn send_user_file_parsing_extracts_the_file_name() {
        let file = parse_send_user_file(&json!({
            "path": "reports/summary.pdf",
            "content_b64": "aGk="
        }))
        .unwrap();
        assert_eq!(file.name, "summary.pdf");
        assert_eq!(file.content_b64, "aGk=");

        assert!(parse_send_user_file(&json!({"path": "x.txt"})).is_err());
        assert!(parse_send_user_file(&json!({"content_b64": "aGk="})).is_err());
        assert!(parse_send_user_file(&json!({"path": "..", "content_b64": "aGk="})).is_err());
    }

    #[test]
    fn exit_parsing_requires_a_message() {
        assert_eq!(
            parse_exit_message(&json!({"message": "done"})).unwrap(),
            "done"
        );
        assert!(parse_exit_message(&json!({})).is_err());
        assert!(parse_exit_message(&json!({"message": 7})).is_err());
    }
}
