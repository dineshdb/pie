#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

mod agent;
mod cmd;
mod config;
mod db;
mod handler;
mod hook;
mod instructions;
mod plugin;
mod prompt;
mod provider;
mod registry;
mod session;
mod skill;
mod tools;
mod ui;
mod utils;

use crate::config::{PieConfig, ResolvedConfig, build_sandbox, load_config};
use crate::instructions::Instructions;
use crate::registry::Registry;
use crate::utils::output::OutputFormat;
use crate::{db::DbPool, session::Session};
use anyhow::Context;
use clap::Parser;
use core::option::Option::Some;
use std::io::{self, IsTerminal, Read};
use std::sync::Arc;
use tracing::trace;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Clone)]
#[command(name = "pie", version = "0.1.0")]
#[command(about = "Minimal Pi-like agent using OpenAI-compatible providers")]
#[allow(clippy::struct_excessive_bools)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    #[command(flatten)]
    provider_config: config::ProviderConfig,

    #[arg(short, long, global = true)]
    debug: bool,

    /// Output response in JSON format
    #[arg(long, global = true)]
    json: bool,

    /// Output response in Markdown format
    #[arg(long, global = true)]
    md: bool,

    /// Config provider name (from ~/.pie/pie.toml or .pie/pie.toml)
    #[arg(short, long, global = true)]
    provider: Option<String>,

    /// Query to process
    query: Vec<String>,

    /// Continue the last session for this directory
    #[arg(short, long, global = true)]
    resume: bool,
}

#[derive(clap::Subcommand, Clone)]
enum Commands {
    /// Show current configuration and system status
    Status,
    /// List available skills and agents
    Skills,
    /// Launch another agent with current provider environment
    Launch {
        /// Command and arguments to execute
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        all_args: Vec<String>,
        /// Do not sandbox the command
        #[arg(short = 'S', long)]
        no_sandbox: bool,
    },
}

impl Cli {
    pub fn output_format(&self) -> OutputFormat {
        match (self.json, self.md) {
            (true, _) => OutputFormat::Json,
            (false, true) => OutputFormat::Markdown,
            _ => OutputFormat::Default,
        }
    }
}

async fn resolve_session(pool: Arc<DbPool>, resume: bool) -> anyhow::Result<Session> {
    let cwd = std::env::current_dir()?.to_string_lossy().to_string();
    if resume && let Some(session) = Session::find_latest_for_cwd(pool.clone(), &cwd).await? {
        return Ok(session);
    }
    Session::create(pool).await
}

/// Run the PIE agent.
///
/// # Errors
///
/// Returns an error if:
/// - Configuration cannot be loaded or resolved.
/// - Database pool cannot be initialized.
/// - Session cannot be resolved.
/// - Subscriber initialization fails.
/// - The command or interactive session fails.
pub async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let format = cli.output_format();

    let pool = Arc::new(db::create_persistent_pool().await?);

    let pie_config = load_config()?;
    let config: ResolvedConfig = (cli.clone(), pie_config.clone()).try_into()?;

    config::CONFIG
        .set(config)
        .map_err(|_| anyhow::anyhow!("global config already initialized"))?;

    let config = config::CONFIG.get().context("config should be set")?;

    let registry = Registry::load();
    if let Some(cmd) = cli.command {
        return handle_command(cmd, config, &registry);
    }

    let session = resolve_session(pool.clone(), cli.resume).await?;
    let has_query = !cli.query.is_empty() || !io::stdin().is_terminal();
    if format.is_explicit() || has_query {
        init_stderr_subscriber(cli.debug, &config.log_level);
        run_single_shot(cli, config, session, format, &pie_config, registry).await
    } else {
        init_file_subscriber(&session.id.to_string(), &config.log_level)?;
        run_interactive(config, session, &pie_config, registry).await
    }
}

