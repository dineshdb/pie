#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

/// The agent on the daemon's card that pie-tui drives: `pie` when the
/// daemon advertises it (the a2acp default config spawns `pie acp`),
/// else the card's first skill (the daemon's default agent) — an
/// honestly-driven agent beats a hard failure when the daemon is
/// configured for other agents.
#[cfg(feature = "tui")]
async fn pie_agent_on(door: &pie_tui::a2a::A2aClient) -> anyhow::Result<String> {
    let card = door
        .card()
        .await
        .map_err(|e| anyhow::anyhow!("the a2acp daemon's card is unreadable: {e}"))?;
    let skills = card
        .get("skills")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("the a2acp daemon's card lists no agents"))?;
    let ids: Vec<&str> = skills
        .iter()
        .filter_map(|skill| skill["id"].as_str())
        .collect();
    Ok(if ids.contains(&"pie") {
        "pie".to_string()
    } else {
        ids.first()
            .map(|id| (*id).to_string())
            .ok_or_else(|| anyhow::anyhow!("the a2acp daemon's card lists no agents"))?
    })
}

/// Interactive mode: connect the TUI to the a2acp daemon — ensuring one
/// runs first (the pidfile rendezvous under `XDG_RUNTIME_DIR`; a
/// healthy daemon is adopted, a missing one is spawned and waited on) —
/// and hand the daemon's HTTP door to the TUI. The daemon owns the
/// gateway and spawns `pie acp` per conversation (pie's agent core
/// lives behind `pie acp`); this process is just the frontend, so
/// a second `pie` in the same directory shares the same conversations.
#[cfg(feature = "tui")]
async fn run_interactive(setup: Interactive) -> anyhow::Result<()> {
    let Interactive {
        registry,
        agent_name,
        resume,
        yolo,
    } = setup;
    let cwd = std::env::current_dir().context("cannot determine working directory")?;
    let startup = agent_name
        .as_deref()
        .and_then(|name| registry.agents.iter().find(|a| a.name == name));
    let resolved = config::CONFIG.get().context("config should be set")?;
    let startup_provider =
        resolve_agent_provider(startup, &resolved.provider, &resolved.model_tiers);
    let provider = pie_tui::ProviderView {
        name: startup_provider.name.clone(),
        model: startup_provider.model.clone(),
    };
    let catalog = model_catalog(&startup_provider, &resolved.model_tiers);

    let base_url = pie_tui::a2a::ensure_daemon(&spawn_a2acp).await?;
    let door = pie_tui::a2a::A2aClient::new(base_url)?;
    let agent = pie_agent_on(&door).await?;
    let (context, history) = resume_lookup(&door, &cwd, resume).await;
    let session_id = pie_tui::SessionId::new(
        context
            .clone()
            .unwrap_or_else(|| format!("tui-{}", mint_session_suffix())),
    );

    let (client, events) = pie_tui::client::open(door, agent, cwd.clone()).await;
    // A resumed conversation continues on its gateway context; otherwise
    // the warm opens the agent's session before the first message (modes
    // on the card immediately), falling back to the lazy first-prompt
    // open when it fails.
    if let Some(context_id) = context {
        client.resume(context_id);
    } else {
        client.warm();
    }

    pie_tui::run_tui(pie_tui::TuiDeps {
        client,
        events,
        session_id,
        history,
        provider,
        catalog,
        registry,
        yolo,
    })
    .await
}

/// The spawn command for [`pie_tui::a2a::ensure_daemon`]: the `a2acp`
/// binary from `$PATH` with its own configuration (bind, agents,
/// permission mode are the daemon's business, not pie's).
#[cfg(feature = "tui")]
fn spawn_a2acp() -> std::process::Command {
    std::process::Command::new("a2acp")
}

/// The resume lookup over the daemon's HTTP endpoint: the directory's
/// most recent conversation plus its transcript (empty without
/// `--resume` or when nothing is persisted).
#[cfg(feature = "tui")]
async fn resume_lookup(
    door: &pie_tui::a2a::A2aClient,
    cwd: &std::path::Path,
    resume: bool,
) -> (Option<String>, Vec<HistoryEntry>) {
    if !resume {
        return (None, Vec::new());
    }
    let Some(context_id) = door.last_context_for_cwd(cwd).await else {
        return (None, Vec::new());
    };
    let turns = door.context_history(&context_id).await;
    let history = turns
        .into_iter()
        .enumerate()
        .map(|(n, message)| HistoryEntry {
            id: i64::try_from(n + 1).unwrap_or(i64::MAX),
            ts: i64::try_from(n + 1).unwrap_or(i64::MAX),
            role: match message.role.as_str() {
                "ROLE_AGENT" => Role::Assistant,
                _ => Role::User,
            },
            content: message.text,
        })
        .collect();
    (Some(context_id), history)
}

