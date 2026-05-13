use crate::agent::{OutputMode, find_subsume_candidate};
use crate::config::CONFIG;
use crate::db::DbPool;
use crate::hook::{HookContext, HookContextData, HookEvent, HookOutcome, PromptData};
use crate::instructions::Instructions;
use crate::prompt::SystemPrompt;
use crate::providers::{Model, strip_control_tokens};
use crate::registry::Registry;
use crate::session::{HistoryEntry, Role, Session, ToolCall};
use crate::skill::Skill;
use crate::tools::plan::plan_tools;
use crate::tools::{
    execute_skill_script_tool, glob_tool, list_directory_tool, load_references_tool,
    load_skills_tool, read_file_tool, replace_tool, shell, subagent_tool, websearch,
    write_file_tool,
};
use crate::utils::anonymize_path;
use agentsdk::core::language_model::LanguageModelResponseContentType;
use agentsdk::core::tools::{ToolCallInfo, ToolDetails, ToolResultInfo};
use agentsdk::core::utils::step_count_is;
use agentsdk::core::{
    AssistantMessage, LanguageModelRequest, LanguageModelStreamChunkType, Message,
    StreamTextResponse, UserMessage,
};
use agentsdk::extensions::Extensions;
use anyhow::{Context, Result};
use futures::future::BoxFuture;
use itertools::Itertools;
use p1e_sandbox::SandboxConfig;
use serde::Deserialize;
use std::collections::HashSet;
use std::fmt::Write;
use std::sync::{Arc, Mutex, PoisonError};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_stream::StreamExt;

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
    pub model: Model,
    pub registry: Arc<Registry>,
    pub sandbox: Arc<SandboxConfig>,
    pub pool: Arc<DbPool>,
    pub session: Session,
    pub config: AgentConfig,
    loaded_skills: Arc<Mutex<HashSet<String>>>,
    #[allow(dead_code)]
    loaded_refs: Arc<Mutex<HashSet<String>>>,
}

