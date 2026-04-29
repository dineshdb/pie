use crate::agent::find_subsume_candidate;
use crate::handler::{extract_output_text, strip_control_tokens};
use crate::instructions::Instructions;
use crate::prompt::SystemPrompt;
use crate::providers::Model;
use crate::session::{Role, Session};
use crate::tools::subagent::Subagent;
use crate::tools::tasks::task_tools;
use crate::tools::{
    execute_skill_script_tool, load_references_tool, load_skills_tool, read_file_tool,
    replace_tool, shell, subagent_tool, write_file_tool,
};
use crate::ui::tui::components::input::InputComponent;
use crate::ui::tui::realm::StreamEvent;
use crate::ui::tui::widgets::tool_display::ToolCallResult;
use crate::utils::execute_with_retry;
use agentsdk::core::utils::step_count_is;
use agentsdk::core::{
    AssistantMessage, LanguageModelRequest, LanguageModelStreamChunkType, Message,
    StreamTextResponse, UserMessage,
};
use anyhow::Context;
use itertools::Itertools;
use p1e_sandbox::SandboxConfig;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_stream::StreamExt;

/// Environment shared across stream invocations — held by [`InputComponent`].
pub struct StreamContext {
    pub model: Model,
    pub sandbox: Arc<SandboxConfig>,
    pub session_id: uuid::Uuid,
    pub pool: Arc<crate::db::DbPool>,
    pub max_steps: u32,
    pub registry: Arc<crate::registry::Registry>,
    pub task_list: crate::tools::tasks::SharedTaskList,
}

impl From<&InputComponent> for StreamContext {
    fn from(input: &InputComponent) -> Self {
        Self {
            model: input.model.clone(),
            sandbox: input.sandbox_settings.clone(),
            session_id: input.session_id,
            pool: input.session_pool.clone(),
            max_steps: input.max_steps,
            registry: input.registry.clone(),
            task_list: input.task_list.clone(),
        }
    }
}

pub async fn spawn_stream(
    ctx: StreamContext,
    query: String,
    event_tx: UnboundedSender<StreamEvent>,
    mut abort_rx: UnboundedReceiver<()>,
) {
    let query = Instructions::new(query);
    let mut session = match Session::load(ctx.pool, ctx.session_id) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("failed to load session: {e}");
            let _ = event_tx.send(StreamEvent::Error(e.to_string()));
            return;
        }
    };
    let history = session.history_entries().to_vec();
    // Persist user message before streaming so tool calls land after it in DB order.
    let _ = session.add_user(&query.raw);
    let mut response = match execute_with_retry("stream_text", move || {
        let history = history.clone();
        let query = query.clone();
        let sandbox = ctx.sandbox.clone();
        let model = ctx.model.clone();
        let registry = ctx.registry.clone();
        let task_list = ctx.task_list.clone();

        async move {
            let mut req = if let Some(agent) = find_subsume_candidate(&query, &registry.agents) {
                let subagent = Subagent::new(
                    model.clone(),
                    registry.clone(),
                    sandbox.clone(),
                    task_list.clone(),
                );
                subagent
                    .build_request(&agent, &query.raw, 0, None)
                    .map_err(|e| anyhow::anyhow!(e))?
            } else {
                history
                    .iter()
                    .filter(|e| e.role == Role::User)
                    .for_each(|e| query.clone().merge_mentions(&e.content));

                let sp = SystemPrompt::new(&registry.skills, &registry.agents)
                    .resolve(&query)
                    .with_mode(crate::prompt::RunMode::Tui);
                let loaded_skills = Arc::new(Mutex::new(
                    sp.loaded_skills
                        .iter()
                        .map(ToString::to_string)
                        .collect::<HashSet<String>>(),
                ));

                let system = sp.render();
                let messages = history
                    .iter()
                    .filter_map(|entry| match entry.role {
                        Role::User => Some(Message::User(UserMessage::new(&entry.content))),
                        Role::Assistant => Some(Message::Assistant(AssistantMessage::from(
                            entry.content.clone(),
                        ))),
                        _ => None,
                    })
                    .chain(std::iter::once(Message::User(UserMessage::new(&query.raw))))
                    .collect();

                let loaded_refs = Arc::new(Mutex::new(HashSet::new()));
                let mut builder = LanguageModelRequest::builder()
                    .model(model.clone())
                    .system(&system)
                    .messages(messages)
                    .with_tool(shell(sandbox.clone(), task_list.clone()))
                    .with_tool(read_file_tool())
                    .with_tool(write_file_tool(task_list.clone()))
                    .with_tool(replace_tool(task_list.clone()))
                    .with_tool(load_skills_tool(
                        registry.clone(),
                        Some(loaded_skills.clone()),
                    ))
                    .with_tool(load_references_tool(loaded_refs))
                    .with_tool(execute_skill_script_tool(sandbox.clone()))
                    .with_tool(subagent_tool(
                        model.clone(),
                        registry.clone(),
                        sandbox.clone(),
                        task_list.clone(),
                    ))
                    .stop_when(step_count_is(ctx.max_steps as usize));

                for tool in task_tools(&task_list).context("failed to build task tools")? {
                    builder = builder.with_tool(tool);
                }
                builder.build()
            };
            req.stream_text().await.map_err(|e| anyhow::anyhow!(e))
        }
    })
    .await
    {
        Ok(r) => r,
        Err(e) => {
            let _ = event_tx.send(StreamEvent::Error(e.to_string()));
            return;
        }
    };

    let mut processor = StreamProcessor::new(&mut session, event_tx);
    processor.handle(&mut response, &mut abort_rx).await;
}

