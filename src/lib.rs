#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

use anyhow::Context;
use clap::Parser;
use core::option::Option::Some;
use p1e_sandbox::SandboxConfig;
use pie_core::agent::Agent;
use pie_core::config::{ResolvedConfig, build_sandbox, load_config};
use pie_core::db::DbPool;
use pie_core::error::Result;
use pie_core::handler;
use pie_core::instructions::Instructions;
use pie_core::registry::Registry;
use pie_core::session::Session;
use pie_core::utils::output::OutputFormat;
use pie_core::{cmd, config, cron, db};
use std::io::{self, IsTerminal, Read};
use std::sync::Arc;
use tracing::trace;
use tracing_subscriber::EnvFilter;

mod roster;
mod server;
mod server_service;

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

    /// Run the TUI against an external ACP agent instead of the
    /// in-process engine: command and arguments, e.g. `pie --acp-agent
    /// pie acp`
    #[arg(long = "acp-agent", value_name = "COMMAND [ARGS]", num_args = 1..)]
    acp_agent: Vec<String>,
}

#[derive(clap::Subcommand, Clone)]
enum Commands {
    /// Show current configuration and system status
    Status,
    /// Show LLM usage and cost per model (bookkeeping)
    Usage {
        /// Only include runs from the last N days (0 = all time)
        #[arg(long)]
        days: Option<u32>,
    },
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
        command: cmd::CronCommand,
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
    Acp,
    /// Serve the A2A agent-to-agent protocol over streamable HTTP
    Server {
        /// Bind address override ([server] bind in pie.toml by default)
        #[arg(long, global = true)]
        bind: Option<String>,
        /// Hostname remote clients will use (`allowed_hosts`; install only)
        #[arg(long, global = true)]
        host: Vec<String>,
        #[command(subcommand)]
        command: Option<ServerCommand>,
    },
}

#[derive(clap::Subcommand, Clone, Debug)]
enum ServerCommand {
    /// Install the daemon as a login service (launchd / systemd user unit)
    Install,
    /// Remove the installed service
    Uninstall,
}

impl Cli {
    pub fn output_format(&self) -> OutputFormat {
        self.overrides.output_format()
    }
}

async fn resolve_session(pool: Arc<DbPool>, resume: bool) -> Result<Session> {
    let cwd = std::env::current_dir()?;
    if resume
        && let Some(session) =
            Session::find_latest_for_cwd(pool.clone(), &cwd.to_string_lossy()).await?
    {
        return Ok(session);
    }
    Session::create(pool, &cwd).await
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
    let mut timing: Vec<(&'static str, std::time::Duration)> = Vec::new();
    let t0 = std::time::Instant::now();

    let mut cli = Cli::parse();
    let format = cli.output_format();

    let pool = Arc::new(db::create_persistent_pool().await?);
    timing.push(("db_pool", t0.elapsed()));

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
    let server_config = pie_config.server.clone();
    let base_sandbox = build_sandbox(&pie_config);
    if let Some(cmd) = cli.command {
        return handle_command(
            cmd,
            config,
            &registry,
            pool.clone(),
            &server_config,
            &base_sandbox,
        )
        .await;
    }

    // `pie <agent> [query...]`: a first token matching an agent name selects
    // that agent; the rest (or piped stdin) is the query.
    let agent = extract_agent(&registry, &mut cli);

    let t = std::time::Instant::now();
    let session = resolve_session(pool.clone(), cli.resume).await?;
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
        init_file_subscriber(&session.id.to_string(), &config.log_level)?;
        let setup = Interactive {
            pool,
            registry,
            base_sandbox,
            config,
            agent_name: agent.map(|a| a.name),
            session,
            acp_agent: cli.acp_agent.clone(),
        };
        run_interactive(setup).await
    }
}

/// The agent's sandbox config layered on top of the base one.
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