#[derive(Debug, Clone, Deserialize)]
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
        model: Model,
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
            loaded_refs: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    async fn prepare_system_prompt(&self, query: &Instructions) -> Result<String> {
        let mut query_mentions = query.clone();

        if self.config.history_limit > 0 {
            self.session
                .history_entries()
                .iter()
                .rev()
                .take(self.config.history_limit as usize)
                .filter(|e| e.role() == Role::User)
                .for_each(|e| query_mentions.merge_mentions(&e.content()));
        }

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
                .unwrap_or_default()
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

    pub async fn run_pre_prompt_hooks(
        &self,
        system: &str,
        query: &str,
        output_mode: OutputMode,
    ) -> Result<(Option<String>, Option<String>)> {
        let data = HookContextData::Prompt(PromptData {
            system: Some(system.to_string()),
            query: Some(query.to_string()),
        });
        match self.run_hook(HookEvent::PrePrompt, data, output_mode).await {
            Ok((_, HookContextData::Prompt(p))) => Ok((p.system, p.query)),
            Ok(_) => Ok((None, None)),
            Err(e) => {
                tracing::warn!("prompt.pre infrastructure failure: {}", e);
                Ok((None, None))
            }
        }
    }

    pub async fn run_post_prompt_hooks(
        &self,
        system: &str,
        query: &str,
        output_mode: OutputMode,
    ) -> Result<()> {
        let data = HookContextData::Prompt(PromptData {
            system: Some(system.to_string()),
            query: Some(query.to_string()),
        });
        if let Err(e) = self
            .run_hook(HookEvent::PostPrompt, data, output_mode)
            .await
        {
            tracing::warn!("prompt.post hook failure: {}", e);
        }
        Ok(())
    }

    pub async fn run_pre_completion_hooks(
        &self,
        output: &str,
        output_mode: OutputMode,
    ) -> Result<Option<String>> {
        let data = HookContextData::Prompt(PromptData {
            system: None,
            query: Some(output.to_string()),
        });
        match self
            .run_hook(HookEvent::PreCompletion, data, output_mode)
            .await
        {
            Ok((_, HookContextData::Prompt(p))) => {
                if let Some(feedback) = p.query
                    && feedback != output
                {
                    return Ok(Some(feedback));
                }
                Ok(None)
            }
            Ok(_) => Ok(None),
            Err(e) => {
                tracing::warn!("completion.pre infrastructure failure: {}", e);
                Ok(None)
            }
        }
    }

    pub async fn run_post_completion_hooks(&self, output: &str, output_mode: OutputMode) {
        let data = HookContextData::Prompt(PromptData {
            system: None,
            query: Some(output.to_string()),
        });
        if let Err(e) = self
            .run_hook(HookEvent::PostCompletion, data, output_mode)
            .await
        {
            tracing::warn!("completion.post hook failure: {}", e);
        }
    }

    fn build_tools(&self, output_mode: OutputMode) -> Result<Vec<agentsdk::core::tools::Tool>> {
        let session_id = self.session.id.to_string();
        let mut tools = vec![
            read_file_tool(),
            write_file_tool(),
            replace_tool(),
            list_directory_tool(),
            glob_tool(),
            shell(self.sandbox.clone()),
            load_skills_tool(self.registry.clone(), Some(self.loaded_skills.clone())),
            load_references_tool(self.loaded_refs.clone()),
            execute_skill_script_tool(self.sandbox.clone()),
            websearch(self.sandbox.clone()),
            subagent_tool(
                self.model.clone(),
                self.registry.clone(),
                self.sandbox.clone(),
                self.pool.clone(),
            ),
        ];

        for tool in plan_tools(self.pool.clone(), session_id.clone())
            .context("failed to build plan tools")?
        {
            tools.push(tool);
        }

        Ok(crate::tools::wrap_tools_with_hooks(
            tools,
            &session_id,
            output_mode,
        ))
    }

    fn build_messages(&self, query: &Instructions) -> Vec<Message> {
        let skill_messages = self.build_skill_messages(query);

        if self.config.history_limit == 0 {
            let mut msgs = skill_messages;
            msgs.push(Message::User(UserMessage::new(&query.raw)));
            return msgs;
        }

        let mut messages = self.build_history_messages();

        let already_has_query = messages.last().is_some_and(|m| {
            if let Message::User(u) = m {
                u.content == query.raw
            } else {
                false
            }
        });

        messages.extend(skill_messages);

        if !already_has_query {
            messages.push(Message::User(UserMessage::new(&query.raw)));
        }

        messages
    }

    fn build_skill_messages(&self, query: &Instructions) -> Vec<Message> {
        let mut merged_mentions = query.clone();
        self.session
            .history_entries()
            .iter()
            .rev()
            .take(self.config.history_limit as usize)
            .filter_map(HistoryEntry::user)
            .for_each(|e| merged_mentions.merge_mentions(&e.content()));

        let mentions: Vec<String> = merged_mentions.mentions.iter().cloned().collect();
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

        let call_id = uuid::Uuid::now_v7();
        let names: Vec<String> = resolved.iter().map(|s| s.name.clone()).collect();

        let tool_call_info = ToolCallInfo {
            call_id,
            tool: ToolDetails {
                name: "load_skills".to_string(),
                id: String::new(),
            },
            input: serde_json::json!({ "skills": names }),
            extensions: Extensions::default(),
        };

        let mut content = String::new();
        for skill in &resolved {
            write!(
                content,
                "## Skill: {}\n{}\n---\n",
                skill.name, skill.content
            )
            .ok();
        }

        let mut tool_result = ToolResultInfo::new("load_skills");
        tool_result.call_id = call_id;
        tool_result.output(serde_json::Value::String(content));

        vec![
            Message::Assistant(AssistantMessage::new(
                LanguageModelResponseContentType::ToolCall(tool_call_info),
                None,
            )),
            Message::Tool(tool_result),
        ]
    }

    fn build_history_messages(&self) -> Vec<Message> {
        self.session
            .history_entries()
            .iter()
            .rev()
            .take(self.config.history_limit as usize)
            .rev()
            .flat_map(|entry| match entry {
                HistoryEntry::User(c) => vec![Message::User(UserMessage::new(c))],
                HistoryEntry::Assistant(c) => {
                    vec![Message::Assistant(AssistantMessage::from(c.clone()))]
                }
                HistoryEntry::Tool(tc) => {
                    let mut msgs = Vec::new();
                    let tool_call_info = ToolCallInfo {
                        call_id: tc.call_id,
                        tool: ToolDetails {
                            name: tc.tool_name.clone(),
                            id: String::new(),
                        },
                        input: tc.params.clone(),
                        extensions: Extensions::default(),
                    };
                    msgs.push(Message::Assistant(AssistantMessage::new(
                        LanguageModelResponseContentType::ToolCall(tool_call_info),
                        None,
                    )));

                    if let Some(res) = &tc.output {
                        let mut tool_result = ToolResultInfo::new(&tc.tool_name);
                        tool_result.call_id = tc.call_id;
                        match res {
                            Ok(v) | Err(v) => tool_result.output(v.clone()),
                        }
                        msgs.push(Message::Tool(tool_result));
                    }
                    msgs
                }
                HistoryEntry::System(_) => vec![],
            })
            .collect()
    }

    pub fn run<'a>(&'a mut self, query_str: &'a str) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let (event_tx, mut _event_rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
            let (abort_tx, mut abort_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

            let query = if let Some(ref name) = self.config.agent_name
                && !query_str.contains(name)
            {
                format!("{name} {query_str}")
            } else {
                query_str.to_string()
            };

            let res = self
                .stream(&query, OutputMode::Md, event_tx, &mut abort_rx)
                .await;

            // Explicitly keep abort_tx alive until the stream is done
            drop(abort_tx);
            res
        })
    }

    pub fn stream<'a>(
        &'a mut self,
        query_str: &'a str,
        output_mode: OutputMode,
        event_tx: UnboundedSender<AgentEvent>,
        abort_rx: &'a mut UnboundedReceiver<()>,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let query = Instructions::new(query_str);

            if self.config.depth < 2
                && let Some(agent_name) = find_subsume_candidate(&query, &self.registry.agents)
                && self.config.agent_name.as_ref() != Some(&agent_name)
            {
                let mut subagent = self.spawn_subagent(Some(agent_name)).await;
                return subagent
                    .stream(query_str, output_mode, event_tx, abort_rx)
                    .await;
            }

            let mut current_query_raw = query_str.to_string();
            let mut loop_count = 0;

            loop {
                // Add a second delay between requests to mitigate 429 errors.
                if loop_count > 0 {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
                loop_count += 1;

                let current_query = Instructions::new(current_query_raw.clone());
                self.session.add_user(&current_query.raw).await?;

                let agent_clone = self.clone();
                let query_inner = current_query.clone();

                let mut response = crate::utils::execute_with_retry("stream_text", move || {
                    let agent = agent_clone.clone();
                    let query = query_inner.clone();
                    async move {
                        let mut system = agent.prepare_system_prompt(&query).await?;
                        let mut query_text = query.raw.clone();

                        let (new_system, new_query) = agent
                            .run_pre_prompt_hooks(&system, &query_text, output_mode)
                            .await?;
                        if let Some(s) = new_system {
                            system = s;
                        }
                        if let Some(q) = new_query {
                            query_text = q;
                        }

                        let _ = agent
                            .run_post_prompt_hooks(&system, &query_text, output_mode)
                            .await;

                        let messages = agent.build_messages(&Instructions::new(query_text));
                        let tools = agent.build_tools(output_mode)?;

                        let mut builder = LanguageModelRequest::builder()
                            .model(agent.model.clone())
                            .system(&system)
                            .messages(messages)
                            .stop_when(step_count_is(agent.config.max_steps as usize));

                        for tool in tools {
                            builder = builder.with_tool(tool);
                        }

                        builder
                            .build()
                            .stream_text()
                            .await
                            .map_err(|e| anyhow::anyhow!(e))
                    }
                })
                .await
                .map_err(|e| anyhow::anyhow!(e).context("stream_text failed"))?;

                let mut processor = StreamProcessor::new(&mut self.session, event_tx.clone());
                processor.handle(&mut response, abort_rx).await;

                let results = response.tool_results().await;
                let output = extract_output_text(&processor.accumulated, results.as_deref());
                let output = strip_control_tokens(&output);

                if loop_count <= self.config.max_retries
                    && let Some(feedback) =
                        self.run_pre_completion_hooks(&output, output_mode).await?
                {
                    tracing::info!("PreCompletion hook triggered, re-running LLM with feedback");
                    let assistant_text = response.text().await.unwrap_or_default();
                    self.session.add_assistant(&assistant_text).await?;
                    current_query_raw = feedback;
                    continue;
                }

                if !output.is_empty() {
                    self.session.add_assistant(&output).await?;
                }
                let _ = event_tx.send(AgentEvent::Done(output.clone()));
                self.run_post_completion_hooks(&output, output_mode).await;
                return Ok(output);
            }
        })
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

