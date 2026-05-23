use crate::cmd::BuiltinCommand;
use crate::ui::tui::state::ChatMessage;
use std::str::FromStr;

/// Parsed command intent from user input.
pub enum Command {
    /// Send a chat message / invoke a skill/agent.
    Send(String),
    /// Execute a shell command directly.
    Shell(String),
    /// Invoke a named agent or skill with a query.
    Invoke { name: String, query: String },
    /// Built-in slash commands.
    Builtin(BuiltinCommand, Option<String>),
}

impl Command {
    /// Parse raw input text into a [`Command`].
    pub fn parse(input: &str, registry: &crate::registry::Registry) -> Self {
        let trimmed = input.trim();
        if let Some(after_bang) = trimmed.strip_prefix('!')
            && !after_bang.is_empty()
            && !after_bang.starts_with(char::is_whitespace)
        {
            return Self::Shell(after_bang.to_string());
        }

        if trimmed.starts_with('/') {
            let (cmd_part, rest) = match trimmed.split_once(' ') {
                Some((c, r)) => (c, Some(r.trim().to_string())),
                None => (trimmed, None),
            };

            if let Ok(builtin) = BuiltinCommand::from_str(cmd_part) {
                return Self::Builtin(builtin, rest);
            }

            let without_slash = &cmd_part[1..];
            let query = rest.unwrap_or_default();

            if registry.agents.iter().any(|a| a.name == without_slash) {
                return Self::Invoke {
                    name: without_slash.to_string(),
                    query,
                };
            }
            if registry.skills.iter().any(|s| s.name == without_slash) {
                return Self::Invoke {
                    name: without_slash.to_string(),
                    query,
                };
            }
        }
        Self::Send(input.to_string())
    }

    /// Map a parsed command into the action the app should take.
    pub fn dispatch(self, registry: &crate::registry::Registry) -> CommandAction {
        match self {
            Self::Builtin(builtin, args) => match builtin {
                BuiltinCommand::Help => CommandAction::Help,
                BuiltinCommand::Quit => CommandAction::Quit,
                BuiltinCommand::Model => CommandAction::Model(args),
                BuiltinCommand::Skills => {
                    let text = build_skills_list(registry);
                    CommandAction::AddMessage(ChatMessage::system(&text))
                }
                BuiltinCommand::Clear | BuiltinCommand::New => CommandAction::NewSession,
            },
            Self::Shell(command) => CommandAction::Shell(command),
            Self::Send(query) => CommandAction::Stream(query),
            Self::Invoke { name, query } => CommandAction::Stream(format!("/{name} {query}")),
        }
    }
}

/// Result of executing a command — what the app should do next.
pub enum CommandAction {
    AddMessage(ChatMessage),
    NewSession,
    Stream(String),
    Shell(String),
    Model(Option<String>),
    Help,
    Quit,
}

/// Build the full list of agents and skills.
fn build_skills_list(registry: &crate::registry::Registry) -> String {
    let mut parts = Vec::new();

    if !registry.agents.is_empty() {
        parts.push("Agents:".to_string());
        for agent in &registry.agents {
            parts.push(format!(" - {}: {}", agent.name, agent.description));
        }
    }

    if !registry.skills.is_empty() {
        if !parts.is_empty() {
            parts.push(String::new());
        }
        parts.push("Skills:".to_string());
        for skill in &registry.skills {
            parts.push(format!(" - {}: {}", skill.name, skill.description));
            for r in &skill.references {
                parts.push(format!("   - {}: {}", r.title, r.path));
            }
        }
    }

    if parts.is_empty() {
        "No agents or skills found.".to_string()
    } else {
        parts.join("\n")
    }
}
