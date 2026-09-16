use crate::agent::AgentEvent;
use crate::config::CONFIG;
use crate::config::McpServerConfig;
use crate::db::DbPool;
use crate::error::{AppError, Result};
use crate::plugin::{
    AgentMode, GateAsk, HelperBinariesPlugin, ModePlugin, PermissionRequest, PersistencePlugin,
    ToolGatePlugin, ToolGrants, UserCommandPlugin, WebsearchPlugin,
};
use crate::prompt::SystemPrompt;
use crate::registry::Registry;
use crate::session::Session;
use crate::usage::RunUsage;
use agentsdk::core::Sandbox;
use agentsdk::{Agent as SdkAgent, MemoryHistoryPlugin, Message};
use agentsdk_plugin_fs::{FileSystemPlugin, ReadOnlyFileSystemPlugin};
use agentsdk_plugin_jewels::JewelsPlugin;
use agentsdk_plugin_mcp::McpPlugin;
use agentsdk_plugin_shell::ShellPlugin;
use agentsdk_plugin_skills::SkillsPlugin;
use futures::future::BoxFuture;
use p1e_sandbox::{Permission, SandboxConfig};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

use super::definition::Agent;

/// Which filesystem plugin variant a run gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum FsMode {
    #[default]
    Full,
    Readonly,
    Off,
}

/// Which configured `[mcp.*]` servers a run connects to.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum McpSelection {
    #[default]
    Off,
    /// Default runs: every server configured under `[mcp.*]`, connected
    /// best-effort — a server that is down costs a warning in the session
    /// log, never the run. Configuring a server is opt-out (drop the
    /// section), not opt-in.
    Available,
    /// Explicit `plugins: [mcp]`: every configured server, and any
    /// connection failure fails the run loudly.
    All,
    /// Explicit `plugins: ["mcp:<name>"]`: only the named servers, as
    /// strict as [`McpSelection::All`].
    Only(Vec<String>),
}

/// The optional, selectable plugins. Defaults = the full set (markdown
/// agents and agent-less runs), including best-effort connections to every
/// configured `[mcp.*]` server; [`PluginSelection::none`] = agents that
/// opt in explicitly. Agents name `mcp`/`mcp:<server>` for the strict,
/// fail-loud contract.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PluginSelection {
    fs: FsMode,
    flags: PluginFlags,
    mcp: McpSelection,
}

/// Bitflags-style on/off set for the toggleable plugins (`fs` and `mcp`
/// carry state beyond a bool, so they live on [`PluginSelection`] itself).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct PluginFlags(u8);

impl PluginFlags {
    const SHELL: Self = Self(1 << 0);
    const WEBSEARCH: Self = Self(1 << 1);
    const SKILLS: Self = Self(1 << 2);
    const AGENTSMD: Self = Self(1 << 3);

    const fn set(self, flag: Self, on: bool) -> Self {
        if on {
            Self(self.0 | flag.0)
        } else {
            Self(self.0 & !flag.0)
        }
    }

    const fn get(self, flag: Self) -> bool {
        self.0 & flag.0 != 0
    }

    /// Every toggleable plugin on.
    const fn all() -> Self {
        Self(Self::SHELL.0 | Self::WEBSEARCH.0 | Self::SKILLS.0 | Self::AGENTSMD.0)
    }
}

impl std::ops::Index<PluginFlags> for PluginSelection {
    type Output = bool;

    fn index(&self, flag: PluginFlags) -> &bool {
        // Read-only accessor over the bitfield: the returned `&bool` is
        // from a const-evaluated match, not a field of self.
        if self.flags.get(flag) { &true } else { &false }
    }
}

impl Default for PluginSelection {
    fn default() -> Self {
        Self {
            fs: FsMode::Full,
            flags: PluginFlags::all(),
            mcp: McpSelection::Available,
        }
    }
}

impl PluginSelection {
    fn none() -> Self {
        Self {
            fs: FsMode::Off,
            flags: PluginFlags::default(),
            mcp: McpSelection::Off,
        }
    }
}

#[derive(Clone)]
pub struct PieAgent {
    pub model: agentsdk::OpenAI,
    pub registry: Arc<Registry>,
    pub sandbox: Arc<SandboxConfig>,
    pub session: Session,
    pub config: AgentConfig,
    permission_tx: Option<UnboundedSender<PermissionRequest>>,
    /// Approval channel for the gated tools + the "always allow" memory
    /// shared with the approver (one set per ACP session, so a grant
    /// outlives one turn).
    tool_gate: Option<ToolGate>,
}

/// The channel pair [`PieAgent`] keeps when an out-of-band approver gates
/// the machine-changing tools (see [`crate::plugin::ToolGatePlugin`]).
type ToolGate = (UnboundedSender<GateAsk>, ToolGrants);