async fn handle_command(
    cmd: Commands,
    config: &ResolvedConfig,
    registry: &Arc<Registry>,
    pool: Arc<DbPool>,
    server_config: &config::ServerConfig,
    base_sandbox: &Arc<SandboxConfig>,
) -> anyhow::Result<()> {
    // Commands that don't need interactive UI usually want stderr logging
    if !matches!(cmd, Commands::Daemon { .. }) || config.debug {
        init_stderr_subscriber(config.debug, &config.log_level);
    }

    match cmd {
        Commands::Status => {
            cmd::handle_status(config, registry, server_config);
            Ok(())
        }
        Commands::Usage { days } => cmd::handle_usage(config, pool, days).await,
        Commands::Skills => {
            cmd::handle_skills(config, registry);
            Ok(())
        }
        Commands::Launch {
            all_args,
            no_sandbox,
        } => cmd::handle_launch(config, &all_args, no_sandbox),
        Commands::Cron { command } => cmd::handle_cron(command, pool, registry.clone()).await,
        Commands::Exec { skill, script } => cmd::handle_exec(config, registry, skill, &script),
        Commands::Acp => pie_acp::serve_stdio(pool, registry.clone(), config).await,
        Commands::Server {
            bind,
            host,
            command,
        } => match command {
            Some(ServerCommand::Install) => server_service::install(bind, &host, server_config),
            Some(ServerCommand::Uninstall) => server_service::uninstall(),
            None => {
                server::serve(
                    bind,
                    server::ServerDeps {
                        pool,
                        registry: registry.clone(),
                        sandbox: base_sandbox.clone(),
                    },
                    server_config.clone(),
                    config,
                )
                .await
            }
        },
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

/// The engine dependencies shared by both run paths (single-shot and
/// interactive): the model handle, the sandbox and the registry/agent
/// selection resolved at startup.
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
        format,
        sandbox_settings: engine.sandbox_settings,
        retry: config.retry.clone(),
        registry: engine.registry,
        agent_name: engine.agent.map(|a| a.name),
    })
    .await
}

/// Everything interactive mode needs to open its A2A door.
struct Interactive<'a> {
    pool: Arc<DbPool>,
    registry: Arc<Registry>,
    /// The base sandbox, BEFORE any agent's merge — the roster factory
    /// merges each entry's own agent sandbox onto it.
    base_sandbox: Arc<SandboxConfig>,
    config: &'a ResolvedConfig,
    agent_name: Option<String>,
    session: Session,
    /// External ACP agent to run instead of the in-process engine
    /// (`--acp-agent`); empty means in-process.
    acp_agent: Vec<String>,
}

/// How long the interactive gateway keeps a conversation's agent session
/// alive between turns: effectively forever — the TUI's conversation is
/// the session, and an idle re-mint would open a fresh (amnesiac) one.
/// TODO(a2acp): re-minted sessions should resume the conversation's
/// session (`session/load`) instead of relying on a long grace.
const TUI_IDLE_GRACE_SECS: u64 = 31_536_000; // one year

/// The a2acp gateway config every TUI door runs on: permission asks are
/// forwarded (the TUI answers them), no process agents unless
/// `--acp-agent` provides one.
fn tui_gateway_config() -> a2acp::Config {
    a2acp::Config {
        permission: a2acp::PermissionMode::Ask,
        agents: std::collections::BTreeMap::new(),
        idle_grace_secs: TUI_IDLE_GRACE_SECS,
        ..a2acp::Config::default()
    }
}

/// The interactive door's in-process roster: the default `pie` entry —
/// the startup-selected agent, resuming the startup conversation — plus
/// the addressable registry roster ([`roster::install`]). The TUI keeps
/// driving the default entry; only the gateway's addressable roster
/// grows.
fn tui_roster(
    config: &mut a2acp::Config,
    deps: &roster::RosterDeps<'_>,
    startup: Option<&Agent>,
    startup_session: pie_core::session::SessionId,
) -> std::collections::BTreeMap<String, Arc<dyn a2acp::InProcessAgent>> {
    let mut default = deps.host_deps(startup);
    default.resume = Some(startup_session);
    roster::install(config, deps, default)
}

/// The startup model catalog for the TUI's `/model` picker: the
/// `"default"` entry (the startup provider and model) plus one entry
/// per configured `[model.<name>]` tier in name order — plain data,
/// matching what the agent-side resolver accepts.
fn model_catalog(
    startup: &config::ResolvedProvider,
    tiers: &std::collections::HashMap<String, config::ResolvedProvider>,
) -> pie_tui::ModelCatalog {
    let mut names: Vec<&String> = tiers.keys().collect();
    names.sort_unstable();
    pie_tui::ModelCatalog {
        entries: std::iter::once(pie_tui::CatalogEntry {
            id: pie_acp::DEFAULT_MODEL_SELECTION.to_string(),
            model: startup.model.clone(),
        })
        .chain(names.into_iter().map(|name| pie_tui::CatalogEntry {
            id: name.clone(),
            model: tiers[name].model.clone(),
        }))
        .collect(),
    }
}

