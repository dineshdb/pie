use crate::ui::tui::state::ChatMessage;

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
    /// Quit the application.
    Quit,
    /// Show help text.
    Help,
    /// List available models or switch to a specific one.
    Model(Option<String>),
    /// List available skills and agents.
    ListSkills,
    /// Clear the message history.
    Clear,
    /// Start a new session.
    New,
}

impl Command {
    /// Parse raw input text into a [`Command`].
    pub fn parse(input: &str) -> Self {
        let trimmed = input.trim();
        if trimmed.starts_with('/') {
            match trimmed {
                "/exit" | "/quit" | "/q" => Self::Quit,
                "/help" | "/h" => Self::Help,
                "/model" => Self::Model(None),
                "/skills" | "/ls" => Self::ListSkills,
                "/clear" => Self::Clear,
                "/new" => Self::New,
                rest => {
                    if let Some(model_name) = rest.strip_prefix("/model ") {
                        return Self::Model(Some(model_name.trim().to_string()));
                    }
                    let without_slash = &rest[1..];
                    let (name, query) = match without_slash.split_once(' ') {
                        Some((n, q)) => (n, q.trim()),
                        _ => (without_slash, ""),
                    };
                    let agents = crate::agent::get_all_agents();
                    if agents.iter().any(|a| a.name == name) {
                        return Self::Invoke {
                            name: name.to_string(),
                            query: query.to_string(),
                            is_agent: true,
                        };
                    }
                    let skills = crate::skill::get_all_skills();
                    if skills.iter().any(|s| s.name == name) {
                        return Self::Invoke {
                            name: name.to_string(),
                            query: query.to_string(),
                            is_agent: false,
                        };
                    }
                    Self::Send(input.to_string())
                }
            }
        } else {
            Self::Send(input.to_string())
        }
    }

    /// Map a parsed command into the action the app should take.
    pub fn dispatch(self) -> CommandAction {
        match self {
            Self::Quit => CommandAction::Quit,
            Self::Help => CommandAction::AddMessage(ChatMessage::system(HELP_TEXT)),
            Self::Model(name) => CommandAction::Model(name),
            Self::ListSkills => {
                let text = build_skills_list();
                CommandAction::AddMessage(ChatMessage::system(&text))
            }
            Self::Clear => CommandAction::ClearMessages,
            Self::New => CommandAction::NewSession,
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
    Quit,
}

/// All slash commands and their aliases, used for tab-completion.
pub const SLASH_COMMANDS: &[&str] = &[
    "/help", "/h", "/exit", "/quit", "/q", "/model", "/skills", "/ls", "/clear", "/new",
];

/// The static help text shown by /help.
pub const HELP_TEXT: &str = r"
Commands:
  /help, /h          Show this help          /skills, /ls  List agents
  /model             List/switch models      /clear        Clear conversation
  /new               New session             /exit, /quit  Exit

Keys:
  Enter              Send message            Ctrl+Enter  New line
  Up/Down            Navigate history        Esc  Cancel streaming
  Page Up/Down       Scroll messages         Ctrl+c  Quit
";

/// Build the full list of slash-command completions (agents + skills + builtins).
pub fn build_all_completions() -> Vec<String> {
    let mut cmds = Vec::new();
    for agent in crate::agent::get_all_agents() {
        cmds.push(format!("/{}", agent.name));
    }
    cmds.extend(SLASH_COMMANDS.iter().map(ToString::to_string));
    for skill in crate::skill::get_all_skills() {
        cmds.push(format!("/{}", skill.name));
    }
    cmds
}

fn build_skills_list() -> String {
    let agents = crate::agent::get_all_agents();
    if agents.is_empty() {
        return "No agents found.".to_string();
    }
    let names: Vec<&str> = agents.iter().map(|a| a.name.as_str()).collect();
    format!("Agents: {}", names.join(", "))
}
