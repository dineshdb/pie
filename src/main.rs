mod aisdk_appleai;
mod bash;
mod cmd;
mod ffi;
mod interactive;
mod router;
mod skill;

use clap::Parser;

#[derive(Parser)]
#[command(name = "pie", version = "0.1.0")]
#[command(about = "Minimal Pi-like agent using Apple on-device AI")]
struct Cli {
    /// Explicitly use a specific skill
    #[arg(short, long)]
    skill: Option<String>,

    /// Query to process
    query: Vec<String>,

    /// List available skills
    #[arg(long)]
    list_skills: bool,
}

fn main() -> anyhow::Result<()> {
    // Handle `list-skills` / `ls` as positional subcommands (like the TS version)
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args
        .first()
        .map_or(false, |a| a == "list-skills" || a == "ls")
    {
        cmd::handle_list_skills();
        return Ok(());
    }

    let cli = Cli::parse();

    if cli.list_skills {
        cmd::handle_list_skills();
        return Ok(());
    }

    // No query → interactive mode
    if cli.query.is_empty() && cli.skill.is_none() {
        return interactive::start_interactive_mode();
    }

    let query = cli.query.join(" ");
    if cli.skill.is_some() && query.is_empty() {
        anyhow::bail!("Usage: pie -s <skill> '<query>'");
    }

    let parsed = interactive::ParsedInput {
        skill: cli.skill,
        query,
    };
    cmd::handle_query(&parsed)
}
