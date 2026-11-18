//! The slash commands Muse accepts in a session, and the normalisation that
//! turns CLI words into the exact line the server expects.
//!
//! These are delivered as ordinary chat text: the gateway interprets a leading
//! `/name` itself. The client's job is to send the right line and to report what
//! came back, including the case where the server refuses the command — a
//! refusal is a result, not a reason to retry with a different spelling.

/// One documented command.
pub struct Command {
    pub name: &'static str,
    pub scope: &'static str,
    pub description: &'static str,
    /// Whether the command takes an argument, so `--list` can show the form.
    pub argument: Option<&'static str>,
}

/// The commands the app documents. `scope` is the app's own grouping (Session
/// versus View), kept because a View command changes what is shown rather than
/// what the conversation is.
pub const COMMANDS: &[Command] = &[
    Command {
        name: "/status",
        scope: "Session",
        description: "Show current session status",
        argument: None,
    },
    Command {
        name: "/compact",
        scope: "Session",
        description: "Compact conversation context",
        argument: None,
    },
    Command {
        name: "/clear",
        scope: "Session",
        description: "Start a fresh context on the next message",
        argument: None,
    },
    Command {
        name: "/usage",
        scope: "Session",
        description: "Toggle usage display",
        argument: Some("on|off"),
    },
    Command {
        name: "/stop",
        scope: "Session",
        description: "Stop the current response",
        argument: None,
    },
    Command {
        name: "/latest",
        scope: "Session",
        description: "Jump back to the latest transcript view",
        argument: None,
    },
    Command {
        name: "/activity",
        scope: "View",
        description: "Open the Activity panel",
        argument: None,
    },
];

/// Render the list for a terminal.
pub fn list_text() -> String {
    let width = COMMANDS
        .iter()
        .map(|c| match c.argument {
            Some(arg) => c.name.len() + 1 + arg.len(),
            None => c.name.len(),
        })
        .max()
        .unwrap_or(0);
    let mut lines = Vec::new();
    for command in COMMANDS {
        let form = match command.argument {
            Some(arg) => format!("{} {}", command.name, arg),
            None => command.name.to_string(),
        };
        lines.push(format!(
            "{:<width$}  {:<8} {}",
            form,
            command.scope,
            command.description,
            width = width
        ));
    }
    lines.join("\n")
}

/// Find a documented command by name, accepting it with or without the slash.
pub fn find(name: &str) -> Option<&'static Command> {
    let normalised = if name.starts_with('/') {
        name.to_string()
    } else {
        format!("/{name}")
    };
    COMMANDS.iter().find(|command| command.name == normalised)
}

/// Build the line that goes on the wire.
///
/// A command is a single line by definition, so embedded newlines are refused:
/// sending `/clear\nand do something else` would be two instructions wearing the
/// shape of one command.
pub fn build(name: &str, args: &[String]) -> anyhow::Result<String> {
    let name = name.trim();
    if name.is_empty() {
        anyhow::bail!("a slash command needs a name, e.g. `muse slash /status`");
    }
    let body = name.trim_start_matches('/');
    if body.is_empty() {
        anyhow::bail!("`/` alone is not a command");
    }
    if body.chars().any(char::is_whitespace) {
        anyhow::bail!(
            "the command name must be one word; put its argument after it \
             (e.g. `muse slash /usage on`)"
        );
    }
    // A command that documents an argument needs one. Sending `/usage` bare
    // reaches the server as an incomplete command and comes back as an opaque
    // HTTP 400; refusing here says which argument is missing.
    if let Some(command) = COMMANDS.iter().find(|c| c.name == format!("/{body}")) {
        if let Some(argument) = command.argument {
            if args.is_empty() {
                anyhow::bail!(
                    "`{}` needs an argument: {}",
                    command.name,
                    argument
                );
            }
        }
    }
    let line = if args.is_empty() {
        format!("/{body}")
    } else {
        format!("/{body} {}", args.join(" "))
    };
    if line.contains('\n') || line.contains('\r') {
        anyhow::bail!("a slash command must be a single line");
    }
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_documented_command_has_a_unique_name() {
        let mut names: Vec<&str> = COMMANDS.iter().map(|c| c.name).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), COMMANDS.len(), "duplicate command name");
    }

    #[test]
    fn names_start_with_a_slash() {
        for command in COMMANDS {
            assert!(command.name.starts_with('/'), "{} lacks a slash", command.name);
        }
    }

    #[test]
    fn lookup_accepts_the_name_with_or_without_the_slash() {
        assert!(find("/status").is_some());
        assert!(find("status").is_some());
        assert!(find("/nope").is_none());
        assert!(find("").is_none());
    }

    #[test]
    fn build_normalises_the_leading_slash() {
        assert_eq!(build("/status", &[]).expect("line"), "/status");
        assert_eq!(build("status", &[]).expect("line"), "/status");
        assert_eq!(
            build("usage", &["on".to_string()]).expect("line"),
            "/usage on"
        );
        assert_eq!(
            build("/usage", &["off".to_string()]).expect("line"),
            "/usage off"
        );
    }

    #[test]
    fn a_documented_argument_is_required_locally() {
        // The server rejects a bare `/usage` with an unlabelled 400, so the
        // missing argument is reported before the request is sent.
        let err = build("/usage", &[]).expect_err("should refuse");
        assert!(err.to_string().contains("on|off"), "{err}");
        // Commands without an argument stay usable bare.
        assert_eq!(build("/status", &[]).expect("line"), "/status");
    }

    #[test]
    fn build_joins_multiple_arguments_with_one_space() {
        assert_eq!(
            build("/usage", &["on".to_string(), "extra".to_string()]).expect("line"),
            "/usage on extra"
        );
    }

    #[test]
    fn build_refuses_a_command_that_would_be_two_lines() {
        // The check is on the assembled line, so an argument carrying a newline
        // is caught even though the name itself is fine.
        let err = build("/status", &["a\nb".to_string()]).expect_err("should refuse");
        assert!(err.to_string().contains("single line"), "{err}");
    }

    #[test]
    fn build_refuses_an_empty_or_bare_slash() {
        assert!(build("", &[]).is_err());
        assert!(build("/", &[]).is_err());
        assert!(build("   ", &[]).is_err());
    }

    #[test]
    fn build_refuses_a_name_with_whitespace() {
        let err = build("/clear now", &[]).expect_err("should refuse");
        assert!(err.to_string().contains("one word"), "{err}");
    }

    #[test]
    fn list_text_is_one_line_per_command() {
        let text = list_text();
        assert_eq!(text.lines().count(), COMMANDS.len());
        assert!(text.contains("/usage on|off"));
        assert!(text.contains("Open the Activity panel"));
        // Scopes are shown, because a View command is not a Session command.
        assert!(text.contains("Session"));
        assert!(text.contains("View"));
    }
}
