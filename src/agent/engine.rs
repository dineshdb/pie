use crate::agent::user_plugins::run_prompt_hook;
use crate::agent::{AgentEvent, OutputMode, UserPluginRunner, find_subsume_candidate};
use crate::config::CONFIG;
use crate::db::DbPool;
use crate::error::{AppError, Result};
use crate::hook::HookEvent;
use crate::instructions::Instructions;
use crate::prompt::SystemPrompt;
use crate::registry::Registry;
use crate::session::{HistoryEntry, Session};
use crate::tools::plan::plan_tools;
use crate::tools::{shell, subagent_tool, websearch};
use agentsdk::core::tools::Tool;
use agentsdk::openai::api::ChatCompletionRequestUserMessageContent;
use agentsdk::{Agent as SdkAgent, MemoryHistoryPlugin, Message};
use futures::future::BoxFuture;
use p1e_sandbox::SandboxConfig;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Clone)]
pub struct PieAgent {
    pub model: agentsdk::OpenAI,
    pub registry: Arc<Registry>,
    pub sandbox: Arc<SandboxConfig>,
    pub pool: Arc<DbPool>,
    pub session: Session,
    pub config: AgentConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct AgentConfig {
    pub agent_name: Option<String>,
    pub history_limit: u32,
    pub max_steps: u32,
    pub depth: u32,
    pub max_retries: u32,
    pub retry: crate::config::RetryConfig,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            agent_name: None,
            history_limit: 10,
            max_steps: 20,
            depth: 0,
            max_retries: 3,
            retry: crate::config::RetryConfig::default(),
        }
    }
}

impl AgentConfig {
    pub fn is_debug() -> bool {
        CONFIG.get().is_some_and(|c| c.debug)
    }

    pub fn subagent(depth: u32, agent_name: Option<String>) -> Self {
        Self {
            agent_name,
            history_limit: 10,
            max_steps: 10,
            depth,
            ..Self::default()
        }
    }
}

impl PieAgent {
    pub fn new(
        model: agentsdk::OpenAI,
        registry: Arc<Registry>,
        sandbox: Arc<SandboxConfig>,
        pool: Arc<DbPool>,
        session: Session,
        config: AgentConfig,
    ) -> Self {
        Self {
            model,
            registry,
            sandbox,
            pool,
            session,
            config,
        }
    }

    fn merged_mentions(&self, query: &Instructions) -> Instructions {
        let mut merged = query.clone();
        if self.config.history_limit > 0 {
            self.session
                .history_entries()
                .iter()
                .rev()
                .take(self.config.history_limit as usize)
                .filter_map(|e| match e {
                    HistoryEntry::User(c) => Some(c),
                    _ => None,
                })
                .for_each(|c| merged.merge_mentions(c));
        }
        merged
    }

    async fn prepare_system_prompt(&self, query: &Instructions) -> Result<String> {
        let query_mentions = self.merged_mentions(query);

        let sp = SystemPrompt::new(
            &self.registry.skills,
            &self.registry.agents,
            &self.registry.plugins,
        )
        .with_plan(self.pool.clone(), self.session.id.to_string())
        .with_agent(self.config.agent_name.as_deref())
        .resolve(&query_mentions);

        Ok(sp.render().await?)
    }

    fn build_tools(&self, _output_mode: OutputMode) -> Result<Vec<Tool>> {
        let sandbox = self.sandbox.clone();
        let registry = self.registry.clone();
        let pool = self.pool.clone();
        let model = self.model.clone();
        let session_id = self.session.id.to_string();

        let mut tools = vec![
            shell(sandbox.clone())?,
            subagent_tool(model, registry, sandbox.clone(), pool.clone())?,
            websearch(sandbox)?,
        ];

        for tool in plan_tools(self.pool.clone(), &session_id)? {
            tools.push(tool);
        }

        Ok(tools)
    }

