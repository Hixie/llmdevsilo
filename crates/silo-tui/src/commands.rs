//! Slash-command parsing for the input line.
//!
//! A line starting with `/` is a client command rather than a prompt. The
//! parser returns `None` for ordinary prompt text, `Some(Ok(_))` for a
//! recognized command, and `Some(Err(_))` with a user-facing message for a
//! malformed or unknown command.

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SlashCommand {
    /// Show the sandbox access report in a popup.
    Access,
    /// Request and show the cost report in a popup.
    Cost,
    /// Upload a local file to the harness.
    Upload { path: String },
    /// Request a pairing code for connecting another device.
    Pair,
    /// Close this client.
    Quit,
    /// Ask the harness to shut down.
    Shutdown,
}

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
    let no_args = |command: SlashCommand| {
        if rest.is_empty() {
            Ok(command)
        } else {
            Err(format!("{name} takes no argument"))
        }
    };
    let result = match name.as_str() {
        "/access" => no_args(SlashCommand::Access),
        "/cost" => no_args(SlashCommand::Cost),
        "/pair" => no_args(SlashCommand::Pair),
        "/quit" => no_args(SlashCommand::Quit),
        "/shutdown" => no_args(SlashCommand::Shutdown),
        "/upload" => {
            if rest.is_empty() {
                Err("usage: /upload <path>".to_string())
            } else {
                Ok(SlashCommand::Upload {
                    path: rest.to_string(),
                })
            }
        }
        _ => Err(format!(
            "unknown command {name} (try /access, /cost, /upload, /pair, /quit, /shutdown)"
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
        assert_eq!(parse("/access"), Some(Ok(SlashCommand::Access)));
        assert_eq!(parse("/cost"), Some(Ok(SlashCommand::Cost)));
        assert_eq!(parse("/pair"), Some(Ok(SlashCommand::Pair)));
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
    }

    #[test]
    fn unknown_commands_are_errors() {
        let result = parse("/frobnicate").unwrap();
        let message = result.unwrap_err();
        assert!(message.contains("/frobnicate"));
    }
}