/// Interactive mode: assemble an a2acp gateway in process — pie's agent
/// roster hosted as the gateway's in-process agents by default (the
/// TUI's own session on the default entry), an external ACP agent's
/// process spec with `--acp-agent` — and hand the front door to the
/// TUI. The TUI's code path is identical either way; to the gateway the
/// two hosting modes are indistinguishable.
async fn run_interactive(setup: Interactive<'_>) -> anyhow::Result<()> {
    let Interactive {
        pool,
        registry,
        base_sandbox,
        config: resolved,
        agent_name,
        session,
        acp_agent,
    } = setup;
    let cwd = std::env::current_dir().context("cannot determine working directory")?;
    let history = session.history_entries().to_vec();
    let session_id = pie_tui::SessionId::new(session.id.to_string());
    let deps = roster::RosterDeps {
        pool,
        registry: Arc::clone(&registry),
        sandbox: base_sandbox,
        config: resolved,
    };
    let startup = agent_name
        .as_deref()
        .and_then(|name| registry.agents.iter().find(|a| a.name == name));
    let startup_provider =
        resolve_agent_provider(startup, &resolved.provider, &resolved.model_tiers);
    let provider = pie_tui::ProviderView {
        name: startup_provider.name.clone(),
        model: startup_provider.model.clone(),
    };
    let catalog = model_catalog(&startup_provider, &resolved.model_tiers);

    let mut config = tui_gateway_config();
    let in_process = if acp_agent.is_empty() {
        config.a2a.default_agent = roster::PIE_AGENT.into();
        tui_roster(&mut config, &deps, startup, session.id)
    } else {
        let Some((program, args)) = acp_agent.split_first() else {
            anyhow::bail!("--acp-agent needs a command to run");
        };
        let spec = a2acp::AgentSpec {
            command: program.clone(),
            args: args.to_vec(),
            ..a2acp::AgentSpec::new("", &[])
        };
        config.a2a.default_agent = "agent".into();
        config.agents.insert("agent".to_string(), spec);
        std::collections::BTreeMap::new()
    };
    let gateway = a2acp::a2a::gateway_from_config(&config, &in_process)?;
    let (client, events) = pie_tui::door::open(gateway.connect(), &config.a2a.default_agent, cwd);

    pie_tui::run_tui(pie_tui::TuiDeps {
        client,
        events,
        session_id,
        history,
        provider,
        catalog,
        registry,
    })
    .await
}

fn default_env_filter(default_level: &str) -> EnvFilter {
    // rmcp logs every MCP handshake at INFO with full peer metadata —
    // connection noise, only useful while debugging.
    let filter_str = match default_level {
        "debug" => "warn,p1e=debug,pie=debug,p1e_sandbox=debug,rmcp=debug".to_string(),
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
                .map(|n| roster::tests::minimal_agent(n))
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
            acp_agent: Vec::new(),
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

    #[tokio::test]
    async fn the_interactive_door_assembles_the_addressable_roster() {
        let registry = registry_with(&["review", "explore"]);
        let pool = Arc::new(db::create_test_pool().await.unwrap());
        let session = Session::create(pool.clone(), std::path::Path::new("/tmp/roster-tui"))
            .await
            .unwrap();
        let resolved = ResolvedConfig {
            provider: config::ResolvedProvider {
                name: "default".into(),
                model: "gpt".into(),
                anthropic_url: None,
                openai_url: "http://127.0.0.1:9/v1".parse().unwrap(),
                api_key: redact::Secret::new("k".into()),
                temperature: None,
            },
            retry: config::RetryConfig::default(),
            model_tiers: std::collections::HashMap::new(),
            mcp: std::collections::HashMap::new(),
            pricing: std::collections::HashMap::new(),
            output_format: OutputFormat::default(),
            log_level: "warn".to_string(),
            debug: false,
        };
        let deps = roster::RosterDeps {
            pool,
            registry: Arc::new(registry),
            sandbox: Arc::new(SandboxConfig::default()),
            config: &resolved,
        };

        // The TUI's own default: the startup-selected agent (here
        // `review`), resuming the startup session.
        let startup = deps.registry.agents.iter().find(|a| a.name == "review");
        let mut config = tui_gateway_config();
        let hosts = tui_roster(&mut config, &deps, startup, session.id);

        // The TUI drives the default entry; the crate's own default
        // agrees with pie's.
        assert_eq!(config.a2a.default_agent, roster::PIE_AGENT);
        let mut names: Vec<&str> = hosts.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["explore", "pie", "review"]);
        assert!(
            config.agents.contains_key("review") && config.agents.contains_key("pie"),
            "the card's display specs ride the config"
        );
    }
}