/// A short unique suffix for the TUI's local display id (the
/// input-history key) when no gateway conversation exists yet.
#[cfg(feature = "tui")]
fn mint_session_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    format!("{now:x}{seq:x}")
}

use anyhow::Context;
use clap::Parser;
use core::option::Option::Some;
use p1e_sandbox::SandboxConfig;
use pie_core::agent::Agent;
use pie_core::config::{ResolvedConfig, build_sandbox, load_config};
use pie_core::handler;
use pie_core::instructions::Instructions;
use pie_core::registry::Registry;
use pie_core::session::Session;
#[cfg(feature = "tui")]
use pie_core::session::{HistoryEntry, Role};
use pie_core::utils::output::OutputFormat;
use pie_core::{cmd, config};
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
    overrides: config::CliOverrides,

    /// Query to process
    query: Vec<String>,

    /// Continue the last session for this directory
    #[arg(short, long, global = true)]
    resume: bool,

    /// Interactive mode only: auto-approve permission asks (yolo mode).
    /// Toggleable at runtime with `/yolo`.
    #[arg(long, global = true)]
    yolo: bool,
}

#[derive(clap::Subcommand, Clone)]
enum Commands {
    /// Show current configuration and system status
    Status,
    /// Show LLM usage and cost per model (bookkeeping; needs the store)
    #[cfg(feature = "acp")]
    Usage {
        /// Only include runs from the last N days (0 = all time)
        #[arg(long)]
        days: Option<u32>,
    },
    /// List available skills and agents
    Skills,
    /// Launch another agent with current provider environment
    Launch {
        /// Command and arguments to execute
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        all_args: Vec<String>,
    },
    /// Manage MCP server OAuth authorization (needs the store)
    #[cfg(feature = "acp")]
    Mcp {
        #[command(subcommand)]
        command: cmd::McpCommand,
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
    /// Serve the Agent Client Protocol (ACP) over stdio, for editor clients
    #[cfg(feature = "acp")]
    Acp,
}

impl Cli {
    pub fn output_format(&self) -> OutputFormat {
        self.overrides.output_format()
    }
}

/// Run the PIE agent.
///
/// # Errors
///
/// Returns an error if:
/// - Configuration cannot be loaded or resolved.
/// - The pie-local store (`~/.pie/pie.db`) cannot be opened.
/// - Subscriber initialization fails.
/// - The command or interactive session fails.
pub async fn run() -> anyhow::Result<()> {
    let mut timing: Vec<(&'static str, std::time::Duration)> = Vec::new();
    let t0 = std::time::Instant::now();

    let mut cli = Cli::parse();
    let format = cli.output_format();

    let t = std::time::Instant::now();
    let pie_config = load_config()?;
    let config: ResolvedConfig = (cli.overrides.clone(), pie_config.clone()).try_into()?;
    timing.push(("config_resolve", t.elapsed()));

    config::CONFIG
        .set(config)
        .map_err(|_| anyhow::anyhow!("global config already initialized"))?;

    let config = config::CONFIG.get().context("config should be set")?;

    let t = std::time::Instant::now();
    let registry = Registry::load();
    timing.push(("registry_load", t.elapsed()));
    let base_sandbox = build_sandbox(&pie_config);
    if let Some(cmd) = cli.command {
        return handle_command(cmd, config, &registry).await;
    }

    // `pie <agent> [query...]`: a first token matching an agent name selects
    // that agent; the rest (or piped stdin) is the query.
    let agent = extract_agent(&registry, &mut cli);

    // Stateless pie: the conversation's durable transcript lives in the
    // gateway's agent filesystem. The in-memory session here is the
    // process's working memory — hydrated from the gateway on --resume,
    // never persisted locally.
    let cwd = std::env::current_dir()?;
    let session = Session::new(&cwd);
    timing.push(("session_resolve", t.elapsed()));
    timing.push(("startup_total", t0.elapsed()));

    let has_query = !cli.query.is_empty() || !io::stdin().is_terminal();
    if format.is_explicit() || has_query {
        init_stderr_subscriber(cli.overrides.debug, &config.log_level);
        for (phase, dur) in &timing {
            tracing::debug!(
                phase,
                ms = pie_core::utils::ms_of(*dur),
                "timing: startup phase"
            );
        }
        let provider =
            resolve_agent_provider(agent.as_ref(), &config.provider, &config.model_tiers);
        let engine = RunEngine {
            registry,
            model: provider.build_client(),
            // The agent's sandbox config layers on top of the base one.
            sandbox_settings: merged_sandbox(&base_sandbox, agent.as_ref()),
            agent,
        };
        run_single_shot(cli, config, session, format, engine).await
    } else {
        #[cfg(not(feature = "tui"))]
        {
            anyhow::bail!(
                "this build has no interactive frontend (compile with the `tui` feature)"
            );
        }
        #[cfg(feature = "tui")]
        {
            init_file_subscriber(&session.id.to_string(), &config.log_level)?;
            let setup = Interactive {
                registry,
                agent_name: agent.map(|a| a.name),
                resume: cli.resume,
                yolo: cli.yolo,
            };
            run_interactive(setup).await
        }
    }
}

/// The agent's sandbox config layered on top of the base one. The
/// merged provider decides the execution provider for the run.
fn merged_sandbox(base: &Arc<SandboxConfig>, agent: Option<&Agent>) -> Arc<SandboxConfig> {
    let mut sandbox = (**base).clone();
    if let Some(agent_sandbox) = agent.and_then(|a| a.sandbox.as_ref()) {
        sandbox.merge(agent_sandbox);
    }
    Arc::new(sandbox)
}

/// Peel the first query token off if it names an agent.
fn extract_agent(registry: &Registry, cli: &mut Cli) -> Option<Agent> {
    let name = cli.query.first()?;
    let agent = registry.agents.iter().find(|a| &a.name == name)?;
    cli.query.remove(0);
    Some(agent.clone())
}

/// Apply an agent's `model:` — a configured tier name wins, otherwise it is
/// a literal model id on the default provider.
fn resolve_agent_provider(
    agent: Option<&Agent>,
    default: &config::ResolvedProvider,
    tiers: &std::collections::HashMap<String, config::ResolvedProvider>,
) -> config::ResolvedProvider {
    let Some(model) = agent.and_then(|a| a.model.as_deref()) else {
        return default.clone();
    };
    tiers
        .get(model)
        .cloned()
        .unwrap_or_else(|| default.clone().with_model(model.to_string()))
}

/// Dispatch a subcommand. Async only because the `acp` arms open the
/// store; without that feature every remaining arm is synchronous.
#[cfg_attr(not(feature = "acp"), allow(clippy::unused_async))]
async fn handle_command(
    cmd: Commands,
    config: &ResolvedConfig,
    registry: &Arc<Registry>,
) -> anyhow::Result<()> {
    // Commands that don't need interactive UI want stderr logging
    init_stderr_subscriber(config.debug, &config.log_level);

    match cmd {
        Commands::Status => {
            cmd::handle_status(config, registry);
            Ok(())
        }
        #[cfg(feature = "acp")]
        Commands::Usage { days } => {
            // The only database consumers are the ones below — the
            // interactive TUI stays stateless and never opens pie.db.
            let store = Arc::new(pie_acp::store::create_persistent_pool().await?);
            let usage: Arc<dyn pie_core::store::UsageStore> = store.clone();
            cmd::handle_usage(config, &usage, days).await
        }
        Commands::Skills => {
            cmd::handle_skills(config, registry);
            Ok(())
        }
        Commands::Launch { all_args } => cmd::handle_launch(config, &all_args),
        #[cfg(feature = "acp")]
        Commands::Mcp { command } => {
            let store = Arc::new(pie_acp::store::create_persistent_pool().await?);
            let tokens: Arc<dyn pie_core::store::TokenStore> = store.clone();
            cmd::handle_mcp(command, config, &tokens).await
        }
        Commands::Exec { skill, script } => cmd::handle_exec(config, registry, skill, &script),
        #[cfg(feature = "acp")]
        Commands::Acp => {
            let store = Arc::new(pie_acp::store::create_persistent_pool().await?);
            let usage: Arc<dyn pie_core::store::UsageStore> = store.clone();
            let tokens: Arc<dyn pie_core::store::TokenStore> = store.clone();
            pie_acp::serve_stdio(usage, tokens, registry.clone(), config).await
        }
    }
}

/// The engine dependencies shared by single-shot runs: the model handle,
/// the sandbox and the registry/agent selection resolved at startup.
struct RunEngine {
    registry: Arc<Registry>,
    agent: Option<Agent>,
    model: agentsdk::OpenAI,
    sandbox_settings: Arc<SandboxConfig>,
}

async fn run_single_shot(
    cli: Cli,
    config: &ResolvedConfig,
    session: Session,
    format: OutputFormat,
    engine: RunEngine,
) -> anyhow::Result<()> {
    trace!(config = ?config, "config");
    let piped_stdin = read_piped_stdin();

    let cli_query = cli.query.join(" ");
    if cli_query.is_empty() && piped_stdin.is_none() {
        anyhow::bail!(
            "No query provided. Use `pie` for interactive mode or pass a query with --md or --json."
        );
    }

    // Stateless resume: the conversation's durable transcript is the
    // gateway's, so resuming assembles it (read-only plumbing for the
    // history lookup) and hydrates the session. Without --resume there
    // is nothing to look up — and assembling would only contend on the
    // gateway's store while a server holds it.
    if cli.resume {
        anyhow::bail!(
            "--resume on single-shot requires the pie daemon: run `pie` (interactive) once, \
             or `pie server`, then retry"
        );
    }

    // One store, both seams: usage bookkeeping + MCP OAuth grants. The
    // single-shot CLI records runs; the interactive TUI never gets here.
    // An `acp`-less build carries no database — usage stays in memory.
    #[cfg(feature = "acp")]
    let store = Arc::new(pie_acp::store::create_persistent_pool().await?);
    #[cfg(feature = "acp")]
    let usage: Arc<dyn pie_core::store::UsageStore> = store.clone();
    #[cfg(feature = "acp")]
    let tokens: Arc<dyn pie_core::store::TokenStore> = store.clone();
    #[cfg(not(feature = "acp"))]
    let usage: Arc<dyn pie_core::store::UsageStore> = Arc::new(pie_core::store::MemoryStore::new());
    #[cfg(not(feature = "acp"))]
    let tokens: Arc<dyn pie_core::store::TokenStore> =
        Arc::new(pie_core::store::MemoryStore::new());

    let full_query = match (piped_stdin.as_deref(), cli_query.is_empty()) {
        (Some(stdin), false) => format!("## Stdin\n```\n{stdin}\n```\n\n{cli_query}"),
        (Some(stdin), true) => stdin.to_string(),
        (None, _) => cli_query,
    };

    let query = Instructions::new(full_query);
    handler::handle_query(handler::HandleParams {
        model: engine.model,
        query,
        session,
        usage,
        tokens,
        format,
        sandbox_settings: engine.sandbox_settings,
        retry: config.retry.clone(),
        registry: engine.registry,
        agent_name: engine.agent.map(|a| a.name),
    })
    .await
}

/// Everything interactive mode needs to open its A2A client.
#[cfg(feature = "tui")]
struct Interactive {
    registry: Arc<Registry>,
    agent_name: Option<String>,
    /// Continue this directory's most recent conversation from the
    /// gateway's history (`--resume`).
    resume: bool,
    /// Start the TUI in yolo mode: permission asks are auto-approved.
    yolo: bool,
}

/// The startup model catalog for the TUI's `/model` picker: the
/// `"default"` entry (the startup provider and model) plus one entry
/// per configured `[model.<name>]` tier in name order — plain data,
/// matching what the agent-side resolver accepts.
#[cfg(feature = "tui")]
fn model_catalog(
    startup: &config::ResolvedProvider,
    tiers: &std::collections::HashMap<String, config::ResolvedProvider>,
) -> pie_tui::ModelCatalog {
    let mut names: Vec<&String> = tiers.keys().collect();
    names.sort_unstable();
    pie_tui::ModelCatalog {
        entries: std::iter::once(pie_tui::CatalogEntry {
            id: pie_tui::client::DEFAULT_MODEL_SELECTION.to_string(),
            model: startup.model.clone(),
        })
        .chain(names.into_iter().map(|name| pie_tui::CatalogEntry {
            id: name.clone(),
            model: tiers[name].model.clone(),
        }))
        .collect(),
    }
}

fn default_env_filter(default_level: &str) -> EnvFilter {
    // rmcp logs every MCP handshake at INFO with full peer metadata —
    // connection noise, only useful while debugging.
    let filter_str = match default_level {
        "debug" => "warn,p1e=debug,pie=debug,rmcp=debug".to_string(),
        others => format!("{others},rmcp=warn"),
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

#[cfg(feature = "tui")]
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

    #[test]
    fn rmcp_handshake_logs_are_capped_below_info() {
        let filter = default_env_filter("info").to_string();
        assert!(filter.contains("rmcp=warn"), "{filter}");

        let filter = default_env_filter("warn").to_string();
        assert!(filter.contains("rmcp=warn"), "{filter}");
    }

    #[test]
    fn debug_level_unmutes_rmcp() {
        let filter = default_env_filter("debug").to_string();
        assert!(filter.contains("rmcp=debug"), "{filter}");
    }

    fn registry_with(names: &[&str]) -> Registry {
        Registry {
            agents: names
                .iter()
                .map(|name| Agent {
                    name: (*name).to_string(),
                    description: String::new(),
                    output_mode: pie_core::agent::OutputMode::default(),
                    model: None,
                    temperature: None,
                    content: String::new(),
                    needs: Vec::new(),
                    tools: Vec::new(),
                    sandbox: None,
                    grants: Vec::new(),
                    readonly: false,
                    plugins: None,
                    skills_paths: Vec::new(),
                    max_steps: None,
                })
                .collect(),
            skills: Vec::new(),
            completions: Vec::new(),
        }
    }

    fn cli_with_query(query: &str) -> Cli {
        Cli {
            command: None,
            overrides: config::CliOverrides::default(),
            query: query.split_whitespace().map(ToString::to_string).collect(),
            resume: false,
            yolo: false,
        }
    }

    #[test]
    fn extract_agent_peels_matching_first_token() {
        let registry = registry_with(&["review", "explore"]);
        let mut cli = cli_with_query("review this code please");
        let agent = extract_agent(&registry, &mut cli);
        assert_eq!(agent.expect("agent").name, "review");
        assert_eq!(cli.query, vec!["this", "code", "please"]);
    }

    #[test]
    fn extract_agent_ignores_non_agent_first_token() {
        let registry = registry_with(&["review"]);
        let mut cli = cli_with_query("reviewing some code");
        assert!(extract_agent(&registry, &mut cli).is_none());
        assert_eq!(cli.query.len(), 3);
    }

    #[test]
    fn extract_agent_from_bare_name() {
        let registry = registry_with(&["explore"]);
        let mut cli = cli_with_query("explore");
        assert_eq!(
            extract_agent(&registry, &mut cli).expect("agent").name,
            "explore"
        );
        assert!(cli.query.is_empty());
    }

    #[test]
    fn tier_name_wins_over_literal_model() {
        let base = registry_with(&["a"]).agents.into_iter().next().unwrap();
        let agent = Agent {
            model: Some("deep".into()),
            ..base.clone()
        };
        let mut tiers = std::collections::HashMap::new();
        tiers.insert(
            "deep".to_string(),
            config::ResolvedProvider {
                name: "deep".into(),
                model: "claude-opus".into(),
                anthropic_url: None,
                openai_url: "http://deep".parse().unwrap(),
                api_key: redact::Secret::new("k".into()),
                temperature: None,
            },
        );
        let default = config::ResolvedProvider {
            name: "default".into(),
            model: "gpt".into(),
            anthropic_url: None,
            openai_url: "http://default".parse().unwrap(),
            api_key: redact::Secret::new("k".into()),
            temperature: None,
        };

        let resolved = resolve_agent_provider(Some(&agent), &default, &tiers);
        assert_eq!(resolved.model, "claude-opus");

        let literal = Agent {
            model: Some("my-model-x".into()),
            ..base
        };
        let resolved = resolve_agent_provider(Some(&literal), &default, &tiers);
        assert_eq!(resolved.model, "my-model-x");
        assert_eq!(resolved.name, "default");

        let resolved = resolve_agent_provider(None, &default, &tiers);
        assert_eq!(resolved.model, "gpt");
    }
}
