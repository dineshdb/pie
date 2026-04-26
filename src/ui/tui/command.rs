use crate::ui::tui::state::ChatMessage;
use std::str::FromStr;
use strum::{AsRefStr, EnumIter, EnumString};

macro_rules! define_builtin_commands {
    ($($variant:ident => [$($name:expr),+]),* $(,)?) => {
        #[derive(Debug, Clone, Copy, EnumIter, EnumString, AsRefStr, PartialEq, Eq)]
        pub enum BuiltinCommand {
            $(
                $(#[strum(serialize = $name)])+
                $variant,
            )*
        }

        impl BuiltinCommand {
            #[allow(dead_code)]
            pub fn all_commands() -> Vec<&'static str> {
                vec![$($($name),+),*]
            }

            pub fn names(&self) -> &[&'static str] {
                match self {
                    $(Self::$variant => &[$($name),+],)*
                }
            }
        }
    };
}

define_builtin_commands! {
    Help => ["/help", "/h"],
    Quit => ["/quit", "/exit", "/q"],
    Model => ["/model"],
    Skills => ["/skills", "/ls"],
    Clear => ["/clear"],
    New => ["/new"],
}

const HELP_DESC: &str = "Show help and available commands";
const QUIT_DESC: &str = "Exit the application";
const MODEL_DESC: &str = "Switch or view the current model";
const SKILLS_DESC: &str = "List available agents and skills";
const CLEAR_DESC: &str = "Clear the chat history";
const NEW_DESC: &str = "Start a new session";

impl BuiltinCommand {
    pub fn description(self) -> &'static str {
        match self {
            Self::Help => HELP_DESC,
            Self::Quit => QUIT_DESC,
            Self::Model => MODEL_DESC,
            Self::Skills => SKILLS_DESC,
            Self::Clear => CLEAR_DESC,
            Self::New => NEW_DESC,
        }
    }
}

/// Parsed command intent from user input.
pub enum Command {
    /// Send a chat message / invoke a skill/agent.
    Send(String),
    /// Invoke a named agent or skill with a query.
    Invoke {
        name: String,
        query: String,
        is_agent: bool,
    },
    /// Built-in slash commands.
    Builtin(BuiltinCommand, Option<String>),
}

impl Command {
    /// Parse raw input text into a [`Command`].
    pub fn parse(input: &str, registry: &crate::registry::Registry) -> Self {
        let trimmed = input.trim();
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
                    is_agent: true,
                };
            }
            if registry.skills.iter().any(|s| s.name == without_slash) {
                return Self::Invoke {
                    name: without_slash.to_string(),
                    query,
                    is_agent: false,
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
                BuiltinCommand::Clear => CommandAction::ClearMessages,
                BuiltinCommand::New => CommandAction::NewSession,
            },
            Self::Send(query) => CommandAction::Stream(query),
            Self::Invoke {
                name,
                query,
                is_agent,
            } => {
                let rewritten = if is_agent {
                    format!("Use the subagent tool with agent_name=\"{name}\" to handle: {query}")
                } else if query.is_empty() {
                    format!("/{name}")
                } else {
                    format!("/{name} {query}")
                };
                CommandAction::Stream(rewritten)
            }
        }
    }
}

/// Result of executing a command — what the app should do next.
pub enum CommandAction {
    AddMessage(ChatMessage),
    ClearMessages,
    NewSession,
    Stream(String),
    Model(Option<String>),
    Help,
    Quit,
}

/// Build the full list of agents.
fn build_skills_list(registry: &crate::registry::Registry) -> String {
    if registry.agents.is_empty() {
        return "No agents found.".to_string();
    }
    let names: Vec<&str> = registry.agents.iter().map(|a| a.name.as_str()).collect();
    format!("Agents: {}", names.join(", "))
}
