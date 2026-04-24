use crate::agent::{find_subsume_candidate, get_all_agents};
use crate::handler::{build_request, extract_output_text, strip_control_tokens};
use crate::instructions::Instructions;
use crate::providers::Model;
use crate::session::Session;
use crate::skill::get_all_skills;
use crate::tools::subagent::Subagent;
use crate::ui::tui::realm::StreamEvent;
use crate::ui::tui::widgets::tool_display::ToolCallResult;
use aisdk::core::{LanguageModelStreamChunkType, StreamTextResponse};
use itertools::Itertools;
use p1e_srt::SandboxConfig;
use std::sync::Arc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_stream::StreamExt;

/// Environment shared across stream invocations — held by [`InputComponent`].
pub struct StreamContext {
    pub model: Model,
    pub sandbox: Arc<SandboxConfig>,
    pub session_id: uuid::Uuid,
    pub pool: Arc<crate::db::DbPool>,
    pub max_steps: u32,
}

impl From<&crate::ui::tui::components::input::InputComponent> for StreamContext {
    fn from(input: &crate::ui::tui::components::input::InputComponent) -> Self {
        Self {
            model: input.model.clone(),
            sandbox: input.sandbox_settings.clone(),
            session_id: input.session_id,
            pool: input.session_pool.clone(),
            max_steps: input.max_steps,
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

    let history_clone = history.clone();
    let query_clone = query.clone();
    let sandbox_clone = ctx.sandbox.clone();
    let model_clone = ctx.model.clone();

    let mut response = match crate::utils::execute_with_retry("stream_text", move || {
        let history = history_clone.clone();
        let query = query_clone.clone();
        let sandbox = sandbox_clone.clone();
        let model = model_clone.clone();

        async move {
            let model = model.clone();
            let mut req = if let Some(agent) = find_subsume_candidate(&query, &get_all_agents()) {
                let subagent = Subagent::new(
                    model.clone(),
                    get_all_skills(),
                    get_all_agents(),
                    sandbox.clone(),
                );
                subagent
                    .build_request(&agent, &query.raw, 0, None)
                    .map_err(|e| anyhow::anyhow!(e))?
            } else {
                build_request(&model, &query, &history, sandbox, ctx.max_steps)
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
                let event = CompletedToolCall {
                    name: std::mem::take(&mut self.pending_tool.name),
                    params: std::mem::take(&mut self.pending_tool.params),
                    output,
                };
                let display = if event.params.is_empty() {
                    event.name.clone()
                } else {
                    format!("{}: {}", event.name, event.params)
                };
                persist_tool_call(self.session, &event.name, &display, &event.output);
                let _ = self.event_tx.send(event.into());
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

fn tool_output_text(result: &aisdk::core::ToolResultInfo) -> String {
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
