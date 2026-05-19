use crate::agent::{OutputMode, find_subsume_candidate};
use crate::config::CONFIG;
use crate::db::DbPool;
use crate::hook::{HookContext, HookContextData, HookEvent, HookOutcome, PromptData};
use crate::instructions::Instructions;
use crate::prompt::SystemPrompt;
use crate::registry::Registry;
use crate::session::{HistoryEntry, Session, ToolCall};
use crate::skill::Skill;
use crate::tools::plan::{PlanContext, plan_tools};
use crate::tools::{
    execute_skill_script_tool, glob_tool, list_directory_tool, load_references_tool,
    load_skills_tool, read_file_tool, replace_tool, shell, subagent_tool, websearch,
    write_file_tool,
};
use agentsdk::core::tools::Tool;
use agentsdk::openai::api::ChatCompletionRequestUserMessageContent;
use agentsdk::{
    Agent as SdkAgent, AgentListener, CompletionAction, Extensions, Message, Messages,
    PostToolAction, PreToolAction, ToolErrorAction, messages,
};
use anyhow::Result;
use async_trait::async_trait;
use futures::future::BoxFuture;
use p1e_sandbox::SandboxConfig;
use serde::Deserialize;
use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt::Write;
use std::sync::{Arc, Mutex, PoisonError};
use tokio::sync::mpsc::UnboundedSender;

pub enum CompletionVerdict {
    Accepted(Option<String>),
    Rejected(String),
}

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
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            agent_name: None,
            history_limit: 10,
            max_steps: 20,
            depth: 0,
            max_retries: 3,
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
            max_retries: 3,
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

        sp.render().await
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
            .map_err(|e| anyhow::anyhow!(e))
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

    pub async fn run_pre_completion_hooks(
        &self,
        output: &str,
        output_mode: OutputMode,
    ) -> Result<CompletionVerdict> {
        let data = HookContextData::Prompt(PromptData {
            system: None,
            query: Some(output.to_string()),
        });
        let (outcomes, transformed_data) = self
            .run_hook(HookEvent::PreCompletion, data, output_mode)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!("completion.pre infrastructure failure: {e}");
                (vec![], HookContextData::Prompt(PromptData::default()))
            });

        for outcome in &outcomes {
            if let HookOutcome::Error { message, .. } = outcome {
                return Ok(CompletionVerdict::Rejected(message.clone()));
            }
        }

        if let HookContextData::Prompt(p) = transformed_data
            && let Some(transformed) = p.query
            && transformed != output
        {
            return Ok(CompletionVerdict::Accepted(Some(transformed)));
        }

        Ok(CompletionVerdict::Accepted(None))
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

    fn build_tools(&self, output_mode: OutputMode) -> Vec<Tool> {
        let mut tools = vec![
            read_file_tool(),
            write_file_tool(),
            replace_tool(),
            list_directory_tool(),
            glob_tool(),
            shell(),
            load_skills_tool(),
            load_references_tool(),
            execute_skill_script_tool(),
            websearch(),
            subagent_tool(),
        ];

        for tool in plan_tools() {
            tools.push(tool);
        }

        crate::tools::wrap_tools_with_hooks(tools, &self.session.id.to_string(), output_mode)
    }

    fn build_messages(&self, query: &Instructions) -> Vec<Message> {
        let skill_messages = self.build_skill_messages(query);

        if self.config.history_limit == 0 {
            let mut msgs = skill_messages;
            msgs.push(messages::user(&query.raw));
            return msgs;
        }

        let mut messages = self.build_history_messages();

        let already_has_query = messages.last().is_some_and(|m| {
            if let Message::UserMessage(u) = m {
                if let Some(ChatCompletionRequestUserMessageContent::String(ref s)) = u.content {
                    s == &query.raw
                } else {
                    false
                }
            } else {
                false
            }
        });

        messages.extend(skill_messages);

        if !already_has_query {
            messages.push(messages::user(&query.raw));
        }

        messages
    }

    fn build_skill_messages(&self, query: &Instructions) -> Vec<Message> {
        let merged = self.merged_mentions(query);
        let mentions: Vec<String> = merged.mentions.iter().cloned().collect();
        let resolved = Skill::resolve(&self.registry.skills, &mentions);

        if resolved.is_empty() {
            return Vec::new();
        }

        {
            let mut loaded = self
                .loaded_skills
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            for skill in &resolved {
                loaded.insert(skill.name.clone());
            }
        }

        let mut content = String::new();
        for skill in &resolved {
            write!(
                content,
                "## Skill: {}\n{}\n---\n",
                skill.name, skill.content
            )
            .ok();
        }

        let call_id = uuid::Uuid::now_v7();
        let call_msg = messages::assistant_tool_call(
            "load_skills",
            call_id,
            &serde_json::json!({ "skills": resolved.iter().map(|s| s.name.clone()).collect::<Vec<_>>() }),
        );

        vec![call_msg, messages::tool(content, call_id)]
    }

    fn build_history_messages(&self) -> Vec<Message> {
        self.session
            .history_entries()
            .iter()
            .rev()
            .take(self.config.history_limit as usize)
            .rev()
            .flat_map(|entry| match entry {
                HistoryEntry::User(c) => vec![messages::user(c)],
                HistoryEntry::Assistant(c) => vec![messages::assistant(c)],
                HistoryEntry::Tool(tc) => {
                    let mut msgs = Vec::new();
                    let call_id = tc.call_id.to_string();
                    msgs.push(messages::assistant_tool_call(
                        &tc.tool_name,
                        &call_id,
                        &tc.params.clone(),
                    ));

                    if let Some(res) = &tc.output {
                        let content = match res {
                            Ok(v) | Err(v) => v.to_string(),
                        };
                        msgs.push(messages::tool(content, &call_id));
                    }
                    msgs
                }
                HistoryEntry::System(c) => vec![messages::system(c)],
            })
            .collect()
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

    #[allow(clippy::too_many_lines)]
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
                let mut subagent = self.spawn_subagent(Some(agent_name)).await;
                return subagent.stream(query_str, output_mode, event_tx).await;
            }

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

            self.session.add_user(&query).await?;
            self.run_post_prompt_hooks(&system, &query, output_mode)
                .await;

            let mut messages = self.build_messages(&Instructions::new(query.clone()));

            let mut retry_count = 0u32;
            loop {
                let sdk_agent = self.build_sdk_agent(messages.clone(), output_mode)?;

                let mut handler = StreamHandler {
                    system_prompt: system.clone(),
                    event_tx: event_tx.clone(),
                    session: self.session.clone(),
                };

                let history = sdk_agent.run(&mut handler).await?;

                self.session = handler.session;

                let final_text = history
                    .iter()
                    .rev()
                    .find_map(|m| {
                        if let Message::AssistantMessage(a) = m {
                            a.content.clone()
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();

                match self
                    .run_pre_completion_hooks(&final_text, output_mode)
                    .await
                {
                    Ok(CompletionVerdict::Accepted(Some(transformed))) => {
                        self.session.add_assistant(&transformed).await?;
                        let _ = event_tx.send(AgentEvent::Done(transformed.clone()));
                        self.run_post_completion_hooks(&transformed, output_mode)
                            .await;
                        return Ok(transformed);
                    }
                    Ok(CompletionVerdict::Accepted(None)) => {
                        self.session.add_assistant(&final_text).await?;
                        let _ = event_tx.send(AgentEvent::Done(final_text.clone()));
                        self.run_post_completion_hooks(&final_text, output_mode)
                            .await;
                        return Ok(final_text);
                    }
                    Ok(CompletionVerdict::Rejected(reason)) => {
                        retry_count += 1;
                        tracing::warn!(
                            retry = retry_count,
                            reason = %reason,
                            "completion rejected by pre-completion hook"
                        );
                        self.session.add_assistant(&final_text).await?;
                        let correction = format!(
                            "Your previous response was rejected:\n{reason}\n\nPlease fix and retry."
                        );
                        self.session.add_user(&correction).await?;
                        let _ = event_tx.send(AgentEvent::Error(reason));
                        messages = self.build_messages(&Instructions::new(query.clone()));
                    }
                    Err(e) => {
                        tracing::warn!("completion.pre hook error: {}", e);
                        self.session.add_assistant(&final_text).await?;
                        let _ = event_tx.send(AgentEvent::Done(final_text.clone()));
                        self.run_post_completion_hooks(&final_text, output_mode)
                            .await;
                        return Ok(final_text);
                    }
                }
            }
        })
    }

    fn build_sdk_agent(&self, messages: Vec<Message>, output_mode: OutputMode) -> Result<SdkAgent> {
        let tools = self.build_tools(output_mode);

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
                    .messages(Arc::new(messages))
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
                    .build()?,
            )
            .build()
            .map_err(Into::into)
    }

    pub async fn spawn_subagent(&self, agent_name: Option<String>) -> Self {
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

        #[allow(clippy::expect_used)]
        let sub_session = Session::create_with_id(self.pool.clone(), sub_id)
            .await
            .expect("failed to create subagent session");
        let config = AgentConfig::subagent(self.config.depth + 1, agent_name);

        PieAgent::new(
            model,
            self.registry.clone(),
            self.sandbox.clone(),
            self.pool.clone(),
            sub_session,
            config,
        )
    }
}