#[derive(Debug, Clone, Deserialize)]
pub struct AgentConfig {
    pub agent_name: Option<String>,
    #[allow(dead_code)]
    pub history_limit: u32,
    pub depth: u32,
    #[allow(dead_code)]
    pub max_retries: u32,
    pub retry: crate::config::RetryConfig,
    #[serde(default)]
    pub grants: HashSet<Permission>,
    /// Operating mode the run starts in. `None` = build (the default).
    /// A remote client (ACP) seeds this from `session/set_mode`.
    #[serde(default)]
    pub mode: Option<AgentMode>,
    /// May the agent change its own mode mid-run? Frontends that own a mode
    /// selector (ACP) say no: they get no `switch_mode` tool, and because the
    /// mode is then fixed for the whole run, the tools it forbids are left
    /// out of the advertised list instead of failing when called.
    #[serde(default = "default_true")]
    pub mode_switching: bool,
    /// The directory this run happens in: sandbox entries, fs/shell tool
    /// paths, repo discovery, and the `<pwd>` prompt block all key off it.
    /// `None` = the process working directory (the CLI frontends).
    #[serde(default)]
    pub cwd: Option<std::path::PathBuf>,
}

fn default_true() -> bool {
    true
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            agent_name: None,
            history_limit: 10,
            depth: 0,
            max_retries: 3,
            retry: crate::config::RetryConfig::default(),
            grants: HashSet::new(),
            mode: None,
            mode_switching: true,
            cwd: None,
        }
    }
}

impl AgentConfig {
    pub fn is_debug() -> bool {
        CONFIG.get().is_some_and(|c| c.debug)
    }
}

/// One completed interaction: the final text plus the LLM usage it cost.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub text: String,
    pub usage: RunUsage,
    /// USD cost at the configured per-model pricing; `None` when the model
    /// has no `[pricing.*]` entry or the provider reported no usage.
    pub cost_usd: Option<f64>,
}

impl PieAgent {
    pub fn new(
        model: agentsdk::OpenAI,
        registry: Arc<Registry>,
        sandbox: Arc<SandboxConfig>,
        session: Session,
        config: AgentConfig,
    ) -> Self {
        Self {
            model,
            registry,
            sandbox,
            session,
            config,
            permission_tx: None,
            tool_gate: None,
        }
    }

    pub fn with_permission_channel(mut self, tx: UnboundedSender<PermissionRequest>) -> Self {
        self.permission_tx = Some(tx);
        self
    }

    /// Route Write/Edit/Bash tool calls through an out-of-band approver (an
    /// ACP client today; a TUI dialog could be next). The approver answers
    /// `true`/`false`; `grants` is the shared "always allow" memory.
    pub fn with_tool_gate(mut self, gate: ToolGate) -> Self {
        self.tool_gate = Some(gate);
        self
    }

    fn resolve_grants(&self) -> HashSet<Permission> {
        let mut grants = self.config.grants.clone();
        if let Some(name) = &self.config.agent_name
            && let Some(agent) = self.registry.agents.iter().find(|a| &a.name == name)
        {
            for g in &agent.grants {
                grants.insert(g.clone());
            }
        }
        grants
    }

    /// Whether the agent should run with read-only filesystem tools: either
    /// the agent forces it via frontmatter, or the sandbox permits no writes
    /// so `Write`/`Edit` would only ever fail.
    fn wants_readonly(agent: Option<&Agent>, sandbox: &SandboxConfig) -> bool {
        agent.is_some_and(|a| a.readonly) || sandbox.allow_write.is_empty()
    }

    fn find_agent_definition(&self) -> Option<&Agent> {
        let name = self.config.agent_name.as_deref()?;
        self.registry.agents.iter().find(|a| a.name == name)
    }

    /// The directory this run happens in. Everything cwd-shaped keys off
    /// this — never `std::env::current_dir()` mid-run, so one process can
    /// serve concurrent runs in different directories.
    fn run_cwd(&self) -> Result<std::path::PathBuf> {
        match &self.config.cwd {
            Some(cwd) => Ok(cwd.clone()),
            None => std::env::current_dir()
                .map_err(|e| AppError::Config(format!("cannot determine working directory: {e}"))),
        }
    }

    /// Tool-loop iteration cap for this run. Only an agent's frontmatter
    /// `max_steps` sets one — every mode defaults to unbounded, the cancel
    /// paths being the real limit. The SDK caps a `None` at its own default,
    /// so "unlimited" has to be spelled out.
    fn step_limit(&self) -> usize {
        let Some(steps) = self.find_agent_definition().and_then(|a| a.max_steps) else {
            return usize::MAX;
        };
        steps as usize
    }

