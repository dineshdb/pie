#![warn(
    // Correctness
    future_incompatible,
    nonstandard_style,
    rust_2024_compatibility,
    // Strictness
    missing_debug_implementations,
    missing_copy_implementations,
    trivial_casts,
    trivial_numeric_casts,
    unsafe_op_in_unsafe_fn,
    unused_import_braces,
    unused_lifetimes,
    unused_qualifications,
    variant_size_differences,
    // Clippy pedantic (as compiler warnings)
    clippy::pedantic,
)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
#![allow(
    clippy::module_name_repetitions,
    clippy::multiple_crate_versions,
    clippy::future_not_send
)]

mod agent;
mod cmd;
mod config;
mod db;
mod handler;
mod hook;
mod instructions;
mod output;
mod prompt;
mod providers;
mod registry;
mod session;
mod skill;
mod tools;
mod ui;
mod utils;

use crate::config::{ResolvedConfig, build_sandbox, load_config};
use crate::instructions::Instructions;
use crate::output::OutputFormat;
use crate::{db::DbPool, session::Session};
use anyhow::Context;
use clap::Parser;
use core::option::Option::Some;
use std::io::{self, IsTerminal, Read};
use std::sync::Arc;
use tracing::debug;
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

    #[arg(short, long)]
    debug: bool,

    /// Output response in JSON format
    #[arg(long)]
    json: bool,

    /// Output response in Markdown format
    #[arg(long)]
    md: bool,

    /// Config provider name (from ~/.pie/pie.toml or .pie/pie.toml)
    #[arg(short, long)]
    provider: Option<String>,

    /// Query to process
    query: Vec<String>,

    /// Continue the last session for this directory
    #[arg(short, long)]
    resume: bool,
}

#[derive(clap::Subcommand, Clone)]
enum Commands {
    /// Show current configuration and system status
    Status,
    /// List available skills and agents
    Skills,
}

impl Cli {
    pub fn is_persistent(&self) -> bool {
        self.resume && !self.md && !self.json
    }

    pub fn output_format(&self) -> OutputFormat {
        match (self.json, self.md) {
            (true, _) => OutputFormat::Json,
            (false, true) => OutputFormat::Markdown,
            _ => OutputFormat::Default,
        }
    }
}

fn resolve_session(pool: Arc<DbPool>, resume: bool) -> anyhow::Result<Session> {
    let cwd = std::env::current_dir()?.to_string_lossy().to_string();
    if resume && let Some(session) = Session::find_latest_for_cwd(pool.clone(), &cwd)? {
        return Ok(session);
    }
    Session::create(pool)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let format = if cli.json {
        Some(OutputFormat::Json)
    } else if cli.md {
        Some(OutputFormat::Markdown)
    } else {
        None
    };

    let pool = if cli.is_persistent() {
        Arc::new(db::create_persistent_pool()?)
    } else {
        Arc::new(db::create_memory_pool()?)
    };

    let pie_config = load_config()?;
    let config: ResolvedConfig = (cli.clone(), pie_config.clone()).try_into()?;

    #[allow(clippy::unwrap_used)]
    config::CONFIG.set(config).unwrap();
    let config = config::CONFIG.get().context("config should be set")?;

    let session = resolve_session(pool.clone(), cli.resume)?;
    let registry = registry::Registry::load();

    if let Some(cmd) = cli.command {
        init_stderr_subscriber(cli.debug, &config.log_level);
        match cmd {
            Commands::Status => {
                cmd::handle_status(config, &registry);
                return Ok(());
            }
            Commands::Skills => {
                cmd::handle_skills(&registry);
                return Ok(());
            }
        }
    }

    if format.is_some() {
        init_stderr_subscriber(cli.debug, &config.log_level);
    } else {
        init_file_subscriber(&session.id.to_string(), &config.log_level)?;
    }

    let sandbox_settings = build_sandbox(&pie_config);

    debug!(config = ?config, "config");
    let mut model = providers::build_from_resolved(&config.provider)?;

    let piped_stdin = read_piped_stdin();
    let format = if cli.json {
        OutputFormat::Json
    } else if cli.md {
        OutputFormat::Markdown
    } else {
        config.output_format
    };

    // --md or --json → single-shot mode (logs to stderr)
    // Otherwise → interactive mode (logs to .pie/logs/<session-id>.log)
    if format.is_explicit() {
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

        let mut session = Session::create(pool)?;
        let query = Instructions::new(full_query);
        return handler::handle_query(
            &mut model,
            &query,
            &mut session,
            format,
            sandbox_settings,
            config.max_steps,
            registry,
        )
        .await;
    }

    // Interactive mode: session-based REPL with file logging
    let session = resolve_session(pool, cli.resume)?;
    ui::tui::run_tui(
        model,
        config.provider.clone(),
        session,
        sandbox_settings,
        config.max_steps,
        pie_config,
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
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn resolve_session_creates_new_when_not_resuming() {
        let pool = Arc::new(db::create_memory_pool().unwrap());
        let session = resolve_session(pool, false).unwrap();
        assert!(session.history_entries().is_empty());
    }

    #[test]
    fn resolve_session_restores_when_resuming() {
        let pool = Arc::new(db::create_memory_pool().unwrap());
        let mut original = Session::create(pool.clone()).unwrap();
        original.add_user("hello").unwrap();
        original.add_assistant("world").unwrap();
        drop(original);

        let session = resolve_session(pool, true).unwrap();
        let entries = session.history_entries();
        assert_eq!(entries.len(), 2, "restored session should have history");
        assert_eq!(entries[0].content, "hello");
    }
}