pub fn extract_output_text(text: &str, tool_results: Option<&[ToolResultInfo]>) -> String {
    if !text.is_empty() {
        let subagent_res = tool_results.and_then(|results| {
            results
                .iter()
                .rfind(|r| {
                    crate::tools::ToolName::from_str_lossy(&r.tool.name)
                        == Some(crate::tools::ToolName::Subagent)
                })
                .and_then(|r| r.output.as_ref().ok()?.as_str())
        });

        if let Some(res) = subagent_res
            && !res.is_empty()
        {
            return res.to_string();
        }
        return text.to_string();
    }

    tool_results
        .and_then(|results| {
            results
                .iter()
                .rfind(|r| {
                    crate::tools::ToolName::from_str_lossy(&r.tool.name)
                        == Some(crate::tools::ToolName::Shell)
                })
                .or_else(|| results.last())?
                .output
                .as_ref()
                .ok()?
                .as_str()
        })
        .unwrap_or_default()
        .to_string()
}

struct StreamProcessor<'a> {
    session: &'a mut Session,
    event_tx: UnboundedSender<AgentEvent>,
    pub accumulated: String,
}

impl<'a> StreamProcessor<'a> {
    fn new(session: &'a mut Session, event_tx: UnboundedSender<AgentEvent>) -> Self {
        Self {
            session,
            event_tx,
            accumulated: String::new(),
        }
    }

