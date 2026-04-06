use tracing::info;

use crate::agent::{handle_list_skills, handle_query};
use crate::provider::Model;
use aisdk::core::Messages;
use std::io::{self, Write};

const HELP_TEXT: &str = r#"
pie - Interactive Mode
Usage:
  <query>              Ask a question using auto-detected skills
  /skillname <query>   Use a specific skill
  list-skills, ls      List available skills
  help, h              Show this help
  exit, quit, q        Exit interactive mode

Examples:
  How do I create a new git branch?
  /search latest TypeScript features
  list-skills
"#;

pub async fn start_interactive_mode(model: &mut Model) -> anyhow::Result<()> {
    info!("Welcome to pie! Type 'help' for usage or 'exit' to quit.\n");

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut history: Messages = Vec::new();

    loop {
        print!("pie> ");
        stdout.flush()?;
        let mut input = String::new();
        stdin.read_line(&mut input)?;
        let input = input.trim();

        if input.is_empty() {
            continue;
        }

        match input {
            "exit" | "quit" | "q" => {
                info!("Goodbye!");
                return Ok(());
            }
            "help" | "h" => {
                info!("{HELP_TEXT}");
            }
            "list-skills" | "ls" => {
                handle_list_skills();
            }
            _ => {
                if let Err(e) = handle_query(model, input, Some(&mut history)).await {
                    tracing::error!("Error: {e}");
                }
            }
        }
    }
}