// ── Stream Handler ──────────────────────────────────────────────────

struct StreamHandler {
    system_prompt: String,
    event_tx: UnboundedSender<AgentEvent>,
    session: Session,
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
        id: &str,
        name: &str,
        arguments: &serde_json::Value,
    ) -> PreToolAction {
        let tc = ToolCall {
            call_id: uuid::Uuid::parse_str(id).unwrap_or_else(|_| uuid::Uuid::now_v7()),
            tool_name: name.to_string(),
            params: arguments.clone(),
            output: None,
        };
        let _ = self.session.record_tool_call(tc).await;

        let _ = self.event_tx.send(AgentEvent::ToolCall {
            name: name.to_string(),
            display: format!("{name}({arguments})"),
            output: String::new(),
        });

        PreToolAction::Continue(None)
    }

    async fn on_tool_post_execute(
        &mut self,
        id: &str,
        name: &str,
        result: &serde_json::Value,
    ) -> PostToolAction {
        let output = if let serde_json::Value::String(s) = result {
            s.clone()
        } else {
            result.to_string()
        };

        let tc = ToolCall {
            call_id: uuid::Uuid::parse_str(id).unwrap_or_else(|_| uuid::Uuid::now_v7()),
            tool_name: name.to_string(),
            params: serde_json::Value::Null,
            output: Some(Ok(result.clone())),
        };
        let _ = self.session.record_tool_call(tc).await;

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

    async fn on_tool_error(&mut self, id: &str, name: &str, error: &str) -> ToolErrorAction {
        let tc = ToolCall {
            call_id: uuid::Uuid::parse_str(id).unwrap_or_else(|_| uuid::Uuid::now_v7()),
            tool_name: name.to_string(),
            params: serde_json::Value::Null,
            output: Some(Err(serde_json::json!(error))),
        };
        let _ = self.session.record_tool_call(tc).await;
        let _ = self
            .event_tx
            .send(AgentEvent::Error(format!("Tool {name} failed: {error}")));
        ToolErrorAction::Continue(None)
    }

    async fn on_completion(&mut self, _text: String) -> CompletionAction {
        CompletionAction::Accept(None)
    }
}
