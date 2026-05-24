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
mod cron;
mod db;
pub mod error;
mod handler;
mod instructions;
mod plugin;
mod prompt;
mod registry;
mod session;
mod tools;
mod ui;
mod utils;

use crate::config::{PieConfig, ResolvedConfig, build_sandbox, load_config};
use crate::error::Result;
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

    /// Output response in JSON format. Provide a valid JSON schema (inline or file path).
    #[arg(long, global = true)]
    json: Option<String>,

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
    /// Run the cron daemon (continuous mode)
    Daemon {
        /// Check interval in seconds (default: 60)
        #[arg(short, long, default_value = "60")]
        interval: u64,
    },
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
    /// Manage cron jobs
    Cron {
        #[command(subcommand)]
        command: CronCommand,
    },
    /// Execute a script from a skill directly (no LLM)
    #[command(name = "x")]
    Exec {
        /// Skill name (use -s <skill> or as first positional argument)
        #[arg(short = 's')]
        skill: Option<String>,
        /// Script to execute and optional arguments
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        script: Vec<String>,
    },
}

#[derive(clap::Subcommand, Clone)]
enum CronCommand {
    /// List schedules loaded from files
    List,
    /// Show recent run history (optionally for a specific schedule)
    Runs { id: Option<String> },
    /// Execute due schedules (one-shot)
    Run,
}

impl Cli {
    pub fn output_format(&self) -> OutputFormat {
        match (self.json.is_some(), self.md) {
            (true, _) => OutputFormat::Json(self.json.clone().filter(|s| !s.is_empty())),
            (false, true) => OutputFormat::Markdown,
            _ => OutputFormat::Default,
        }
    }
}

async fn resolve_session(pool: Arc<DbPool>, resume: bool) -> Result<Session> {
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
        return handle_command(cmd, config, &registry, pool.clone()).await;
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

async fn handle_command(
    cmd: Commands,
    config: &ResolvedConfig,
    registry: &Arc<Registry>,
    pool: Arc<DbPool>,
) -> anyhow::Result<()> {
    // Commands that don't need interactive UI usually want stderr logging
    if !matches!(cmd, Commands::Daemon { .. }) || config.debug {
        init_stderr_subscriber(config.debug, &config.log_level);
    }

    match cmd {
        Commands::Status => {
            cmd::handle_status(config, registry);
            Ok(())
        }
        Commands::Skills => {
            cmd::handle_skills(config, registry);
            Ok(())
        }
        Commands::Launch {
            all_args,
            no_sandbox,
        } => cmd::handle_launch(config, &all_args, no_sandbox),
        Commands::Cron { command } => handle_cron(command, pool, registry.clone()).await,
        Commands::Exec { skill, script } => cmd::handle_exec(config, registry, skill, &script),
        Commands::Daemon { interval } => {
            if !config.debug {
                tracing::info!(
                    "pie daemon starting (interval: {interval}s, pid: {})",
                    std::process::id()
                );
            }
            cron::run_daemon(pool, registry.clone(), interval).await
        }
    }
}

async fn handle_cron(
    command: CronCommand,
    pool: Arc<DbPool>,
    registry: Arc<Registry>,
) -> anyhow::Result<()> {
    match command {
        CronCommand::List => {
            let schedules = cron::load_all_schedules();
            if schedules.is_empty() {
                tracing::info!("no schedules found");
                return Ok(());
            }

            let max_id = schedules.iter().map(|s| s.id.len()).max().unwrap_or(4);
            for s in &schedules {
                let status = if s.enabled { "enabled " } else { "disabled" };
                tracing::info!(
                    "{:max_id$}  {}  {}  {}",
                    s.id,
                    status,
                    s.cron,
                    s.description,
                    max_id = max_id
                );
            }
            Ok(())
        }
        CronCommand::Runs { id } => {
            let rows = match &id {
                Some(schedule_id) => cron::CronRun::recent_for_schedule(&pool, schedule_id).await?,
                None => cron::CronRun::recent_all(&pool).await?,
            };

            if rows.is_empty() {
                let label = id.as_deref().unwrap_or("any");
                tracing::info!("no runs for schedule '{label}'");
                return Ok(());
            }

            let schedules = cron::load_all_schedules();
            for r in &rows {
                let started = chrono::DateTime::from_timestamp_millis(r.started_at)
                    .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
                    .unwrap_or_default();
                let dur_ms = r.finished_at.map_or(0, |f| f - r.started_at);
                let code = r.exit_code.map_or("-".to_string(), |c| c.to_string());
                let desc = schedules
                    .iter()
                    .find(|s| s.id == r.cron_id)
                    .and_then(|s| (!s.description.is_empty()).then_some(&s.description))
                    .unwrap_or(&r.cron_id);

                tracing::info!(
                    "{}  {}  {}  {}ms  {}  {}",
                    desc,
                    started,
                    r.status,
                    dur_ms,
                    code,
                    r.notes
                );
            }
            Ok(())
        }
        CronCommand::Run => cron::run_due_jobs(pool, registry).await,
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
    let model = config.provider.build_client();
    let piped_stdin = read_piped_stdin();

    let cli_query = cli.query.join(" ");
    if cli_query.is_empty() && piped_stdin.is_none() {
        anyhow::bail!(
            "No query provided. Use `pie` for interactive mode or pass a query with --md or --json."
        );
    }

    let full_query = match (piped_stdin.as_deref(), cli_query.is_empty()) {
        (Some(stdin), false) => format!("## Stdin\n```\n{stdin}\n```\n\n{cli_query}"),
        (Some(stdin), true) => stdin.to_string(),
        (None, _) => cli_query,
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
    let model = config.provider.build_client();

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