    /// Resolve which optional plugins this run gets. `plugins: None` on the
    /// agent (legacy `commands/` agents, default runs) means the full set;
    /// an explicit frontmatter list is the complete tool set — everything
    /// else stays off.
    fn selected_plugins(agent: Option<&Agent>, sandbox: &SandboxConfig) -> Result<PluginSelection> {
        let Some(names) = agent.and_then(|a| a.plugins.as_deref()) else {
            let mut sel = PluginSelection::default();
            if Self::wants_readonly(agent, sandbox) {
                sel.fs = FsMode::Readonly;
            }
            return Ok(sel);
        };

        let mut sel = PluginSelection::none();
        let mut mcp_all = false;
        let mut mcp_servers = Vec::new();
        for name in names {
            match name.trim() {
                "fs" => sel.fs = FsMode::Full,
                "fs-readonly" => sel.fs = FsMode::Readonly,
                "shell" => sel.flags = sel.flags.set(PluginFlags::SHELL, true),
                "websearch" => sel.flags = sel.flags.set(PluginFlags::WEBSEARCH, true),
                "skills" => sel.flags = sel.flags.set(PluginFlags::SKILLS, true),
                "agentsmd" => sel.flags = sel.flags.set(PluginFlags::AGENTSMD, true),
                "mcp" => mcp_all = true,
                other => match other.strip_prefix("mcp:") {
                    Some(server) if !server.is_empty() => mcp_servers.push(server.to_string()),
                    _ => {
                        return Err(AppError::Config(format!(
                            "agent '{}' lists unknown plugin '{other}' (known: fs, fs-readonly, shell, websearch, skills, agentsmd, mcp, mcp:<server>)",
                            agent.map_or("?", |a| a.name.as_str())
                        )));
                    }
                },
            }
        }
        sel.mcp = if mcp_all {
            McpSelection::All
        } else if mcp_servers.is_empty() {
            McpSelection::Off
        } else {
            McpSelection::Only(mcp_servers)
        };
        if sel.fs == FsMode::Full && Self::wants_readonly(agent, sandbox) {
            sel.fs = FsMode::Readonly;
        }
        Ok(sel)
    }

