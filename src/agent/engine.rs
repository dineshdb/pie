use crate::agent::AgentEvent;
use crate::config::CONFIG;
use crate::error::{AppError, Result};
use crate::plugin::{
    HelperBinariesPlugin, ModePlugin, PermissionRequest, PersistencePlugin, UserCommandPlugin,
    WebsearchPlugin,
};
use crate::prompt::SystemPrompt;
use crate::registry::Registry;
use crate::session::Session;
use agentsdk::core::Sandbox;
use agentsdk::{Agent as SdkAgent, MemoryHistoryPlugin, Message};
use agentsdk_plugin_fs::{FileSystemPlugin, ReadOnlyFileSystemPlugin};
use agentsdk_plugin_jewels::JewelsPlugin;
use agentsdk_plugin_shell::ShellPlugin;
use agentsdk_plugin_skills::SkillsPlugin;
use futures::future::BoxFuture;
use p1e_sandbox::{Permission, SandboxConfig};
use serde::Deserialize;
use std::collections::HashSet;
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

/// The optional, selectable plugins. Defaults = the full set (markdown
/// agents and agent-less runs); [`PluginSelection::none`] = YAML agents
/// that opt in explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PluginSelection {
    fs: FsMode,
    shell: bool,
    websearch: bool,
    skills: bool,
    agentsmd: bool,
}

impl Default for PluginSelection {
    fn default() -> Self {
        Self {
            fs: FsMode::Full,
            shell: true,
            websearch: true,
            skills: true,
            agentsmd: true,
        }
    }
}

