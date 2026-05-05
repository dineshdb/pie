use crate::agent::{Interactivity, find_subsume_candidate};
use crate::db::DbPool;
use crate::instructions::Instructions;
use crate::prompt::SystemPrompt;
use crate::providers::{Model, strip_control_tokens};
use crate::registry::Registry;
use crate::session::{Role, Session};
use crate::tools::plan::plan_tools;
use crate::tools::{
    execute_skill_script_tool, glob_tool, list_directory_tool, load_references_tool,
    load_skills_tool, read_file_tool, replace_tool, shell, subagent_tool, websearch,
    write_file_tool,
};

use crate::utils::anonymize_path;
use agentsdk::core::utils::step_count_is;
use agentsdk::core::{
    AssistantMessage, LanguageModelRequest, LanguageModelStreamChunkType, Message,
    StreamTextResponse, UserMessage,
};
use anyhow::{Context, Result};
use futures::future::BoxFuture;
use itertools::Itertools;
use p1e_sandbox::SandboxConfig;
use std::collections::HashSet;
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

#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Number of previous history entries to include (0 for none)
    pub history_limit: u32,
    pub use_hooks: bool,
    /// Maximum number of completion retries for self-correction (0 to disable)
    pub max_retries: u32,
    pub max_steps: u32,
    pub depth: u32,
    pub agent_name: Option<String>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            history_limit: 10,
            use_hooks: true,
            max_retries: 3,
            max_steps: 20,
            depth: 0,
            agent_name: None,
        }
    }
}

impl AgentConfig {
    pub fn subagent(depth: u32, agent_name: Option<String>) -> Self {
        Self {
            history_limit: 0,
            use_hooks: false,
            max_retries: 0,
            max_steps: 20,
            depth,
            agent_name,
        }
    }
}

#[derive(Clone)]
pub struct PieAgent {
    pub model: Model,
    pub registry: Arc<Registry>,
    pub sandbox: Arc<SandboxConfig>,
    pub pool: Arc<DbPool>,
    pub session: Session,
    pub config: AgentConfig,
    pub loaded_skills: Arc<Mutex<HashSet<String>>>,
    pub loaded_refs: Arc<Mutex<HashSet<String>>>,
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

    async fn prepare_system_prompt(
        &self,
        query: &Instructions,
        interactivity: Interactivity,
    ) -> Result<(String, Vec<String>)> {
        let mut query_mentions = query.clone();

        if self.config.history_limit > 0 {
            self.session
                .history_entries()
                .iter()
                .rev()
                .take(self.config.history_limit as usize)
                .filter(|e| e.role == Role::User)
                .for_each(|e| query_mentions.merge_mentions(&e.content));
        }

        let sp = SystemPrompt::new(
            &self.registry.skills,
            &self.registry.agents,
            &self.registry.plugins,
        )
        .with_plan(self.pool.clone(), self.session.id.to_string())
        .with_agent(self.config.agent_name.as_deref())
        .resolve(&query_mentions)
        .with_interactivity(interactivity);

        {
            let mut loaded = self
                .loaded_skills
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            for skill in &sp.loaded_skills {
                loaded.insert(skill.name.clone());
            }
        }

        let sys = sp.render()?;
        if self.config.use_hooks {
            let (final_sys, warnings) = self.run_pre_prompt_hooks(sys, query).await?;
            tracing::debug!(size = final_sys.len(), "final system prompt ready");
            Ok((final_sys, warnings))
        } else {
            tracing::debug!(size = sys.len(), "system prompt ready (no hooks)");
            Ok((sys, Vec::new()))
        }
    }

    async fn run_pre_prompt_hooks(
        &self,
        system_prompt: String,
        query: &Instructions,
    ) -> Result<(String, Vec<String>)> {
        let mut system = system_prompt;
        let mut warnings = Vec::new();

        let Some(cfg) = crate::config::CONFIG.get() else {
            return Ok((system, warnings));
        };

        let ctx = crate::hook::HookContext::new(
            crate::hook::HookEvent::PrePrompt,
            std::env::current_dir()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string(),
            self.session.id.to_string(),
            crate::hook::HookContextData::Prompt(crate::hook::PromptData {
                system: Some(system.clone()),
                query: Some(query.raw.clone()),
            }),
        );

        match cfg.hooks.run(crate::hook::HookEvent::PrePrompt, &ctx).await {
            Ok((outcomes, transformed_data)) => {
                let mut errors = Vec::new();
                for outcome in &outcomes {
                    if let crate::hook::HookOutcome::Error { .. } = outcome {
                        errors.push(outcome.format());
                    }
                }

                if !errors.is_empty() {
                    return Err(anyhow::anyhow!(
                        "Prompt rejected by validation hooks:\n{}",
                        errors.join("\n")
                    ));
                }

                if let crate::hook::HookContextData::Prompt(p) = transformed_data
                    && let Some(s) = p.system
                {
                    system = s;
                }

                for outcome in outcomes {
                    if let crate::hook::HookOutcome::Warning { .. } = outcome {
                        warnings.push(outcome.format());
                    }
                }

                Ok((system, warnings))
            }
            Err(e) => Err(e),
        }
    }

