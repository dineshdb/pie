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
mod output;
mod prompt;
mod providers;
mod sandbox;
mod session;
mod skill;
mod tools;
mod ui;
mod utils;

use crate::config::logs_dir;
use crate::output::OutputFormat;
use crate::{db::DbPool, session::Session};
use clap::Parser;
use std::io::{self, IsTerminal, Read};
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "pie", version = "0.1.0")]
#[command(about = "Minimal Pi-like agent using OpenAI-compatible providers")]
#[allow(clippy::struct_excessive_bools)]
struct Cli {
    /// Explicitly use a specific skill
    #[arg(short, long)]
    skill: Option<String>,

    #[arg(short, long)]
    debug: bool,

    /// Output response in JSON format
    #[arg(long)]
    json: bool,

    /// Output response in Markdown format
    #[arg(long)]
    md: bool,

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

    /// Use persistent file-backed database instead of in-memory
    #[arg(long)]
    persistent: bool,
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
    dotenvy::dotenv().ok();

    let cli = Cli::parse();

    if cli.list_skills {
        cmd::handle_list_skills();
        return Ok(());
    }

    // Sandbox: required — exits if srt is not installed
    let sandbox_settings = sandbox::prepare(&sandbox::load_config())?;

    let mut model = providers::build_model(
        cli.model.as_deref(),
        cli.base_url.as_deref(),
        cli.api_key.as_deref(),
    )?;

    let persistent = cli.persistent || std::env::var("PERSISTENT").is_ok();
    let pool = Arc::new(db::create_pool(persistent)?);

    let piped_stdin = read_piped_stdin();

    let format = if cli.json {
        OutputFormat::Json
    } else if cli.md {
        OutputFormat::Markdown
    } else {
        OutputFormat::Default
    };

    // --md or --json → single-shot mode (logs to stderr)
    // Otherwise → interactive mode (logs to .pie/logs/<session-id>.log)
    if format.is_explicit() {
        init_stderr_subscriber(cli.debug);

        let cli_query = cli.query.join(" ");
        let query = if cli_query.is_empty() {
            piped_stdin.as_deref().unwrap_or_default().to_string()
        } else {
            cli_query
        };

        if cli.skill.is_some() && query.is_empty() {
            anyhow::bail!("Usage: pie -s <skill> '<query>'");
        }

        let full_query = match (&piped_stdin, query.is_empty()) {
            (Some(stdin), false) => format!("## Stdin\n```\n{stdin}\n```\n\n{query}"),
            (Some(stdin), true) => stdin.clone(),
            (_, false) => query,
            (_, true) => anyhow::bail!(
                "No query provided. Use `pie` for interactive mode or pass a query with --md or --json."
            ),
        };

        let mem_pool = Arc::new(db::create_pool(false)?);
        let mut session = Session::create(mem_pool)?;
        return handler::handle_query(
            &mut model,
            &full_query,
            &mut session,
            format,
            sandbox_settings,
        )
        .await;
    }

    // Interactive mode: session-based REPL with file logging
    let session = resolve_session(pool, cli.r#continue)?;
    init_file_subscriber(&session.id.to_string(), cli.debug);
    ui::tui::run_tui(model, session, sandbox_settings).await
}

fn init_stderr_subscriber(debug: bool) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(if debug { "debug" } else { "refinery=warn" }));

    let subscriber = tracing_subscriber::fmt()
        .with_target(false)
        .with_level(false)
        .with_env_filter(filter)
        .compact();

    if debug {
        subscriber.init();
    } else {
        subscriber.without_time().init();
    }
}

fn init_file_subscriber(session_id: &str, debug: bool) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(if debug { "debug" } else { "warn" }));

    let log_path = logs_dir().join(format!("{session_id}.log"));
    let file = match std::fs::File::create(&log_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "Warning: cannot create log file {}: {e}",
                log_path.display()
            );
            // Fall back to stderr
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .compact()
                .init();
            return;
        }
    };

    tracing_subscriber::fmt()
        .with_writer(file)
        .with_ansi(false)
        .with_target(true)
        .with_level(true)
        .with_env_filter(filter)
        .compact()
        .init();
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
