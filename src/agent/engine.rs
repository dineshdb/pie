use crate::agent::{OutputMode, find_subsume_candidate};
use crate::config::CONFIG;
use crate::db::DbPool;
use crate::error::{AppError, Result};
use crate::hook::{HookContext, HookContextData, HookEvent, HookOutcome, PromptData};
use crate::instructions::Instructions;
use crate::prompt::SystemPrompt;
use crate::registry::Registry;
use crate::session::{HistoryEntry, Session};
use crate::tools::plan::{PlanContext, plan_tools};
use crate::tools::{
    execute_skill_script_tool, glob_tool, list_directory_tool, load_references_tool,
    load_skills_tool, read_file_tool, replace_tool, shell, subagent_tool, websearch,
    write_file_tool,
};
use agentsdk::core::history::HistoryStore;
use agentsdk::core::retry::RetryAction;
use agentsdk::core::tools::Tool;
use agentsdk::error::AgentSdkError;
use agentsdk::{
    Agent as SdkAgent, AgentListener, CompletionAction, Extensions, Messages, PostToolAction,
    PreToolAction, ToolErrorAction,
};
use async_trait::async_trait;
use futures::future::BoxFuture;
use p1e_sandbox::SandboxConfig;
use serde::Deserialize;
use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    Delta(String),
    Done(String),
    Error(String),
    ToolCall {
        name: String,
        display: String,
        output: String,
    },
    PlanUpdate,
}

#[derive(Clone)]
pub struct PieAgent {
    pub model: agentsdk::OpenAI,
    pub registry: Arc<Registry>,
    pub sandbox: Arc<SandboxConfig>,
    pub pool: Arc<DbPool>,
    pub session: Session,
    pub config: AgentConfig,
    loaded_skills: Arc<Mutex<HashSet<String>>>,
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
            loaded_skills: Arc::new(Mutex::new(HashSet::new())),
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