    async fn handle(
        &mut self,
        response: &mut StreamTextResponse,
        abort_rx: &mut UnboundedReceiver<()>,
    ) {
        loop {
            tokio::select! {
                chunk = response.stream.next() => {
                    let Some(chunk) = chunk else { break; };
                    if !self.process_chunk(chunk).await { break; }
                }
                _ = abort_rx.recv() => break,
            }
        }
    }

    async fn process_chunk(&mut self, chunk: LanguageModelStreamChunkType) -> bool {
        match chunk {
            LanguageModelStreamChunkType::TextDelta(delta) => {
                let cleaned = strip_control_tokens(&delta);
                if !cleaned.is_empty() {
                    self.accumulated.push_str(&cleaned);
                    let _ = self.event_tx.send(AgentEvent::Delta(cleaned));
                }
            }
            LanguageModelStreamChunkType::ToolCallAvailable(info) => {
                let info = ToolCall {
                    call_id: info.call_id,
                    tool_name: info.tool.name.clone(),
                    params: info.input.clone(),
                    output: None,
                };
                let _ = self.session.record_tool_call(info).await;
            }
            LanguageModelStreamChunkType::ToolCallEnd(result) => {
                let call_id = result.call_id;
                let name = result.tool.name.clone();
                let output = match &result.output {
                    Ok(v) => Some(Ok(v.clone())),
                    Err(e) => Some(Err(serde_json::json!(e.to_string()))),
                };

                let info = ToolCall {
                    call_id,
                    tool_name: name.clone(),
                    params: serde_json::Value::Null,
                    output,
                };
                let merged = self.session.record_tool_call(info).await;

                let params = merged
                    .as_ref()
                    .map_or_else(|_| serde_json::Value::Null, |m| m.params.clone());
                let params_str = format_tool_params(&params);
                let params_str = anonymize_path(&params_str);
                let display = if params_str.is_empty() {
                    name.clone()
                } else {
                    format!("{name}: {params_str}")
                };

                let output_str = result
                    .output
                    .as_ref()
                    .ok()
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let output_str = anonymize_path(&output_str);

                let tool = crate::tools::ToolName::from_str_lossy(&name);
                let is_plan_tool = tool.is_some_and(crate::tools::ToolName::is_plan_tool);
                if is_plan_tool {
                    let _ = self.event_tx.send(AgentEvent::PlanUpdate);
                }

                let show_tool = !is_plan_tool || CONFIG.get().is_some_and(|c| c.debug);
                if show_tool {
                    let _ = self.event_tx.send(AgentEvent::ToolCall {
                        name,
                        display,
                        output: output_str,
                    });
                }
            }
            LanguageModelStreamChunkType::Failed(err) => {
                let _ = self.event_tx.send(AgentEvent::Error(err));
                return false;
            }
            _ => {}
        }
        true
    }
}

fn format_tool_params(input: &serde_json::Value) -> String {
    input
        .as_object()
        .map(|obj| {
            obj.iter()
                .map(|(k, v)| {
                    let val = match v {
                        serde_json::Value::String(s) => s.clone(),
                        serde_json::Value::Array(arr) => {
                            arr.iter().filter_map(|v| v.as_str()).join(", ")
                        }
                        other => other.to_string(),
                    };
                    format!("{k}={val}")
                })
                .join(", ")
        })
        .unwrap_or_default()
}
