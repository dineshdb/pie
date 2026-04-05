use tracing::info;

use crate::agent::{handle_list_skills, handle_query, ParsedInput};
use crate::provider::Model;
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
                let args = parse_input(input);
                if let Err(e) = handle_query(model, &args).await {
                    tracing::error!("Error: {e}");
                }
            }
        }
    }
}

fn parse_input(input: &str) -> ParsedInput {
    let parts: Vec<&str> = input.splitn(3, ' ').collect();

    if parts[0].starts_with('/') {
        ParsedInput {
            skill: Some(parts[0][1..].to_string()),
            query: parts.get(1).unwrap_or(&"").to_string(),
        }
    } else if parts[0] == "--skill" || parts[0] == "-s" {
        ParsedInput {
            skill: parts.get(1).map(|s| s.to_string()),
            query: parts.get(2).unwrap_or(&"").to_string(),
        }
    } else {
        ParsedInput {
            skill: None,
            query: input.to_string(),
        }
    }
}