fn handle_command(
    cmd: Commands,
    config: &ResolvedConfig,
    registry: &Arc<Registry>,
) -> anyhow::Result<()> {
    match cmd {
        Commands::Status => {
            init_stderr_subscriber(config.debug, &config.log_level);
            cmd::handle_status(config, registry);
            Ok(())
        }
        Commands::Skills => {
            init_stderr_subscriber(config.debug, &config.log_level);
            cmd::handle_skills(config, registry);
            Ok(())
        }
        Commands::Launch {
            all_args,
            no_sandbox,
        } => handle_launch(config, &all_args, no_sandbox),
    }
}

fn handle_launch(
    config: &ResolvedConfig,
    all_args: &[String],
    no_sandbox: bool,
) -> anyhow::Result<()> {
    if config.debug {
        init_stderr_subscriber(config.debug, &config.log_level);
    }

    let (command, args) = if let Some((cmd, rest)) = all_args.split_first() {
        (cmd.clone(), rest.to_vec())
    } else {
        anyhow::bail!("no command provided to launch");
    };

    if command == "claude" && config.provider.anthropic_url.is_none() {
        anyhow::bail!(
            "launching 'claude' is only supported on providers with an anthropic endpoint (e.g., 'anthropic', 'openrouter', 'ollama', 'zai')"
        );
    }

    let env = config.provider.env_vars();
    let launch_configs = config::load_launch_config()?;

    // Resolve alias
    let (actual_command, launch_cfg) = if let Some(cfg) = launch_configs.get(&command) {
        (command.clone(), Some(cfg))
    } else if let Some((name, cfg)) = launch_configs
        .iter()
        .find(|(_, cfg)| cfg.aliases.contains(&command))
    {
        (name.clone(), Some(cfg))
    } else {
        (command.clone(), None)
    };

    let mut final_args = args;
    if let Some(cfg) = launch_cfg
        && final_args.is_empty()
    {
        final_args.clone_from(&cfg.args);
    }

    // If the command itself contains spaces and no args were provided, split it.
    // This handles cases like `pie launch "claude --version"`.
    let (actual_command, extra_args) = if actual_command.contains(' ') && final_args.is_empty() {
        let parts: Vec<String> = actual_command
            .split_whitespace()
            .map(String::from)
            .collect();
        if let (Some(cmd), Some(args)) = (parts.first(), parts.get(1..)) {
            (cmd.clone(), args.to_vec())
        } else {
            (actual_command, Vec::new())
        }
    } else {
        (actual_command, Vec::new())
    };
    if !extra_args.is_empty() {
        final_args = extra_args;
    }

    let mut cmd = if no_sandbox {
        let mut c = std::process::Command::new(&actual_command);
        c.args(&final_args);
        c
    } else if let Some(cfg) = launch_cfg
        && let Some(sandbox) = &cfg.sandbox
    {
        p1e_sandbox::build_command(&actual_command, &final_args, sandbox)
    } else {
        let mut c = std::process::Command::new(&actual_command);
        c.args(&final_args);
        c
    };

    // Explicitly inherit stdio for interaction
    cmd.stdin(std::process::Stdio::inherit());
    cmd.stdout(std::process::Stdio::inherit());
    cmd.stderr(std::process::Stdio::inherit());

    // 1. Provider env vars
    for (k, v) in env {
        cmd.env(k, v);
    }

    // 2. Extra env vars from launch.toml
    if let Some(cfg) = launch_cfg {
        for (k, v) in &cfg.env {
            cmd.env(k, v);
        }
    }

    // Process Handoff (Unix):
    // On Unix-like systems, we use `execvp` to replace the current `pie` process with the target command.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        anyhow::bail!("failed to launch command: {err}");
    }

    // Fallback (Non-Unix):
    // Spawning is used where process replacement is not supported (e.g., Windows).
    #[cfg(not(unix))]
    {
        let mut child = cmd.spawn().context("failed to launch command")?;
        let status = child.wait().context("failed to wait for child")?;
        std::process::exit(status.code().unwrap_or(0));
    }
}