struct StreamProcessor<'a> {
    session: &'a mut Session,
    event_tx: UnboundedSender<StreamEvent>,
    accumulated: String,
    pending_tool: PendingToolCall,
}

impl<'a> StreamProcessor<'a> {
    fn new(session: &'a mut Session, event_tx: UnboundedSender<StreamEvent>) -> Self {
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

        let results = response.tool_results().await;
        tracing::debug!(len = self.accumulated.len(), ?results, "stream finished");
        let output = extract_output_text(&self.accumulated, results.as_deref());
        let output = strip_control_tokens(&output);

        if !output.is_empty() {
            let _ = self.session.add_assistant(&output);
        }

        let _ = self.event_tx.send(StreamEvent::Done(output));
    }

    fn process_chunk(&mut self, chunk: LanguageModelStreamChunkType) -> bool {
        match chunk {
            LanguageModelStreamChunkType::TextDelta(delta) => {
                let cleaned = strip_control_tokens(&delta);
                if !cleaned.is_empty() {
                    self.accumulated.push_str(&cleaned);
                    let _ = self
                        .event_tx
                        .send(StreamEvent::Delta(self.accumulated.clone()));
                }
            }
            LanguageModelStreamChunkType::ToolCallStart(details) => {
                self.pending_tool.name.clone_from(&details.name);
            }
            LanguageModelStreamChunkType::ToolCallAvailable(info) => {
                self.pending_tool.params = format_tool_params(&info.input);
            }
            LanguageModelStreamChunkType::ToolCallEnd(result) => {
                let output = tool_output_text(&result);
                let name = std::mem::take(&mut self.pending_tool.name);
                let params = std::mem::take(&mut self.pending_tool.params);

                let event = CompletedToolCall {
                    name: name.clone(),
                    params,
                    output,
                };
                let display = if event.params.is_empty() {
                    event.name.clone()
                } else {
                    format!("{}: {}", event.name, event.params)
                };

                // For task tools, also trigger a TaskUpdate event to refresh task UI
                if name == "task_add" || name == "task_update" {
                    let _ = self.event_tx.send(StreamEvent::TaskUpdate);
                }

                persist_tool_call(self.session, &event.name, &display, &event.output);

                let is_task_tool = name == "task_add" || name == "task_update";
                let show_tool =
                    !is_task_tool || crate::config::CONFIG.get().is_some_and(|c| c.debug);

                if show_tool {
                    let _ = self.event_tx.send(event.into());
                }
            }
            LanguageModelStreamChunkType::Failed(err) => {
                let _ = self.event_tx.send(StreamEvent::Error(err));
                return false;
            }
            other => {
                tracing::trace!(?other, "stream chunk skipped");
            }
        }
        true
    }
}

/// Format and persist a tool call to the session DB.
fn persist_tool_call(session: &mut Session, name: &str, display: &str, output: &str) {
    let tool = ToolCallResult::new(name, output);
    let result_line = tool.to_string();
    let content = if result_line.is_empty() {
        display.to_string()
    } else {
        format!("{display} → {result_line}")
    };
    let _ = session.add_tool(&content);
}

/// Accumulates tool call state across Start/Available/End chunks.
#[derive(Default)]
struct PendingToolCall {
    name: String,
    params: String,
}

struct CompletedToolCall {
    name: String,
    params: String,
    output: String,
}

impl From<CompletedToolCall> for StreamEvent {
    fn from(call: CompletedToolCall) -> StreamEvent {
        let display = if call.params.is_empty() {
            call.name.clone()
        } else {
            format!("{}: {}", call.name, call.params)
        };
        StreamEvent::ToolCall {
            name: call.name,
            display,
            output: call.output,
        }
    }
}

fn tool_output_text(result: &agentsdk::core::ToolResultInfo) -> String {
    result
        .output
        .as_ref()
        .ok()
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// Format tool input params as a compact single-line summary.
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
