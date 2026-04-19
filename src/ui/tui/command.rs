use crate::ui::tui::state::ChatMessage;

/// Parsed command intent from user input.
pub enum Command {
    /// Send a chat message / invoke a skill/agent.
    Send(String),
    /// Quit the application.
    Quit,
    /// Show help text.
    Help,
    /// List available skills and agents.
    ListSkills,
    /// Clear the message history.
    Clear,
}

impl Command {
    /// Parse raw input text into a [`Command`].
    pub fn parse(input: &str) -> Self {
        let trimmed = input.trim();
        if trimmed.starts_with('/') {
            match trimmed {
                "/exit" | "/quit" | "/q" => Self::Quit,
                "/help" | "/h" => Self::Help,
                "/skills" | "/ls" => Self::ListSkills,
                "/clear" => Self::Clear,
                _ => Self::Send(input.to_string()),
            }
        } else {
            Self::Send(input.to_string())
        }
    }
}

/// Result of executing a command — what the app should do next.
pub enum CommandAction {
    /// Add this message to the chat.
    AddMessage(ChatMessage),
    /// Clear all messages.
    ClearMessages,
    /// Begin streaming a response for this query.
    Stream(String),
    /// Shut down.
    Quit,
}

/// All slash commands and their aliases, used for tab-completion.
pub const SLASH_COMMANDS: &[&str] = &[
    "/help", "/h", "/exit", "/quit", "/q", "/skills", "/ls", "/clear",
];

/// The static help text shown by /help.
pub const HELP_TEXT: &str = r"
Commands:
  /help, /h          Show this help          /skills, /ls  List agents
  /clear             Clear conversation      /exit, /quit, /q  Exit

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
