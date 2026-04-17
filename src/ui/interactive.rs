use crate::agent::get_all_agents;
use crate::cmd::handle_list_skills;
use crate::config::pie_home;
use crate::handler::handle_query_streaming;
use crate::providers::Model;
use crate::session::Session;
use crate::skill::get_all_skills;
use nu_ansi_term::{Color, Style};
use reedline::{
    ColumnarMenu, DefaultCompleter, DefaultPrompt, DefaultPromptSegment, Emacs, FileBackedHistory,
    Hinter, History, KeyCode, KeyModifiers, MenuBuilder, Reedline, ReedlineEvent, ReedlineMenu,
    Signal,
};
use std::path::PathBuf;
use tracing::info;

const HELP_TEXT: &str = r#"
pie - Interactive Mode
Usage:
  <query>              Ask a question using auto-detected skills
  /skillname <query>   Use a specific skill
  /list-skills, /ls    List available skills
  /help, /h            Show this help
  /exit, /quit, /q     Exit interactive mode

Examples:
  How do I create a new git branch?
  /search latest TypeScript features
  /list-skills
"#;

fn build_completions() -> Vec<String> {
    let mut commands: Vec<String> = vec![
        "/help".into(),
        "/h".into(),
        "/exit".into(),
        "/quit".into(),
        "/q".into(),
        "/list-skills".into(),
        "/ls".into(),
    ];

    for skill in get_all_skills() {
        commands.push(format!("/{}", skill.name));
    }

    for agent in get_all_agents() {
        commands.push(format!("/{}", agent.name));
    }

    commands
}

/// Custom hinter that falls back to command completions when history has no match.
struct PieHinter {
    style: Style,
    min_chars: usize,
    current_hint: String,
    commands: Vec<String>,
}

impl PieHinter {
    fn new(commands: Vec<String>) -> Self {
        Self {
            style: Style::new().italic().fg(Color::DarkGray),
            min_chars: 1,
            current_hint: String::new(),
            commands,
        }
    }
}

impl Hinter for PieHinter {
    fn handle(
        &mut self,
        line: &str,
        _pos: usize,
        history: &dyn History,
        use_ansi_coloring: bool,
        _cwd: &str,
    ) -> String {
        self.current_hint = if line.chars().count() >= self.min_chars && line.starts_with('/') {
            // Try history first
            let history_hint = history
                .search(reedline::SearchQuery::last_with_prefix(
                    line.to_string(),
                    history.session(),
                ))
                .ok()
                .and_then(|entries| entries.first().cloned())
                .and_then(|entry| entry.command_line.get(line.len()..).map(|s| s.to_string()));

            if let Some(hint) = history_hint {
                hint
            } else {
                // Fall back to command completions
                self.commands
                    .iter()
                    .find(|cmd| cmd.starts_with(line) && cmd.as_str() != line)
                    .map(|cmd| cmd[line.len()..].to_string())
                    .unwrap_or_default()
            }
        } else {
            String::new()
        };

        if use_ansi_coloring && !self.current_hint.is_empty() {
            self.style.paint(&self.current_hint).to_string()
        } else {
            self.current_hint.clone()
        }
    }

    fn complete_hint(&self) -> String {
        self.current_hint.clone()
    }

    fn next_hint_token(&self) -> String {
        self.current_hint.clone()
    }
}

pub async fn start_interactive_mode(
    model: &mut Model,
    mut session: Session,
    sandbox_settings: PathBuf,
) -> anyhow::Result<()> {
    info!("Welcome to pie! Type '/help' for usage or '/exit' to quit.\n");

    let history_dir = pie_home().join("history");
    std::fs::create_dir_all(&history_dir)?;
    let history_path = history_dir.join(format!("{}.txt", session.id));
    let history = Box::new(
        FileBackedHistory::with_file(1000, history_path)
            .expect("Error configuring history with file"),
    );

    let completions = build_completions();

    let mut completer =
        Box::new(DefaultCompleter::with_inclusions(&['-', '/']).set_min_word_len(1));
    completer.insert(completions.clone());

    let completion_menu = Box::new(ColumnarMenu::default().with_name("completion_menu"));

    let mut keybindings = reedline::default_emacs_keybindings();
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu("completion_menu".to_string()),
            ReedlineEvent::MenuNext,
        ]),
    );

    let edit_mode = Box::new(Emacs::new(keybindings));

    let mut line_editor = Reedline::create()
        .with_history(history)
        .with_completer(completer)
        .with_menu(ReedlineMenu::EngineCompleter(completion_menu))
        .with_edit_mode(edit_mode)
        .with_hinter(Box::new(PieHinter::new(completions)));

    let prompt = DefaultPrompt::new(
        DefaultPromptSegment::Basic("pie".to_string()),
        DefaultPromptSegment::Empty,
    );

    loop {
        let sig = line_editor.read_line(&prompt);
        match sig {
            Ok(Signal::Success(buffer)) => {
                let input = buffer.trim();
                if input.is_empty() {
                    continue;
                }

                match input {
                    "/exit" | "/quit" | "/q" => {
                        info!("Goodbye!");
                        return Ok(());
                    }
                    "/help" | "/h" => {
                        info!("{HELP_TEXT}");
                    }
                    "/list-skills" | "/ls" => {
                        handle_list_skills();
                    }
                    _ => {
                        // Strip leading / for query if it's a bare command
                        let query = input.strip_prefix('/').unwrap_or(input);
                        if let Err(e) = handle_query_streaming(
                            model,
                            query,
                            &mut session,
                            sandbox_settings.clone(),
                        )
                        .await
                        {
                            tracing::error!("Error: {e}");
                        }
                    }
                }
            }
            Ok(Signal::CtrlC) => {
                continue;
            }
            Ok(Signal::CtrlD) => {
                info!("Goodbye!");
                return Ok(());
            }
            _ => {}
        }
    }
}