    fn build_sdk_agent(&self, output_mode: OutputMode) -> Result<agentsdk::AgentBuilder> {
        let tools = self.build_tools(output_mode)?;
        let mut defs = Vec::with_capacity(tools.len());
        let mut execs = HashMap::with_capacity(tools.len());
        for t in tools {
            defs.push(t.definition.clone());
            execs.insert(t.definition.name.clone(), t.execute);
        }

        Ok(SdkAgent::builder().client(self.model.clone()).options(
            agentsdk::AgentOptions::builder()
                .max_iterations(self.config.max_steps as usize)
                .tool_definitions(Arc::new(defs))
                .tool_executors(Arc::new(execs))
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

            self.stream(&query, OutputMode::Md, event_tx).await
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

            let _ = self.stream(&query, OutputMode::Json, event_tx).await?;

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

    async fn prepare_query_and_system(
        &self,
        query_str: &str,
        output_mode: OutputMode,
    ) -> Result<(String, String)> {
        let mut current_query_raw = jewels::redact(query_str);

        let Some(cfg) = CONFIG.get() else {
            let query = current_query_raw.clone();
            let system = self
                .prepare_system_prompt(&Instructions::new(query.clone()))
                .await?;
            return Ok((query, system));
        };
        let sid = &self.session.id.to_string();

        // PostUserQuery: transform the raw query
        let (_, q) = run_prompt_hook(
            &cfg.plugins,
            HookEvent::PostUserQuery,
            None,
            Some(&current_query_raw),
            sid,
            output_mode,
        )
        .await;
        if let Some(transformed) = q {
            current_query_raw = transformed;
        }

        let mut query = current_query_raw.clone();
        let mut system = self
            .prepare_system_prompt(&Instructions::new(query.clone()))
            .await?;

        // PrePrompt: transform system + query
        let (s, q) = run_prompt_hook(
            &cfg.plugins,
            HookEvent::PrePrompt,
            Some(&system),
            Some(&query),
            sid,
            output_mode,
        )
        .await;
        if let Some(s) = s {
            system = s;
        }
        if let Some(q) = q {
            query = q;
        }

        system = jewels::redact(&system);

        Ok((query, system))
    }

    fn redact_message(msg: &mut Message) {
        match msg {
            Message::UserMessage(u) => {
                if let Some(ChatCompletionRequestUserMessageContent::String(s)) = &mut u.content {
                    *s = jewels::redact(s);
                }
            }
            Message::AssistantMessage(a) => {
                if let Some(tool_calls) = &mut a.tool_calls {
                    for tc in tool_calls {
                        tc.function.arguments = jewels::redact(&tc.function.arguments);
                    }
                }
            }
            Message::ToolMessage(_) | Message::FunctionMessage(_) => {}
            Message::SystemMessage(s) => {
                if let Some(content) = &mut s.content {
                    *content = jewels::redact(content);
                }
            }
        }
    }

    pub fn stream<'a>(
        &'a mut self,
        query_str: &'a str,
        output_mode: OutputMode,
        event_tx: UnboundedSender<AgentEvent>,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let query = Instructions::new(query_str);

            if self.config.depth < 2
                && let Some(agent_name) = find_subsume_candidate(&query, &self.registry.agents)
                && self.config.agent_name.as_ref() != Some(&agent_name)
            {
                let mut subagent = self.spawn_subagent(Some(agent_name)).await?;
                return subagent.stream(query_str, output_mode, event_tx).await;
            }

            let (query, _system) = self
                .prepare_query_and_system(query_str, output_mode)
                .await?;

            // The system prompt is re-derived by EmbeddedSystemPromptPlugin inside the agent.
            // We still need it for the plugin registration below.
            let system = self
                .prepare_system_prompt(&Instructions::new(query.clone()))
                .await?;
            let system = jewels::redact(&system);

            self.session.add_user(&query).await?;

            // PostPrompt: fire-and-forget after prompt sent
            if let Some(cfg) = CONFIG.get() {
                let sid = self.session.id.to_string();
                run_prompt_hook(
                    &cfg.plugins,
                    HookEvent::PostPrompt,
                    Some(&system),
                    Some(&query),
                    &sid,
                    output_mode,
                )
                .await;
            }

            let mut builder = self.build_sdk_agent(output_mode)?;

            let history_plugin = MemoryHistoryPlugin::new();
            for mut msg in self.session.to_messages() {
                Self::redact_message(&mut msg);
                history_plugin.push(msg).await;
            }
            builder = builder
                .plugin(history_plugin.clone())
                .plugin(crate::plugin::EmbeddedSystemPromptPlugin::new(&system))
                .plugin(crate::plugin::SystemPromptsPlugin::new())
                .plugin(crate::plugin::ConversationModePlugin::new(output_mode))
                .plugin(crate::plugin::SkillsPlugin::new(
                    self.registry.clone(),
                    self.sandbox.clone(),
                ))
                .plugin(crate::plugin::FileSystemPlugin::new())
                .plugin(crate::plugin::DeveloperPlugin::new())
                .plugin(crate::plugin::HelperBinariesPlugin::new())
                .plugin(UserPluginRunner::new(
                    self.session.id.to_string(),
                    output_mode,
                ));

            if AgentConfig::is_debug() {
                builder = builder.plugin(crate::plugin::DebugPlugin::new(
                    &self.session.id.to_string(),
                    &system,
                ));
            }

            let stream_plugin =
                crate::agent::StreamPlugin::new(event_tx.clone(), self.config.retry.clone());
            builder = builder.plugin(stream_plugin);

            let mut agent = builder
                .build()
                .map_err(|e| AppError::Config(e.to_string()))?;
            let _output = agent.run().await?;

            let final_messages = history_plugin.messages().await;
            self.session.sync_from_messages(&final_messages).await?;

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

    pub async fn spawn_subagent(&self, agent_name: Option<String>) -> Result<Self> {
        let tier = agent_name.as_ref().and_then(|name| {
            self.registry
                .agents
                .iter()
                .find(|a| &a.name == name)
                .and_then(|a| a.model.as_deref())
        });
        let model = CONFIG
            .get()
            .map_or(self.model.clone(), |c| c.resolve_model(tier, &self.model));

        let sub_id = if let Some(ref name) = agent_name {
            crate::session::SessionId::subagent(&self.session.id, name)
        } else {
            crate::session::SessionId::new()
        };

        let sub_session = Session::create_with_id(self.pool.clone(), sub_id).await?;
        let config = AgentConfig::subagent(self.config.depth + 1, agent_name);

        Ok(PieAgent::new(
            model,
            self.registry.clone(),
            self.sandbox.clone(),
            self.pool.clone(),
            sub_session,
            config,
        ))
    }
}