    pub async fn run_pre_completion_hooks(&self, output: &str) -> Result<Option<String>> {
        let Some(cfg) = crate::config::CONFIG.get() else {
            return Ok(None);
        };

        let ctx = crate::hook::HookContext::new(
            crate::hook::HookEvent::PreCompletion,
            std::env::current_dir()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string(),
            self.session.id.to_string(),
            crate::hook::HookContextData::Prompt(crate::hook::PromptData {
                system: None,
                query: Some(output.to_string()),
            }),
        );

        match cfg
            .hooks
            .run(crate::hook::HookEvent::PreCompletion, &ctx)
            .await
        {
            Ok((_, transformed_data)) => {
                if let crate::hook::HookContextData::Prompt(p) = transformed_data
                    && let Some(feedback) = p.query
                    && feedback != output
                {
                    return Ok(Some(feedback));
                }
                Ok(None)
            }
            Err(e) => {
                tracing::warn!("completion.pre infrastructure failure: {}", e);
                Ok(None)
            }
        }
    }

    fn build_tools(&self) -> Result<Vec<agentsdk::core::tools::Tool>> {
        let session_id = self.session.id.to_string();
        let mut tools = vec![
            shell(self.sandbox.clone(), self.pool.clone(), session_id.clone()),
            read_file_tool(),
            list_directory_tool(),
            glob_tool(),
            write_file_tool(self.pool.clone(), session_id.clone()),
            replace_tool(self.pool.clone(), session_id.clone()),
            load_skills_tool(self.registry.clone(), Some(self.loaded_skills.clone())),
            load_references_tool(self.loaded_refs.clone()),
            execute_skill_script_tool(self.sandbox.clone()),
            websearch(self.sandbox.clone(), self.pool.clone(), session_id.clone()),
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

        Ok(crate::tools::wrap_tools_with_hooks(tools, &session_id))
    }

    fn build_messages(&self, query: &Instructions) -> Vec<Message> {
        if self.config.history_limit == 0 {
            return vec![Message::User(UserMessage::new(&query.raw))];
        }

        self.session
            .history_entries()
            .iter()
            .rev()
            .take(self.config.history_limit as usize)
            .rev()
            .filter_map(|entry| match entry.role {
                Role::User => Some(Message::User(UserMessage::new(&entry.content))),
                Role::Assistant => Some(Message::Assistant(AssistantMessage::from(
                    entry.content.clone(),
                ))),
                _ => None,
            })
            .chain(std::iter::once(Message::User(UserMessage::new(&query.raw))))
            .collect()
    }

    pub fn run<'a>(&'a mut self, query_str: &'a str) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let (event_tx, mut _event_rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
            let (abort_tx, mut abort_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

            let res = self
                .stream(query_str, Interactivity::None, event_tx, &mut abort_rx)
                .await;

            // Explicitly keep abort_tx alive until the stream is done
            drop(abort_tx);
            res
        })
    }

    pub fn stream<'a>(
        &'a mut self,
        query_str: &'a str,
        interactivity: Interactivity,
        event_tx: UnboundedSender<AgentEvent>,
        abort_rx: &'a mut UnboundedReceiver<()>,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let query = Instructions::new(query_str);

            if self.config.depth < 2
                && let Some(agent_name) = find_subsume_candidate(&query, &self.registry.agents)
            {
                let mut subagent = self.spawn_subagent(Some(agent_name));
                return subagent
                    .stream(query_str, interactivity, event_tx, abort_rx)
                    .await;
            }

            let mut current_query_raw = query_str.to_string();
            let mut loop_count = 0;

            loop {
                let current_query = Instructions::new(current_query_raw.clone());
                self.session.add_user(&current_query.raw)?;

                let agent_clone = self.clone();
                let query_inner = current_query.clone();

                let mut response = crate::utils::execute_with_retry("stream_text", move || {
                    let agent = agent_clone.clone();
                    let query = query_inner.clone();
                    async move {
                        let (system, _warnings) =
                            agent.prepare_system_prompt(&query, interactivity).await?;
                        let messages = agent.build_messages(&query);
                        let tools = agent.build_tools()?;

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

                if loop_count < self.config.max_retries
                    && let Some(feedback) = self.run_pre_completion_hooks(&output).await?
                {
                    tracing::info!("PreCompletion hook triggered, re-running LLM with feedback");
                    let assistant_text = response.text().await.unwrap_or_default();
                    self.session.add_assistant(&assistant_text)?;
                    current_query_raw = feedback;
                    loop_count += 1;
                    continue;
                }

                if !output.is_empty() {
                    self.session.add_assistant(&output)?;
                }
                let _ = event_tx.send(AgentEvent::Done(output.clone()));
                return Ok(output);
            }
        })
    }

    pub fn spawn_subagent(&self, agent_name: Option<String>) -> Self {
        #[allow(clippy::expect_used)]
        let sub_session =
            Session::create(self.pool.clone()).expect("failed to create subagent session");
        let config = AgentConfig::subagent(self.config.depth + 1, agent_name);

        PieAgent::new(
            self.model.clone(),
            self.registry.clone(),
            self.sandbox.clone(),
            self.pool.clone(),
            sub_session,
            config,
        )
    }
}

