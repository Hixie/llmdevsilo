//! Slash-command parsing for the input line.
//!
//! A line starting with `/` is a client command rather than a prompt. The
//! parser returns `None` for ordinary prompt text, `Some(Ok(_))` for a
//! recognized command, and `Some(Err(_))` with a user-facing message for a
//! malformed or unknown command.
//!
//! [`COMMANDS`] is the single source of truth: the parser matches against
//! it and the /help popup renders it, so the two cannot drift apart.

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SlashCommand {
    /// Show the command list in a popup.
    Help,
    /// Show the sandbox access report in a popup.
    Access,
    /// Request and show the cost report in a popup.
    Cost,
    /// Upload a local file to the harness.
    Upload { path: String },
    /// Request a pairing code for connecting another device.
    Pair,
    /// Interrupt the model's current turn.
    Stop,
    /// Toggle the per-session debug mode (raw ids in the UI).
    Debug,
    /// Close this client.
    Quit,
    /// Ask the harness to shut down.
    Shutdown,
}

/// One entry of the command table: the name the parser matches, the usage
/// line and description shown by /help, and the builder that turns the
/// argument text into a [`SlashCommand`].
pub struct CommandSpec {
    pub name: &'static str,
    pub usage: &'static str,
    pub description: &'static str,
    build: fn(rest: &str) -> Result<SlashCommand, String>,
}

fn no_argument(
    name: &'static str,
    rest: &str,
    command: SlashCommand,
) -> Result<SlashCommand, String> {
    if rest.is_empty() {
        Ok(command)
    } else {
        Err(format!("{name} takes no argument"))
    }
}

/// Every slash command the client understands, in /help display order.
pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "/help",
        usage: "/help",
        description: "list the available commands",
        build: |rest| no_argument("/help", rest, SlashCommand::Help),
    },
    CommandSpec {
        name: "/access",
        usage: "/access",
        description: "show what the sandbox can reach",
        build: |rest| no_argument("/access", rest, SlashCommand::Access),
    },
    CommandSpec {
        name: "/cost",
        usage: "/cost",
        description: "show token and dollar usage",
        build: |rest| no_argument("/cost", rest, SlashCommand::Cost),
    },
    CommandSpec {
        name: "/upload",
        usage: "/upload <path>",
        description: "send a local file to the workspace",
        build: |rest| {
            if rest.is_empty() {
                Err("usage: /upload <path>".to_string())
            } else {
                Ok(SlashCommand::Upload {
                    path: rest.to_string(),
                })
            }
        },
    },
    CommandSpec {
        name: "/pair",
        usage: "/pair",
        description: "issue a pairing code for another device",
        build: |rest| no_argument("/pair", rest, SlashCommand::Pair),
    },
    CommandSpec {
        name: "/stop",
        usage: "/stop",
        description: "interrupt the current turn",
        build: |rest| no_argument("/stop", rest, SlashCommand::Stop),
    },
    CommandSpec {
        name: "/debug",
        usage: "/debug",
        description: "toggle raw ids in the display (this session only)",
        build: |rest| no_argument("/debug", rest, SlashCommand::Debug),
    },
    CommandSpec {
        name: "/shutdown",
        usage: "/shutdown",
        description: "ask the harness to shut down",
        build: |rest| no_argument("/shutdown", rest, SlashCommand::Shutdown),
    },
    CommandSpec {
        name: "/quit",
        usage: "/quit",
        description: "close this client (the harness keeps running)",
        build: |rest| no_argument("/quit", rest, SlashCommand::Quit),
    },
];

/// Parses one input line. Command names are matched case-insensitively.
pub fn parse(line: &str) -> Option<Result<SlashCommand, String>> {
    let trimmed = line.trim();
    if !trimmed.starts_with('/') {
        return None;
    }
    let (word, rest) = match trimmed.split_once(char::is_whitespace) {
        Some((word, rest)) => (word, rest.trim()),
        None => (trimmed, ""),
    };
    let name = word.to_ascii_lowercase();
    let result = match COMMANDS.iter().find(|spec| spec.name == name) {
        Some(spec) => (spec.build)(rest),
        None => Err(format!(
            "unknown command {name}; /help lists the available commands"
        )),
    };
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_not_a_command() {
        assert_eq!(parse("hello world"), None);
        assert_eq!(parse(""), None);
        assert_eq!(parse("  spaced  "), None);
        assert_eq!(parse("a /quit in the middle"), None);
    }

    #[test]
    fn simple_commands_parse() {
        assert_eq!(parse("/help"), Some(Ok(SlashCommand::Help)));
        assert_eq!(parse("/access"), Some(Ok(SlashCommand::Access)));
        assert_eq!(parse("/cost"), Some(Ok(SlashCommand::Cost)));
        assert_eq!(parse("/pair"), Some(Ok(SlashCommand::Pair)));
        assert_eq!(parse("/stop"), Some(Ok(SlashCommand::Stop)));
        assert_eq!(parse("/debug"), Some(Ok(SlashCommand::Debug)));
        assert_eq!(parse("/quit"), Some(Ok(SlashCommand::Quit)));
        assert_eq!(parse("/shutdown"), Some(Ok(SlashCommand::Shutdown)));
    }

    #[test]
    fn commands_are_case_insensitive_and_trimmed() {
        assert_eq!(parse("  /QUIT  "), Some(Ok(SlashCommand::Quit)));
        assert_eq!(parse("/Access"), Some(Ok(SlashCommand::Access)));
    }

    #[test]
    fn upload_takes_the_rest_of_the_line_as_a_path() {
        assert_eq!(
            parse("/upload notes.txt"),
            Some(Ok(SlashCommand::Upload {
                path: "notes.txt".into()
            }))
        );
        assert_eq!(
            parse("/upload /tmp/my file.txt "),
            Some(Ok(SlashCommand::Upload {
                path: "/tmp/my file.txt".into()
            }))
        );
    }

    #[test]
    fn upload_without_a_path_is_an_error() {
        assert!(matches!(parse("/upload"), Some(Err(_))));
        assert!(matches!(parse("/upload   "), Some(Err(_))));
    }

    #[test]
    fn argument_after_a_no_argument_command_is_an_error() {
        assert!(matches!(parse("/quit now"), Some(Err(_))));
        assert!(matches!(parse("/cost all"), Some(Err(_))));
        assert!(matches!(parse("/stop now"), Some(Err(_))));
        assert!(matches!(parse("/debug on"), Some(Err(_))));
    }

    #[test]
    fn unknown_commands_point_at_help() {
        let result = parse("/frobnicate").unwrap();
        let message = result.unwrap_err();
        assert!(message.contains("/frobnicate"));
        assert!(message.contains("/help"));
    }

    #[test]
    fn the_command_table_covers_exactly_the_parsed_set() {
        // The required command set; changing the table must update this
        // list deliberately.
        let mut expected = vec![
            "/help",
            "/access",
            "/cost",
            "/upload",
            "/pair",
            "/stop",
            "/debug",
            "/shutdown",
            "/quit",
        ];
        let mut names: Vec<&str> = COMMANDS.iter().map(|spec| spec.name).collect();
        expected.sort_unstable();
        names.sort_unstable();
        assert_eq!(names, expected);

        for spec in COMMANDS {
            // Every table entry parses through its own usage line (the
            // placeholder in "/upload <path>" doubles as a sample
            // argument), so the help content and the parser cannot drift.
            assert!(
                matches!(parse(spec.usage), Some(Ok(_))),
                "{} does not parse",
                spec.usage
            );
            assert!(spec.usage.starts_with(spec.name));
            assert!(!spec.description.is_empty());
        }
    }
}