    fn make_hook_ctx(
        &self,
        event: HookEvent,
        data: HookContextData,
        output_mode: OutputMode,
    ) -> HookContext {
        HookContext::new(
            event,
            std::env::current_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from("."))
                .to_string_lossy()
                .to_string(),
            self.session.id.to_string(),
            output_mode,
            data,
        )
    }

    async fn run_hook(
        &self,
        event: HookEvent,
        data: HookContextData,
        output_mode: OutputMode,
    ) -> Result<(Vec<HookOutcome>, HookContextData)> {
        let Some(cfg) = CONFIG.get() else {
            return Ok((vec![], data));
        };
        let ctx = self.make_hook_ctx(event, data, output_mode);
        cfg.plugins
            .run(event, &ctx)
            .await
            .map_err(|e| AppError::Plugin(e.to_string()))
    }

    /// Run a prompt hook and extract (system, query) from the result.
    async fn run_prompt_hook(
        &self,
        event: HookEvent,
        system: Option<&str>,
        query: Option<&str>,
        output_mode: OutputMode,
        warn_ctx: &str,
    ) -> (Option<String>, Option<String>) {
        let data = HookContextData::Prompt(PromptData {
            system: system.map(String::from),
            query: query.map(String::from),
        });
        match self.run_hook(event, data, output_mode).await {
            Ok((_, HookContextData::Prompt(p))) => (p.system, p.query),
            Ok(_) => (None, None),
            Err(e) => {
                tracing::warn!("{warn_ctx} infrastructure failure: {e}");
                (None, None)
            }
        }
    }

    pub async fn run_user_query_post_hooks(
        &self,
        query: &str,
        output_mode: OutputMode,
    ) -> Result<Option<String>> {
        Ok(self
            .run_prompt_hook(
                HookEvent::PostUserQuery,
                None,
                Some(query),
                output_mode,
                "user_query.post",
            )
            .await
            .1)
    }

    pub async fn run_pre_prompt_hooks(
        &self,
        system: &str,
        query: &str,
        output_mode: OutputMode,
    ) -> Result<(Option<String>, Option<String>)> {
        Ok(self
            .run_prompt_hook(
                HookEvent::PrePrompt,
                Some(system),
                Some(query),
                output_mode,
                "prompt.pre",
            )
            .await)
    }

    pub async fn run_post_prompt_hooks(&self, system: &str, query: &str, output_mode: OutputMode) {
        self.run_prompt_hook(
            HookEvent::PostPrompt,
            Some(system),
            Some(query),
            output_mode,
            "prompt.post",
        )
        .await;
    }

    pub async fn run_post_completion_hooks(&self, output: &str, output_mode: OutputMode) {
        self.run_prompt_hook(
            HookEvent::PostCompletion,
            None,
            Some(output),
            output_mode,
            "completion.post",
        )
        .await;
    }

    fn build_tools(&self, output_mode: OutputMode) -> Result<Vec<Tool>> {
        let mut tools = vec![
            read_file_tool()?,
            write_file_tool()?,
            replace_tool()?,
            list_directory_tool()?,
            glob_tool()?,
            shell()?,
            load_skills_tool()?,
            load_references_tool()?,
            execute_skill_script_tool()?,
            websearch()?,
            subagent_tool()?,
        ];

        for tool in plan_tools()? {
            tools.push(tool);
        }

        Ok(crate::tools::wrap_tools_with_hooks(
            tools,
            &self.session.id.to_string(),
            output_mode,
        )?)
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

    async fn prepare_query_and_system(
        &self,
        query_str: &str,
        output_mode: OutputMode,
    ) -> Result<(String, String)> {
        let mut current_query_raw = query_str.to_string();

        if let Ok(Some(transformed)) = self
            .run_user_query_post_hooks(&current_query_raw, output_mode)
            .await
        {
            current_query_raw = transformed;
        }

        let mut query = current_query_raw.clone();
        let mut system = String::new();
        if let Ok((s, q)) = self
            .run_pre_prompt_hooks(&system, &query, output_mode)
            .await
        {
            if let Some(s) = s {
                system = s;
            }
            if let Some(q) = q {
                query = q;
            }
        }

        if system.is_empty() {
            system = self
                .prepare_system_prompt(&Instructions::new(query.clone()))
                .await?;
        }

        Ok((query, system))
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

            let (query, system) = self
                .prepare_query_and_system(query_str, output_mode)
                .await?;

            self.session.add_user(&query).await?;
            self.run_post_prompt_hooks(&system, &query, output_mode)
                .await;

            let sdk_agent = self.build_sdk_agent(output_mode)?;

            let mut handler = StreamHandler {
                system_prompt: system.clone(),
                event_tx: event_tx.clone(),
                session: self.session.clone(),
                api_error_count: 0,
                rate_limit_count: 0,
                retry: self.config.retry.clone(),
                output_mode,
            };

            sdk_agent.run(&mut handler).await?;
            self.session = handler.session;
            self.session.rebuild_cache().await?;

            let final_text = self
                .session
                .history_entries()
                .iter()
                .rev()
                .find_map(|e| match e {
                    HistoryEntry::Assistant(c) => Some(c.clone()),
                    _ => None,
                })
                .unwrap_or_default();

            let _ = event_tx.send(AgentEvent::Done(final_text.clone()));
            self.run_post_completion_hooks(&final_text, output_mode)
                .await;

            Ok(final_text)
        })
    }

    fn build_sdk_agent(&self, output_mode: OutputMode) -> Result<SdkAgent> {
        let tools = self.build_tools(output_mode)?;

        let mut extensions = Extensions::new();
        extensions.insert(self.pool.clone());
        extensions.insert(self.sandbox.clone());
        extensions.insert(self.registry.clone());
        extensions.insert(self.loaded_skills.clone());
        extensions.insert(self.model.clone());
        extensions.insert(PlanContext {
            session_id: self.session.id.to_string(),
        });

        SdkAgent::builder()
            .client(self.model.clone())
            .options(
                agentsdk::AgentOptions::builder()
                    .max_iterations(self.config.max_steps as usize)
                    .history_store({
                        let store: Arc<dyn HistoryStore> = Arc::new(self.session.clone());
                        store
                    })
                    .session_id(self.session.id.to_string())
                    .extensions(extensions)
                    .tool_definitions(Arc::new(
                        tools.iter().map(|t| t.definition.clone()).collect(),
                    ))
                    .tool_executors(Arc::new(
                        tools
                            .into_iter()
                            .map(|t| (t.definition.name.clone(), t.execute.clone()))
                            .collect(),
                    ))
                    .build()
                    .map_err(|e| AppError::Config(e.to_string()))?,
            )
            .build()
            .map_err(Into::into)
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

// ── Stream Handler ──────────────────────────────────────────────────

struct StreamHandler {
    system_prompt: String,
    event_tx: UnboundedSender<AgentEvent>,
    session: Session,
    api_error_count: u32,
    rate_limit_count: u32,
    retry: crate::config::RetryConfig,
    output_mode: OutputMode,
}

#[async_trait]
impl AgentListener for StreamHandler {
    async fn prepare_system_prompt(&mut self, _history: &Messages) -> Option<Cow<'static, str>> {
        Some(Cow::Owned(self.system_prompt.clone()))
    }

    async fn on_text_delta(&mut self, text: &str) {
        if !text.is_empty() {
            let _ = self.event_tx.send(AgentEvent::Delta(text.to_string()));
        }
    }

    async fn on_tool_pre_execute(
        &mut self,
        _id: &str,
        name: &str,
        arguments: &serde_json::Value,
    ) -> PreToolAction {
        let _ = self.event_tx.send(AgentEvent::ToolCall {
            name: name.to_string(),
            display: format!("{name}({arguments})"),
            output: String::new(),
        });

        PreToolAction::Continue(None)
    }

    async fn on_tool_post_execute(
        &mut self,
        _id: &str,
        name: &str,
        result: &serde_json::Value,
    ) -> PostToolAction {
        let output = if let serde_json::Value::String(s) = result {
            s.clone()
        } else {
            result.to_string()
        };

        let _ = self.event_tx.send(AgentEvent::ToolCall {
            name: name.to_string(),
            display: String::new(),
            output: crate::utils::anonymize_path(&output),
        });
        if name.starts_with("plan_") {
            let _ = self.event_tx.send(AgentEvent::PlanUpdate);
        }

        PostToolAction::Continue(None)
    }

    async fn on_tool_error(&mut self, _id: &str, name: &str, error: &str) -> ToolErrorAction {
        let _ = self.event_tx.send(AgentEvent::ToolCall {
            name: name.to_string(),
            display: String::new(),
            output: format!("Error: {error}"),
        });
        let _ = self
            .event_tx
            .send(AgentEvent::Error(format!("Tool {name} failed: {error}")));
        ToolErrorAction::Continue(None)
    }

    async fn on_api_error(&mut self, error: &AgentSdkError) -> RetryAction {
        self.api_error_count += 1;

        if let Some(status) = error.status_code() {
            if status == 429 {
                self.rate_limit_count += 1;
                if self.rate_limit_count > self.retry.rate_limit.max_errors {
                    let _ = self.event_tx.send(AgentEvent::Error(
                        "Too many rate limit errors, aborting".to_string(),
                    ));
                    return RetryAction::DoNotRetry;
                }
                tracing::warn!(status = %status, "rate limited, retrying");
                return RetryAction::Retry(std::time::Duration::from_secs(
                    self.retry.rate_limit.retry_delay_secs,
                ));
            }

            if status.is_server_error() {
                if self.api_error_count > self.retry.api_error.max_errors {
                    let _ = self.event_tx.send(AgentEvent::Error(
                        "Too many API errors, aborting".to_string(),
                    ));
                    return RetryAction::DoNotRetry;
                }
                tracing::warn!(status = %status, count = self.api_error_count, "server error, retrying");
                return RetryAction::Retry(std::time::Duration::from_secs(
                    self.retry.api_error.retry_delay_secs,
                ));
            }
        }

        RetryAction::DoNotRetry
    }

    async fn on_completion(&mut self, text: String) -> CompletionAction {
        let Some(cfg) = CONFIG.get() else {
            return CompletionAction::Accept(None);
        };

        let data = HookContextData::Prompt(PromptData {
            system: None,
            query: Some(text.clone()),
        });
        let ctx = HookContext::new(
            HookEvent::PreCompletion,
            std::env::current_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from("."))
                .to_string_lossy()
                .to_string(),
            self.session.id.to_string(),
            self.output_mode,
            data,
        );

        match cfg.plugins.run(HookEvent::PreCompletion, &ctx).await {
            Ok((outcomes, HookContextData::Prompt(p))) => {
                for outcome in &outcomes {
                    if let HookOutcome::Error { message, .. } = outcome {
                        return CompletionAction::Reject {
                            reason: message.clone(),
                        };
                    }
                }
                if let Some(transformed) = p.query
                    && transformed != text
                {
                    return CompletionAction::Accept(Some(transformed));
                }
                CompletionAction::Accept(None)
            }
            Err(e) => {
                tracing::warn!("completion.pre hook error: {e}");
                CompletionAction::Accept(None)
            }
            Ok(_) => CompletionAction::Accept(None),
        }
    }
}
