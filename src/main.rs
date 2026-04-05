mod afm;
mod agent;
mod interactive;
mod provider;
mod skill;

use clap::Parser;
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Handle `list-skills` / `ls` as positional subcommands
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args
        .first()
        .is_some_and(|a| a == "list-skills" || a == "ls")
    {
        agent::handle_list_skills();
        return Ok(());
    }

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
        agent::handle_list_skills();
        return Ok(());
    }

    let mut model = provider::build_model(
        cli.model.as_deref(),
        cli.base_url.as_deref(),
        cli.api_key.as_deref(),
    )?;

    // No query → interactive mode
    if cli.query.is_empty() && cli.skill.is_none() {
        return interactive::start_interactive_mode(&mut model).await;
    }

    let query = cli.query.join(" ");
    if cli.skill.is_some() && query.is_empty() {
        anyhow::bail!("Usage: pie -s <skill> '<query>'");
    }

    let parsed = agent::ParsedInput {
        skill: cli.skill,
        query,
    };
    agent::handle_query(&mut model, &parsed).await
}
