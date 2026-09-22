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
    let provider = resolve_agent_provider(agent.as_ref(), &config.provider, &config.model_tiers);
    let model = provider.build_client();

    // The agent's sandbox config layers on top of the configured one.
    let mut sandbox = (*build_sandbox(&pie_config)).clone();
    if let Some(agent_sandbox) = agent.as_ref().and_then(|a| a.sandbox.as_ref()) {
        sandbox.merge(agent_sandbox);
    }
    let sandbox = Arc::new(sandbox);

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
        let engine = RunEngine {
            registry,
            agent,
            model,
            sandbox_settings: sandbox,
        };
        run_single_shot(cli, config, session, format, engine).await
    } else {
        init_file_subscriber(&session.id.to_string(), &config.log_level)?;
        let setup = Interactive {
            pool,
            registry,
            sandbox,
            provider,
            retry: config.retry.clone(),
            agent_name: agent.map(|a| a.name),
            session,
            acp_agent: cli.acp_agent.clone(),
        };
        run_interactive(setup).await
    }
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
struct Interactive {
    pool: Arc<DbPool>,
    registry: Arc<Registry>,
    sandbox: Arc<SandboxConfig>,
    provider: config::ResolvedProvider,
    retry: config::RetryConfig,
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

/// Interactive mode: assemble an a2acp gateway in process — pie hosted
/// as its in-process agent by default, an external ACP agent's process
/// spec with `--acp-agent` — and hand the front door to the TUI. The
/// TUI's code path is identical either way; to the gateway the two
/// hosting modes are indistinguishable.
async fn run_interactive(setup: Interactive) -> anyhow::Result<()> {
    let registry = setup.registry.clone();
    let cwd = std::env::current_dir().context("cannot determine working directory")?;
    let history = setup.session.history_entries().to_vec();
    let session_id = pie_tui::SessionId::new(setup.session.id.to_string());
    let provider = pie_tui::ProviderView {
        name: setup.provider.name.clone(),
        model: setup.provider.model.clone(),
    };

    let mut config = tui_gateway_config();
    let mut in_process = std::collections::BTreeMap::new();
    if setup.acp_agent.is_empty() {
        let host: Arc<dyn a2acp::InProcessAgent> =
            Arc::new(pie_acp::PieHost::new(pie_acp::HostDeps {
                pool: setup.pool,
                registry: setup.registry,
                sandbox: setup.sandbox,
                provider: setup.provider,
                retry: setup.retry,
                agent_name: setup.agent_name,
                // The first turn continues the session pie resolved at
                // launch (so `--resume` and fresh starts both behave like
                // the pre-bridge TUI).
                resume: Some(setup.session.id),
            }));
        config.a2a.default_agent = "pie".into();
        in_process.insert("pie".to_string(), host);
    } else {
        let Some((program, args)) = setup.acp_agent.split_first() else {
            anyhow::bail!("--acp-agent needs a command to run");
        };
        let spec = a2acp::AgentSpec {
            command: program.clone(),
            args: args.to_vec(),
            ..a2acp::AgentSpec::new("", &[])
        };
        config.a2a.default_agent = "agent".into();
        config.agents.insert("agent".to_string(), spec);
    }
    let gateway = a2acp::a2a::gateway_from_config(&config, &in_process)?;
    let (client, events) = pie_tui::door::open(gateway.connect(), &config.a2a.default_agent, cwd);

    pie_tui::run_tui(pie_tui::TuiDeps {
        client,
        events,
        session_id,
        history,
        provider,
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
    // Only the fixtures below need it, so it lives here rather than at the
    // top, where it would read as an unused import in a non-test build.
    use pie_core::agent::OutputMode;

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
                .map(|n| Agent {
                    name: (*n).to_string(),
                    description: String::new(),
                    output_mode: OutputMode::default(),
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
}