    /// Pair a parsed MCP selection with the configured servers, sorted by
    /// name for deterministic connection order. Fails loudly on `mcp:<server>`
    /// names that have no `[mcp.*]` section and on an explicit selection
    /// with nothing configured; the default [`McpSelection::Available`]
    /// with nothing configured simply means no MCP tools.
    fn select_mcp_servers<'a>(
        configured: &'a HashMap<String, McpServerConfig>,
        selection: &McpSelection,
    ) -> Result<Vec<(String, &'a McpServerConfig)>> {
        let selected: Vec<String> = match selection {
            McpSelection::Off => return Ok(Vec::new()),
            McpSelection::Available | McpSelection::All => configured.keys().cloned().collect(),
            McpSelection::Only(names) => names.clone(),
        };

        let mut unknown: Vec<&str> = selected
            .iter()
            .map(String::as_str)
            .filter(|name| !configured.contains_key(*name))
            .collect();
        unknown.sort_unstable();
        if !unknown.is_empty() {
            let mut known: Vec<&str> = configured.keys().map(String::as_str).collect();
            known.sort_unstable();
            return Err(AppError::Config(format!(
                "mcp server(s) not configured: {} (known: {})",
                unknown.join(", "),
                known.join(", ")
            )));
        }

        let mut servers: Vec<(String, &McpServerConfig)> = selected
            .into_iter()
            .filter_map(|name| configured.get(&name).map(|cfg| (name, cfg)))
            .collect();
        servers.sort_by_key(|(name, _)| name.clone());
        if servers.is_empty() {
            return match selection {
                McpSelection::Available => Ok(servers),
                _ => Err(AppError::Config(
                    "agent requests mcp but no [mcp.*] servers are configured".into(),
                )),
            };
        }
        Ok(servers)
    }

    /// Connect to the selected MCP servers. An explicit request (`plugins:
    /// [mcp]`, `mcp:<name>`) fails the run when a server is down: a
    /// silently missing server would surface later as confusing
    /// tool-not-found errors. The implicit default set
    /// ([`McpSelection::Available`]) is best-effort instead — the run never
    /// asked for a particular server, so one being down costs a warning,
    /// not the run.
    /// Subagent runs (depth > 0) never connect a server marked
    /// `main_agent_only`: the pie toolset spawning agents that could spawn
    /// more through the same door recurses without end. Returns the
    /// filtered server map and the selection to apply to it — `Only`
    /// entries naming a filtered server are dropped with a warning
    /// instead of failing the run.
    fn filter_for_depth(
        configured: &HashMap<String, McpServerConfig>,
        selection: &McpSelection,
        depth: u32,
    ) -> (HashMap<String, McpServerConfig>, McpSelection) {
        if depth == 0 {
            return (configured.clone(), selection.clone());
        }
        let filtered: HashMap<String, McpServerConfig> = configured
            .iter()
            .filter(|(_, server)| !server.main_agent_only)
            .map(|(name, server)| (name.clone(), server.clone()))
            .collect();
        let selection = match selection {
            McpSelection::Only(names) => {
                let (kept, dropped): (Vec<String>, Vec<String>) =
                    names.iter().cloned().partition(|name| {
                        !configured
                            .get(name)
                            .is_some_and(|server| server.main_agent_only)
                    });
                if !dropped.is_empty() {
                    tracing::warn!(
                        servers = ?dropped,
                        "main-agent-only mcp servers are unavailable to nested runs"
                    );
                }
                McpSelection::Only(kept)
            }
            other => other.clone(),
        };
        (filtered, selection)
    }

    async fn build_mcp_plugin(
        pool: &DbPool,
        configured: &HashMap<String, McpServerConfig>,
        selection: &McpSelection,
        depth: u32,
    ) -> Result<McpPlugin> {
        let (configured, selection) = Self::filter_for_depth(configured, selection, depth);
        let servers = Self::select_mcp_servers(&configured, &selection)?;
        let strict = !matches!(selection, McpSelection::Available);

        let mut plugin = McpPlugin::new();
        for (name, server) in servers {
            let headers = server.http_headers();
            let connect: std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> =
                match &server.auth {
                    // OAuth servers authorize from the tokens `pie mcp login`
                    // stored; rmcp refreshes and retries 401s transparently.
                    Some(_) => {
                        match crate::mcp_auth::authorization_manager(&name, server, pool.clone())
                            .await
                        {
                            Ok(manager) => {
                                plugin
                                    .add_remote_server_authorized(
                                        name.clone(),
                                        server.url.as_str(),
                                        headers,
                                        manager,
                                    )
                                    .await
                            }
                            Err(e) => Err(Box::new(e)),
                        }
                    }
                    None => {
                        plugin
                            .add_remote_server(name.clone(), server.url.as_str(), headers)
                            .await
                    }
                };
            match connect {
                Ok(()) => tracing::debug!(server = name, "mcp server connected"),
                Err(e) if strict => {
                    return Err(AppError::Plugin(format!(
                        "mcp server '{name}' failed to connect: {e}"
                    )));
                }
                Err(e) => {
                    let hint = if server.auth.is_some() {
                        format!(" — run `pie mcp login {name}`")
                    } else {
                        String::new()
                    };
                    tracing::warn!(server = name, error = %e, "mcp server unavailable, continuing without it{hint}");
                }
            }
        }
        Ok(plugin)
    }

    fn prepare_system_prompt(&self, cwd: &std::path::Path) -> Result<String> {
        let sp = SystemPrompt::new(&self.registry.skills, &self.registry.agents)
            .with_agent(self.config.agent_name.as_deref())
            .with_cwd(cwd.to_path_buf());

        Ok(sp.render()?)
    }

    fn build_sdk_agent(&self, cwd: &std::path::Path) -> Result<agentsdk::AgentBuilder> {
        let mut bin_dirs = vec![crate::config::pie_home().join("bin")];
        if let Some(git_root) = crate::utils::git_repo_root_from(cwd) {
            bin_dirs.push(std::path::PathBuf::from(git_root).join(".pie").join("bin"));
        }

        let sandbox =
            p1e_sandbox::PlatformSandbox::new((*self.sandbox).clone(), cwd).with_bin_dirs(bin_dirs);

        Ok(SdkAgent::builder()
            .client(self.model.clone())
            .component(Sandbox::new(sandbox))
            .component(agentsdk::core::Cwd(cwd.to_path_buf()))
            .options(
                agentsdk::AgentOptions::builder()
                    .max_iterations(self.step_limit())
                    .build()
                    .map_err(|e| AppError::Config(e.to_string()))?,
            ))
    }

    pub fn run<'a>(&'a mut self, query_str: &'a str) -> BoxFuture<'a, Result<RunOutcome>> {
        Box::pin(async move {
            let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
            let label = self.config.agent_name.clone().unwrap_or_else(|| {
                let mut q = query_str.trim().to_string();
                if q.chars().count() > 80 {
                    q = q.chars().take(80).collect();
                    q.push('…');
                }
                q
            });
            super::progress::spawn(label, event_rx);

            let query = if let Some(ref name) = self.config.agent_name
                && !query_str.contains(name)
            {
                format!("{name} {query_str}")
            } else {
                query_str.to_string()
            };

            self.stream(&query, event_tx).await
        })
    }

    pub fn run_json<'a>(
        &'a mut self,
        query_str: &'a str,
        schema: serde_json::Value,
    ) -> BoxFuture<'a, Result<RunOutcome>> {
        Box::pin(async move {
            let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();

            let query = if let Some(ref name) = self.config.agent_name
                && !query_str.contains(name)
            {
                format!("{name} {query_str}")
            } else {
                query_str.to_string()
            };

            let outcome = self.stream(&query, event_tx).await?;

            let mut history = self.session.to_messages();

            // Explicitly prompt the LLM to format its response as JSON based on the tools output.
            history.push(agentsdk::core::messages::user(
                "Based on the execution and gathered information, please output the final result strictly as JSON matching the requested schema."
            ));

            // The structured-output call is a single request; it never loops.
            let options = agentsdk::AgentOptions::default();

            let result = self
                .model
                .get_json(&options, &history, &schema)
                .await
                .map_err(|e| AppError::Api(Box::new(e)))?;

            let text = serde_json::to_string_pretty(&result)?;
            // TODO: the final structured-output call above is a
            // non-streaming request, so its usage never reaches the
            // streamed Usage component — run_json undercounts by that call.
            Ok(RunOutcome {
                text,
                usage: outcome.usage,
                cost_usd: outcome.cost_usd,
            })
        })
    }

    /// The skill search paths for a run: pie-home skills, the repo's
    /// `.pie/skills`, plus every path the agent definition adds (relative
    /// paths belong to the run's directory).
    fn skill_paths(&self, cwd: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut paths = vec![crate::config::pie_home().join("skills")];
        if let Some(root) = crate::utils::git_repo_root_from(cwd) {
            paths.push(std::path::PathBuf::from(root).join(".pie").join("skills"));
        }
        if let Some(agent) = self.find_agent_definition() {
            for p in &agent.skills_paths {
                let expanded = p
                    .strip_prefix("~/")
                    .and_then(|rest| dirs::home_dir().map(|h| h.join(rest)))
                    .unwrap_or_else(|| std::path::PathBuf::from(p));
                // Relative skill paths belong to the run's directory.
                let expanded = if expanded.is_absolute() {
                    expanded
                } else {
                    cwd.join(expanded)
                };
                paths.push(expanded);
            }
        }
        paths
    }

    /// Register the always-on and selection-gated plugins on the SDK
    /// builder. Streaming/TUI-facing plugins (stream, history) and the
    /// fs/shell persistence pair are applied by the caller.
    async fn register_plugins(
        &self,
        mut builder: agentsdk::AgentBuilder,
        selection: &PluginSelection,
        cwd: &std::path::Path,
    ) -> Result<agentsdk::AgentBuilder> {
        let mode = self.config.mode.unwrap_or_default();
        builder = builder
            .plugin(JewelsPlugin::new())
            .plugin({
                let modes = ModePlugin::new(mode);
                if self.config.mode_switching {
                    modes
                } else {
                    modes.without_switching()
                }
            })
            .plugin(crate::plugin::EmbeddedSystemPromptPlugin::new(
                include_str!("../../../../.pie/SYSTEM.md"),
            ))
            .plugin(crate::plugin::PermissionsPlugin::new(
                self.registry.clone(),
                self.resolve_grants(),
                self.permission_tx.clone(),
            ));

        if let Some((gate_tx, grants)) = &self.tool_gate {
            builder = builder.plugin(ToolGatePlugin::new(gate_tx.clone(), Arc::clone(grants)));
        }

        if selection[PluginFlags::AGENTSMD] {
            builder = builder.plugin(crate::plugin::build_agentsmd_plugin(cwd)?);
        }
        if selection[PluginFlags::SKILLS] {
            builder = builder.plugin(
                SkillsPlugin::builder()
                    .search_paths(self.skill_paths(cwd))
                    .build()
                    .map_err(|e| AppError::Plugin(format!("failed to build skills plugin: {e}")))?,
            );
        }

        if selection[PluginFlags::SHELL] {
            builder = builder.plugin(ShellPlugin::new());
        }
        if selection[PluginFlags::WEBSEARCH] {
            builder = builder.plugin(WebsearchPlugin::new());
        }
        if selection.mcp != McpSelection::Off {
            let config = CONFIG
                .get()
                .ok_or_else(|| AppError::Config("global config not initialized".into()))?;
            builder = builder.plugin(
                Self::build_mcp_plugin(
                    &self.session.pool,
                    &config.mcp,
                    &selection.mcp,
                    self.config.depth,
                )
                .await?,
            );
        }

        builder = builder
            .plugin(HelperBinariesPlugin::new(cwd))
            .plugin(UserCommandPlugin::new(
                self.registry.clone(),
                self.config.agent_name.clone(),
            ))
            .plugin(crate::plugin::DoomLoopPlugin::new());

        if AgentConfig::is_debug() {
            builder = builder.plugin(crate::plugin::DebugPlugin::new(
                &self.session.id.to_string(),
                "",
            ));
        }

        Ok(builder)
    }

    pub fn stream<'a>(
        &'a mut self,
        query_str: &'a str,
        event_tx: UnboundedSender<AgentEvent>,
    ) -> BoxFuture<'a, Result<RunOutcome>> {
        Box::pin(async move {
            let t_build = std::time::Instant::now();
            let cwd = self.run_cwd()?;
            let mut builder = self.build_sdk_agent(&cwd)?;

            let history_plugin = MemoryHistoryPlugin::new();
            for msg in self.session.to_messages() {
                history_plugin.push(msg).await;
            }

            let selection = Self::selected_plugins(self.find_agent_definition(), &self.sandbox)?;

            if !self.config.mode_switching {
                // The mode is fixed for this run, so what it forbids can never
                // run — hide those tools rather than let the model spend a
                // round trip discovering they are refused. (With switching on,
                // the model may switch and then legitimately use them, so the
                // list has to stay complete and `ModePlugin` refuses per call.)
                let mode = self.config.mode.unwrap_or_default();
                builder = builder.tool_filter(move |name| !mode.is_tool_blocked(name));
            }

            // StreamPlugin must be FIRST: on_tool_post_execute is
            // first-decisive-wins, and JewelsPlugin returns Proceed(Some(..))
            // whenever redaction changes a result — anything registered after
            // it (the output clamp, debug result logging) would be preempted.
            let stream_plugin = crate::agent::StreamPlugin::new(
                event_tx.clone(),
                self.config.retry.clone(),
                self.model.config.model.clone(),
            );
            builder = builder.plugin(stream_plugin).plugin(history_plugin.clone());
            builder = self.register_plugins(builder, &selection, &cwd).await?;
            builder = match selection.fs {
                FsMode::Full => builder.plugin(FileSystemPlugin::new()),
                FsMode::Readonly => builder.plugin(ReadOnlyFileSystemPlugin::new()),
                FsMode::Off => builder,
            }
            .plugin(PersistencePlugin::new(self.session.clone()));

            let mut agent = builder
                .build()
                .map_err(|e| AppError::Config(e.to_string()))?;
            tracing::debug!(
                ms = crate::utils::ms_of(t_build.elapsed()),
                "timing: agent built"
            );

            // Dispatch user message to plugins for transformation/redaction (Fast)
            let query = agent.dispatch_user_message(query_str).await;

            // Notify UI immediately after redaction (only for top-level agent)
            if self.config.depth == 0 {
                let _ = event_tx.send(AgentEvent::UserMessage(query.clone()));
            }

            let t_prompt = std::time::Instant::now();
            let system = self.prepare_system_prompt(&cwd)?;
            tracing::debug!(
                ms = crate::utils::ms_of(t_prompt.elapsed()),
                "timing: system prompt prepared"
            );

            // Inject the system prompt into the agent's context
            if let Some(entity) = agent.entity
                && let Some(mut world) = agent.world.take()
            {
                let _ =
                    world.insert_one(entity, crate::plugin::SystemPromptComponent(system.clone()));
                agent.world = Some(world);
            }

            // Persistence
            history_plugin
                .push(agentsdk::core::messages::user(&query))
                .await;
            self.session.add_user(&query).await?;

            let t_run = std::time::Instant::now();
            let output = agent.run().await?;
            tracing::debug!(
                ms = crate::utils::ms_of(t_run.elapsed()),
                "timing: agent run done"
            );

            let final_messages = history_plugin.messages().await;

            let final_text = final_messages
                .iter()
                .rev()
                .find_map(|msg| match msg {
                    Message::AssistantMessage(a) => a.content.clone(),
                    _ => None,
                })
                .unwrap_or_default();

            let (usage, cost_usd) = self.finalize_usage(&output).await;

            let _ = event_tx.send(AgentEvent::Usage { usage, cost_usd });
            let _ = event_tx.send(AgentEvent::Done(final_text.clone()));

            Ok(RunOutcome {
                text: final_text,
                usage,
                cost_usd,
            })
        })
    }

    /// Read the run's cumulative usage from the agent world and persist it
    /// for bookkeeping. One record per interaction, in one place:
    /// single-shot, TUI and cron runs all funnel through `stream()`.
    /// Providers that don't report usage leave nothing to record.
    async fn finalize_usage(
        &self,
        output: &agentsdk::core::agent::AgentRunOutput,
    ) -> (RunUsage, Option<f64>) {
        let usage = output
            .world
            .get::<&agentsdk::Usage>(output.entity)
            .as_ref()
            .ok()
            .map(|u| RunUsage::from(**u))
            .unwrap_or_default();
        let model = self.model.config.model.clone();
        let cost_usd = crate::usage::pricing_for(&model).map(|p| usage.cost_usd(&p));

        if usage.requests > 0
            && let Err(e) = self
                .session
                .record_usage(&usage, &model, self.config.agent_name.as_deref(), cost_usd)
                .await
        {
            tracing::warn!("failed to record llm usage: {e}");
        }
        (usage, cost_usd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::OutputMode;

    fn test_agent(readonly: bool) -> Agent {
        Agent {
            name: "t".into(),
            description: String::new(),
            output_mode: OutputMode::default(),
            model: None,
            temperature: None,
            content: String::new(),
            needs: Vec::new(),
            tools: Vec::new(),
            sandbox: None,
            grants: Vec::new(),
            readonly,
            plugins: None,
            skills_paths: Vec::new(),
            max_steps: None,
        }
    }

    fn sandbox(write_paths: &[&str]) -> SandboxConfig {
        SandboxConfig {
            allow_write: write_paths.iter().map(|p| (*p).into()).collect(),
            ..SandboxConfig::default()
        }
    }

    #[test]
    fn readonly_forced_by_agent_frontmatter() {
        assert!(PieAgent::wants_readonly(
            Some(&test_agent(true)),
            &sandbox(&["."])
        ));
    }

    #[test]
    fn readonly_when_sandbox_permits_no_writes() {
        assert!(PieAgent::wants_readonly(None, &sandbox(&[])));
        assert!(PieAgent::wants_readonly(
            Some(&test_agent(false)),
            &sandbox(&[])
        ));
    }

    #[test]
    fn full_fs_when_agent_writable_and_sandbox_allows_write() {
        assert!(!PieAgent::wants_readonly(
            Some(&test_agent(false)),
            &sandbox(&["."])
        ));
        assert!(!PieAgent::wants_readonly(None, &sandbox(&["."])));
    }

    fn tooled_agent(plugins: Option<Vec<String>>, readonly: bool) -> Agent {
        Agent {
            readonly,
            plugins,
            ..test_agent(false)
        }
    }

    #[test]
    fn no_plugins_key_means_full_default_set() {
        let sel =
            PieAgent::selected_plugins(Some(&tooled_agent(None, false)), &sandbox(&["."])).unwrap();
        assert_eq!(sel, PluginSelection::default());

        // empty allow_write demotes fs even in the default set
        let sel =
            PieAgent::selected_plugins(Some(&tooled_agent(None, false)), &sandbox(&[])).unwrap();
        assert_eq!(sel.fs, FsMode::Readonly);
    }

    #[test]
    fn explicit_plugins_are_the_whole_set() {
        let sel = PieAgent::selected_plugins(
            Some(&tooled_agent(
                Some(vec!["fs-readonly".into(), "shell".into()]),
                false,
            )),
            &sandbox(&["."]),
        )
        .unwrap();
        assert_eq!(sel.fs, FsMode::Readonly);
        assert!(sel[PluginFlags::SHELL]);
        assert!(!sel[PluginFlags::WEBSEARCH]);
        assert!(!sel[PluginFlags::SKILLS]);
        assert!(!sel[PluginFlags::AGENTSMD]);

        // empty list = no tools at all
        let sel =
            PieAgent::selected_plugins(Some(&tooled_agent(Some(vec![]), false)), &sandbox(&["."]))
                .unwrap();
        assert_eq!(sel, PluginSelection::none());
    }

    #[test]
    fn readonly_demotes_explicit_fs_to_readonly() {
        let sel = PieAgent::selected_plugins(
            Some(&tooled_agent(Some(vec!["fs".into()]), true)),
            &sandbox(&["."]),
        )
        .unwrap();
        assert_eq!(sel.fs, FsMode::Readonly);
    }

    #[test]
    fn unknown_plugin_name_fails() {
        let err = PieAgent::selected_plugins(
            Some(&tooled_agent(
                Some(vec!["fs".into(), "webserch".into()]),
                false,
            )),
            &sandbox(&["."]),
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown plugin 'webserch'"), "{msg}");
    }

    #[test]
    fn mcp_plugin_names_parse_to_selection() {
        // bare `mcp` selects every configured server
        let sel = PieAgent::selected_plugins(
            Some(&tooled_agent(Some(vec!["mcp".into()]), false)),
            &sandbox(&["."]),
        )
        .unwrap();
        assert_eq!(sel.mcp, McpSelection::All);

        // `mcp:<server>` selects just that server, alongside other plugins
        let sel = PieAgent::selected_plugins(
            Some(&tooled_agent(
                Some(vec!["mcp:deepwiki".into(), "fs".into()]),
                false,
            )),
            &sandbox(&["."]),
        )
        .unwrap();
        assert_eq!(sel.mcp, McpSelection::Only(vec!["deepwiki".into()]));
        assert_eq!(sel.fs, FsMode::Full);
    }

    #[test]
    fn default_set_connects_to_available_mcp() {
        // Default runs and legacy agents get every configured server,
        // best-effort; an agent with an explicit (possibly empty) plugin
        // list gets exactly what it names.
        assert_eq!(PluginSelection::default().mcp, McpSelection::Available);
        assert_eq!(PluginSelection::none().mcp, McpSelection::Off);

        let sel =
            PieAgent::selected_plugins(Some(&tooled_agent(None, false)), &sandbox(&["."])).unwrap();
        assert_eq!(sel.mcp, McpSelection::Available);

        let sel =
            PieAgent::selected_plugins(Some(&tooled_agent(Some(vec![]), false)), &sandbox(&["."]))
                .unwrap();
        assert_eq!(sel.mcp, McpSelection::Off);
    }

    #[test]
    fn mcp_with_empty_server_name_is_unknown_plugin() {
        let err = PieAgent::selected_plugins(
            Some(&tooled_agent(Some(vec!["mcp:".into()]), false)),
            &sandbox(&["."]),
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown plugin 'mcp:'"), "{msg}");
    }

    fn mcp_config() -> HashMap<String, McpServerConfig> {
        let server = |host: &str| McpServerConfig {
            url: format!("https://{host}/mcp").parse().unwrap(),
            api_key: None,
            headers: HashMap::new(),
            auth: None,
            main_agent_only: false,
        };
        HashMap::from([
            ("b".to_string(), server("b")),
            ("a".to_string(), server("a")),
        ])
    }

    #[test]
    fn depth_zero_keeps_main_agent_only_servers() {
        let configured = main_agent_only_config();
        let (filtered, _) = PieAgent::filter_for_depth(&configured, &McpSelection::All, 0);
        assert_eq!(filtered.len(), 2, "top-level runs see everything");
    }

    #[test]
    fn nested_runs_drop_main_agent_only_servers() {
        let configured = main_agent_only_config();
        let (filtered, _) = PieAgent::filter_for_depth(&configured, &McpSelection::All, 1);
        assert_eq!(filtered.keys().cloned().collect::<Vec<_>>(), vec!["b"]);

        let (filtered, _) = PieAgent::filter_for_depth(&configured, &McpSelection::Available, 1);
        assert_eq!(filtered.keys().cloned().collect::<Vec<_>>(), vec!["b"]);
    }

    #[test]
    fn nested_runs_only_selection_naming_filtered_server_is_warned_and_dropped() {
        let configured = main_agent_only_config();
        let (filtered, selection) = PieAgent::filter_for_depth(
            &configured,
            &McpSelection::Only(vec!["self".into(), "b".into()]),
            1,
        );
        assert_eq!(filtered.keys().cloned().collect::<Vec<_>>(), vec!["b"]);
        assert_eq!(selection, McpSelection::Only(vec!["b".into()]));

        // Top-level explicit selection is untouched.
        let (_, selection) =
            PieAgent::filter_for_depth(&configured, &McpSelection::Only(vec!["self".into()]), 0);
        assert_eq!(selection, McpSelection::Only(vec!["self".into()]));
    }

    fn main_agent_only_config() -> HashMap<String, McpServerConfig> {
        let server = |host: &str, main_agent_only: bool| McpServerConfig {
            url: format!("https://{host}/mcp").parse().unwrap(),
            api_key: None,
            headers: HashMap::new(),
            auth: None,
            main_agent_only,
        };
        HashMap::from([
            ("self".to_string(), server("self", true)),
            ("b".to_string(), server("b", false)),
        ])
    }

    #[test]
    fn select_mcp_servers_all_sorted_by_name() {
        let configured = mcp_config();
        let servers = PieAgent::select_mcp_servers(&configured, &McpSelection::All).unwrap();
        let names: Vec<&str> = servers.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn select_mcp_servers_only_filters() {
        let configured = mcp_config();
        let servers =
            PieAgent::select_mcp_servers(&configured, &McpSelection::Only(vec!["b".into()]))
                .unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].0, "b");
    }

    #[test]
    fn select_mcp_servers_unknown_name_fails_with_known_list() {
        let configured = mcp_config();
        let err =
            PieAgent::select_mcp_servers(&configured, &McpSelection::Only(vec!["deepwiki".into()]))
                .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not configured: deepwiki"), "{msg}");
        assert!(msg.contains("known: a, b"), "{msg}");
    }

    #[test]
    fn select_mcp_servers_without_config_fails() {
        let configured = HashMap::new();
        let err = PieAgent::select_mcp_servers(&configured, &McpSelection::All).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no [mcp.*] servers are configured"), "{msg}");
    }

    #[test]
    fn select_mcp_servers_available_without_config_is_no_tools() {
        let configured = HashMap::new();
        let servers = PieAgent::select_mcp_servers(&configured, &McpSelection::Available).unwrap();
        assert!(servers.is_empty());
    }

    /// A loopback port nothing listens on: the connect fails fast, no
    /// network leaves the machine.
    fn dead_mcp_config() -> HashMap<String, McpServerConfig> {
        let server = McpServerConfig {
            url: "http://127.0.0.1:1/mcp".parse().unwrap(),
            api_key: None,
            headers: HashMap::new(),
            auth: None,
            main_agent_only: false,
        };
        HashMap::from([("dead".to_string(), server)])
    }

    fn block_on<T>(fut: impl Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fut)
    }

    #[test]
    fn explicit_mcp_connection_failure_fails_the_run() {
        let configured = dead_mcp_config();
        let pool = block_on(crate::db::create_test_pool()).unwrap();
        let result = block_on(PieAgent::build_mcp_plugin(
            &pool,
            &configured,
            &McpSelection::All,
            0,
        ));
        let Err(err) = result else {
            panic!("a dead server must fail an explicit request");
        };
        assert!(
            err.to_string()
                .contains("mcp server 'dead' failed to connect"),
            "{err}"
        );
    }

    #[test]
    fn default_mcp_connection_failure_is_best_effort() {
        let configured = dead_mcp_config();
        // The run asked for no server in particular: a dead one is skipped
        // (with a warning in the session log) instead of failing the run.
        let pool = block_on(crate::db::create_test_pool()).unwrap();
        let plugin = block_on(PieAgent::build_mcp_plugin(
            &pool,
            &configured,
            &McpSelection::Available,
            0,
        ))
        .unwrap();
        assert!(
            agentsdk::AgentPlugin::tools(&plugin).is_empty(),
            "skipped server must advertise no tools"
        );
    }
}