pub fn extract_output_text(
    text: &str,
    tool_results: Option<&[agentsdk::core::ToolResultInfo]>,
) -> String {
    if !text.is_empty() {
        let subagent_res = tool_results.and_then(|results| {
            results
                .iter()
                .rfind(|r| r.tool.name == "subagent")
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
                .rfind(|r| r.tool.name == "shell")
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
    pending_tool: PendingToolCall,
}

impl<'a> StreamProcessor<'a> {
    fn new(session: &'a mut Session, event_tx: UnboundedSender<AgentEvent>) -> Self {
        Self {
            session,
            event_tx,
            accumulated: String::new(),
            pending_tool: PendingToolCall::default(),
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
                    if !self.process_chunk(chunk) { break; }
                }
                _ = abort_rx.recv() => break,
            }
        }
    }

    fn process_chunk(&mut self, chunk: LanguageModelStreamChunkType) -> bool {
        match chunk {
            LanguageModelStreamChunkType::TextDelta(delta) => {
                let cleaned = strip_control_tokens(&delta);
                if !cleaned.is_empty() {
                    self.accumulated.push_str(&cleaned);
                    let _ = self.event_tx.send(AgentEvent::Delta(cleaned));
                }
            }
            LanguageModelStreamChunkType::ToolCallStart(details) => {
                self.pending_tool.name.clone_from(&details.name);
            }
            LanguageModelStreamChunkType::ToolCallAvailable(info) => {
                self.pending_tool.params = format_tool_params(&info.input);
            }
            LanguageModelStreamChunkType::ToolCallEnd(result) => {
                let output = result
                    .output
                    .as_ref()
                    .ok()
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let output = anonymize_path(&output);
                let name = std::mem::take(&mut self.pending_tool.name);
                let params = std::mem::take(&mut self.pending_tool.params);
                let params = anonymize_path(&params);

                let display = if params.is_empty() {
                    name.clone()
                } else {
                    format!("{name}: {params}")
                };

                if name == "plan_set" || name == "plan_step_update" {
                    let _ = self.event_tx.send(AgentEvent::PlanUpdate);
                }

                persist_tool_call(self.session, &name, &display, &output);

                let is_plan_tool = name == "plan_set" || name == "plan_step_update";
                let show_tool =
                    !is_plan_tool || crate::config::CONFIG.get().is_some_and(|c| c.debug);

                if show_tool {
                    let _ = self.event_tx.send(AgentEvent::ToolCall {
                        name,
                        display,
                        output,
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

fn persist_tool_call(session: &mut Session, _name: &str, display: &str, output: &str) {
    let result_line = if output.is_empty() {
        String::new()
    } else {
        output.lines().next().unwrap_or("").to_string()
    };

    let content = if result_line.is_empty() {
        display.to_string()
    } else {
        format!("{display} → {result_line}")
    };
    let _ = session.add_tool(&content);
}

#[derive(Default)]
struct PendingToolCall {
    name: String,
    params: String,
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