async fn run_single_shot(
    cli: Cli,
    config: &ResolvedConfig,
    session: Session,
    format: OutputFormat,
    pie_config: &PieConfig,
    registry: Arc<Registry>,
) -> anyhow::Result<()> {
    let sandbox_settings = build_sandbox(pie_config);
    trace!(config = ?config, "config");
    let model = provider::build_from_resolved(&config.provider);
    let piped_stdin = read_piped_stdin();

    let cli_query = cli.query.join(" ");
    let query = if cli_query.is_empty() {
        piped_stdin.as_deref().unwrap_or_default().to_string()
    } else {
        cli_query
    };

    let full_query = if query.is_empty() && piped_stdin.is_none() {
        anyhow::bail!(
            "No query provided. Use `pie` for interactive mode or pass a query with --md or --json."
        );
    } else {
        match piped_stdin.as_deref() {
            Some(stdin) if !query.is_empty() => {
                format!("## Stdin\n```\n{stdin}\n```\n\n{query}")
            }
            Some(stdin) => stdin.to_string(),
            None => query,
        }
    };

    let query = Instructions::new(full_query);
    handler::handle_query(handler::HandleParams {
        model,
        query,
        session,
        format,
        sandbox_settings,
        max_steps: config.max_steps,
        retry: config.retry.clone(),
        registry,
    })
    .await
}

async fn run_interactive(
    config: &ResolvedConfig,
    session: Session,
    pie_config: &PieConfig,
    registry: Arc<Registry>,
) -> anyhow::Result<()> {
    let sandbox_settings = build_sandbox(pie_config);
    let model = provider::build_from_resolved(&config.provider);

    ui::tui::run_tui(
        model,
        config.provider.clone(),
        session,
        sandbox_settings,
        config.max_steps,
        pie_config.clone(),
        registry,
    )
    .await
}

fn default_env_filter(default_level: &str) -> EnvFilter {
    let filter_str = match default_level {
        "debug" => "warn,p1e=debug,pie=debug,p1e_sandbox=debug",
        others => others,
    };
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(filter_str))
}

fn init_stderr_subscriber(debug: bool, config_level: &str) {
    let filter = default_env_filter(if debug { "debug" } else { config_level });

    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_target(false)
        .with_level(true)
        .without_time()
        .with_env_filter(filter)
        .compact()
        .init();
}

fn init_file_subscriber(session_id: &str, log_level: &str) -> anyhow::Result<()> {
    let filter = default_env_filter(log_level);

    let log_path = config::logs_dir().join(format!("{session_id}.log"));
    let file = std::fs::File::create(&log_path).context("can't create log file")?;

    tracing_subscriber::fmt()
        .with_writer(file)
        .with_ansi(false)
        .with_target(true)
        .with_level(true)
        .with_env_filter(filter)
        .compact()
        .init();

    Ok(())
}

/// Read piped stdin. Returns None if stdin is a terminal or empty.
fn read_piped_stdin() -> Option<String> {
    if io::stdin().is_terminal() {
        return None;
    }
    let mut buf = String::new();
    io::stdin().read_to_string(&mut buf).ok()?;
    let trimmed = buf.trim().to_string();
    (!trimmed.is_empty()).then_some(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolve_session_creates_new_when_not_resuming() {
        let pool = Arc::new(db::create_test_pool().await.unwrap());
        let session = resolve_session(pool, false).await.unwrap();
        assert!(session.history_entries().is_empty());
    }

    #[tokio::test]
    async fn resolve_session_restores_when_resuming() {
        let pool = Arc::new(db::create_test_pool().await.unwrap());
        let mut original = Session::create(pool.clone()).await.unwrap();
        original.add_user("hello").await.unwrap();
        original.add_assistant("world").await.unwrap();
        drop(original);

        let session = resolve_session(pool, true).await.unwrap();
        let entries = session.history_entries();
        assert_eq!(entries.len(), 2, "restored session should have history");
        assert_eq!(entries[0].content(), "hello");
    }
}
