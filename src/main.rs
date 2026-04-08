mod core;
mod providers;
mod ui;

use crate::core::{db::DbPool, session::Session};
use clap::Parser;
use std::io::{self, IsTerminal, Read};
use std::sync::Arc;
use tracing::Level;

#[derive(Parser)]
#[command(name = "pie", version = "0.1.0")]
#[command(about = "Minimal Pi-like agent using Apple on-device AI or OpenAI-compatible providers")]
struct Cli {
    /// Explicitly use a specific skill
    #[arg(short, long)]
    skill: Option<String>,

    #[arg(short, long)]
    debug: bool,

    /// Model name (e.g. llama3, gpt-4o, claude-3.5-sonnet)
    #[arg(short, long)]
    model: Option<String>,

    /// API base URL for OpenAI-compatible providers
    #[arg(long)]
    base_url: Option<String>,

    /// API key for OpenAI-compatible providers
    #[arg(long)]
    api_key: Option<String>,

    /// Query to process
    query: Vec<String>,

    /// List available skills
    #[arg(long)]
    list_skills: bool,

    /// Continue the last session for this directory
    #[arg(short, long)]
    r#continue: bool,
}

fn resolve_session(pool: Arc<DbPool>, resume: bool) -> anyhow::Result<Session> {
    let cwd = std::env::current_dir()?.to_string_lossy().to_string();
    if resume && let Some(session) = Session::find_latest_for_cwd(pool.clone(), &cwd)? {
        return Ok(session);
    }
    core::session::Session::create(pool)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    let cli = Cli::parse();
    {
        let subscriber = tracing_subscriber::fmt()
            .with_target(false)
            .with_level(false)
            .compact();

        if cli.debug {
            subscriber.with_max_level(Level::DEBUG).init();
        } else {
            subscriber.without_time().init();
        }
    }

    if cli.list_skills {
        core::agent::handle_list_skills();
        return Ok(());
    }

    let mut model = providers::build_model(
        cli.model.as_deref(),
        cli.base_url.as_deref(),
        cli.api_key.as_deref(),
    )?;

    let pool = Arc::new(core::db::create_pool()?);

    let piped_stdin = read_piped_stdin();

    // No query args and no skill -> interactive mode (or use piped stdin as query)
    if cli.query.is_empty() && cli.skill.is_none() {
        if let Some(stdin_content) = piped_stdin {
            let mut session = resolve_session(pool, cli.r#continue)?;
            return core::agent::handle_query(&mut model, &stdin_content, &mut session).await;
        }
        let session = resolve_session(pool, cli.r#continue)?;
        return ui::interactive::start_interactive_mode(&mut model, session).await;
    }

    let query = cli.query.join(" ");
    if cli.skill.is_some() && query.is_empty() && piped_stdin.is_none() {
        anyhow::bail!("Usage: pie -s <skill> '<query>'");
    }

    let full_query = match piped_stdin {
        Some(stdin) if !query.is_empty() => format!("## Stdin\n```\n{stdin}\n```\n\n{query}"),
        Some(stdin) => stdin,
        None => query,
    };

    let mut session = resolve_session(pool, cli.r#continue)?;
    core::agent::handle_query(&mut model, &full_query, &mut session).await
}

/// Read piped stdin. Returns None if stdin is a terminal or empty.
fn read_piped_stdin() -> Option<String> {
    if io::stdin().is_terminal() {
        return None;
    }
    let mut buf = String::new();
    io::stdin().read_to_string(&mut buf).ok()?;
    let trimmed = buf.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}