impl PluginSelection {
    fn none() -> Self {
        Self {
            fs: FsMode::Off,
            shell: false,
            websearch: false,
            skills: false,
            agentsmd: false,
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
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentConfig {
    pub agent_name: Option<String>,
    #[allow(dead_code)]
    pub history_limit: u32,
    pub max_steps: u32,
    pub depth: u32,
    #[allow(dead_code)]
    pub max_retries: u32,
    pub retry: crate::config::RetryConfig,
    #[serde(default)]
    pub grants: HashSet<Permission>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            agent_name: None,
            history_limit: 10,
            max_steps: 200,
            depth: 0,
            max_retries: 3,
            retry: crate::config::RetryConfig::default(),
            grants: HashSet::new(),
        }
    }
}

impl AgentConfig {
    pub fn is_debug() -> bool {
        CONFIG.get().is_some_and(|c| c.debug)
    }
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
        }
    }

    pub fn with_permission_channel(mut self, tx: UnboundedSender<PermissionRequest>) -> Self {
        self.permission_tx = Some(tx);
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
        for name in names {
            match name.trim() {
                "fs" => sel.fs = FsMode::Full,
                "fs-readonly" => sel.fs = FsMode::Readonly,
                "shell" => sel.shell = true,
                "websearch" => sel.websearch = true,
                "skills" => sel.skills = true,
                "agentsmd" => sel.agentsmd = true,
                other => {
                    return Err(AppError::Config(format!(
                        "agent '{}' lists unknown plugin '{other}' (known: fs, fs-readonly, shell, websearch, skills, agentsmd)",
                        agent.map_or("?", |a| a.name.as_str())
                    )));
                }
            }
        }
        if sel.fs == FsMode::Full && Self::wants_readonly(agent, sandbox) {
            sel.fs = FsMode::Readonly;
        }
        Ok(sel)
    }

    fn prepare_system_prompt(&self) -> Result<String> {
        let sp = SystemPrompt::new(&self.registry.skills, &self.registry.agents)
            .with_agent(self.config.agent_name.as_deref());

        Ok(sp.render()?)
    }

    fn build_sdk_agent(&self) -> Result<agentsdk::AgentBuilder> {
        let mut bin_dirs = vec![crate::config::pie_home().join("bin")];
        if let Some(git_root) = crate::utils::git_repo_root() {
            bin_dirs.push(std::path::PathBuf::from(git_root).join(".pie").join("bin"));
        }

        let sandbox =
            p1e_sandbox::PlatformSandbox::new((*self.sandbox).clone()).with_bin_dirs(bin_dirs);

        Ok(SdkAgent::builder()
            .client(self.model.clone())
            .component(Sandbox::new(sandbox))
            .options(
                agentsdk::AgentOptions::builder()
                    .max_iterations(self.config.max_steps as usize)
                    .build()
                    .map_err(|e| AppError::Config(e.to_string()))?,
            ))
    }

    pub fn run<'a>(&'a mut self, query_str: &'a str) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();

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
    ) -> BoxFuture<'a, Result<serde_json::Value>> {
        Box::pin(async move {
            let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();

            let query = if let Some(ref name) = self.config.agent_name
                && !query_str.contains(name)
            {
                format!("{name} {query_str}")
            } else {
                query_str.to_string()
            };

            let _ = self.stream(&query, event_tx).await?;

            let mut history = self.session.to_messages();

            // Explicitly prompt the LLM to format its response as JSON based on the tools output.
            history.push(agentsdk::core::messages::user(
                "Based on the execution and gathered information, please output the final result strictly as JSON matching the requested schema."
            ));

            let options = agentsdk::AgentOptions::builder()
                .max_iterations(self.config.max_steps as usize)
                .build()
                .map_err(|e| AppError::Config(e.to_string()))?;

            let result = self
                .model
                .get_json(&options, &history, &schema)
                .await
                .map_err(|e| AppError::Api(Box::new(e)))?;

            Ok(result)
        })
    }

    pub fn stream<'a>(
        &'a mut self,
        query_str: &'a str,
        event_tx: UnboundedSender<AgentEvent>,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let t_build = std::time::Instant::now();
            let mut builder = self.build_sdk_agent()?;

            let history_plugin = MemoryHistoryPlugin::new();
            for msg in self.session.to_messages() {
                history_plugin.push(msg).await;
            }

            let mut paths = vec![crate::config::pie_home().join("skills")];
            if let Some(root) = crate::utils::git_repo_root() {
                paths.push(std::path::PathBuf::from(root).join(".pie").join("skills"));
            }
            if let Some(agent) = self.find_agent_definition() {
                for p in &agent.skills_paths {
                    let expanded = p
                        .strip_prefix("~/")
                        .and_then(|rest| dirs::home_dir().map(|h| h.join(rest)))
                        .unwrap_or_else(|| std::path::PathBuf::from(p));
                    paths.push(expanded);
                }
            }

            let selection = Self::selected_plugins(self.find_agent_definition(), &self.sandbox)?;
            let grants = self.resolve_grants();
            builder = builder
                .plugin(history_plugin.clone())
                .plugin(JewelsPlugin::new())
                .plugin(ModePlugin::default())
                .plugin(crate::plugin::EmbeddedSystemPromptPlugin::new(
                    include_str!("../../.pie/SYSTEM.md"),
                ))
                .plugin(crate::plugin::PermissionsPlugin::new(
                    self.registry.clone(),
                    grants,
                    self.permission_tx.clone(),
                ));

            if selection.agentsmd {
                builder = builder.plugin(crate::plugin::build_agentsmd_plugin()?);
            }
            if selection.skills {
                builder = builder.plugin(
                    SkillsPlugin::builder()
                        .search_paths(paths)
                        .build()
                        .map_err(|e| {
                            AppError::Plugin(format!("failed to build skills plugin: {e}"))
                        })?,
                );
            }

            builder = match selection.fs {
                FsMode::Full => builder.plugin(FileSystemPlugin::new()),
                FsMode::Readonly => builder.plugin(ReadOnlyFileSystemPlugin::new()),
                FsMode::Off => builder,
            }
            .plugin(PersistencePlugin::new(self.session.clone()));

            if selection.shell {
                builder = builder.plugin(ShellPlugin::new());
            }
            if selection.websearch {
                builder = builder.plugin(WebsearchPlugin::new());
            }

            builder = builder
                .plugin(HelperBinariesPlugin::new())
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

            let stream_plugin =
                crate::agent::StreamPlugin::new(event_tx.clone(), self.config.retry.clone());
            builder = builder.plugin(stream_plugin);

            let mut agent = builder
                .build()
                .map_err(|e| AppError::Config(e.to_string()))?;
            tracing::info!(
                ms = t_build.elapsed().as_millis() as u64,
                "timing: agent built"
            );

            // Dispatch user message to plugins for transformation/redaction (Fast)
            let query = agent.dispatch_user_message(query_str).await;

            // Notify UI immediately after redaction (only for top-level agent)
            if self.config.depth == 0 {
                let _ = event_tx.send(AgentEvent::UserMessage(query.clone()));
            }

            let t_prompt = std::time::Instant::now();
            let system = self.prepare_system_prompt()?;
            tracing::info!(
                ms = t_prompt.elapsed().as_millis() as u64,
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
            let _output = agent.run().await?;
            tracing::info!(
                ms = t_run.elapsed().as_millis() as u64,
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

            let _ = event_tx.send(AgentEvent::Done(final_text.clone()));

            Ok(final_text)
        })
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
            plugins,
            readonly,
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
        assert!(sel.shell);
        assert!(!sel.websearch);
        assert!(!sel.skills);
        assert!(!sel.agentsmd);

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
}
