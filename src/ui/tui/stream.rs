use crate::agent::{find_subsume_candidate, get_all_agents};
use crate::handler::{build_request, extract_output_text, strip_control_tokens};
use crate::instructions::Instructions;
use crate::providers::Model;
use crate::session::Session;
use crate::skill::get_all_skills;
use crate::tools::subagent::Subagent;
use crate::ui::tui::realm::StreamEvent;
use crate::ui::tui::widgets::tool_display::ToolCallResult;
use aisdk::core::LanguageModelStreamChunkType;
use futures::StreamExt;
use p1e_srt::SandboxConfig;
use std::sync::Arc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

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

pub fn spawn_stream(
    ctx: StreamContext,
    query: String,
    event_tx: UnboundedSender<StreamEvent>,
    abort_rx: UnboundedReceiver<()>,
) {
    let query = Instructions::new(query);
    tokio::spawn(run_stream(ctx, query, event_tx, abort_rx));
}

async fn run_stream(
    ctx: StreamContext,
    query: Instructions,
    event_tx: UnboundedSender<StreamEvent>,
    mut abort_rx: UnboundedReceiver<()>,
) {
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
    let _ = session.add_user(query.raw());

    let agents = get_all_agents();
    let mut req = if let Some(agent) = find_subsume_candidate(&query, &agents) {
        let skills = get_all_skills();
        let subagent = Subagent::new(ctx.model.clone(), skills, agents, ctx.sandbox.clone());
        tracing::info!(%agent, "subsuming subagent role in TUI");
        match subagent.build_request(&agent, query.raw(), 0, None) {
            Ok(r) => r,
            Err(e) => {
                let _ = event_tx.send(StreamEvent::Error(format!(
                    "Subagent subsumption failed: {e}"
                )));
                return;
            }
        }
    } else {
        build_request(&ctx.model, &query, &history, ctx.sandbox, ctx.max_steps)
    };

    let stream_result = req.stream_text().await;
    let mut response = match stream_result {
        Ok(r) => r,
        Err(e) => {
            let _ = event_tx.send(StreamEvent::Error(e.to_string()));
            return;
        }
    };

    let mut accumulated = String::new();
    let mut pending_tool = PendingToolCall::default();

    loop {
        tokio::select! {
            chunk = response.stream.next() => {
                match chunk {
                    Some(LanguageModelStreamChunkType::TextDelta(delta)) => {
                        let cleaned = strip_control_tokens(&delta);
                        if !cleaned.is_empty() {
                            accumulated.push_str(&cleaned);
                            let _ = event_tx.send(StreamEvent::Delta(accumulated.clone()));
                        }
                    }
                    Some(LanguageModelStreamChunkType::ToolCallStart(details)) => {
                        pending_tool.name.clone_from(&details.name);
                    }
                    Some(LanguageModelStreamChunkType::ToolCallAvailable(info)) => {
                        pending_tool.params = format_tool_params(&info.input);
                    }
                    Some(LanguageModelStreamChunkType::ToolCallEnd(result)) => {
                        let output = tool_output_text(&result);
                        let event = CompletedToolCall {
                            name: std::mem::take(&mut pending_tool.name),
                            params: std::mem::take(&mut pending_tool.params),
                            output,
                        };
                        let display = if event.params.is_empty() {
                            event.name.clone()
                        } else {
                            format!("{}: {}", event.name, event.params)
                        };
                        persist_tool_call(&mut session, &event.name, &display, &event.output);
                        let _ = event_tx.send(event.into());
                    }
                    Some(LanguageModelStreamChunkType::Failed(err)) => {
                        let _ = event_tx.send(StreamEvent::Error(err.clone()));
                        break;
                    }
                    None => break,
                    Some(other) => {
                        tracing::trace!(?other, "stream chunk skipped");
                    }
                }
            }
            _ = abort_rx.recv() => {
                break;
            }
        }
    }

    let tool_results = response.tool_results().await;
    tracing::debug!(len = accumulated.len(), ?tool_results, "stream finished");
    let output = extract_output_text(&accumulated, tool_results.as_deref());
    let output = strip_control_tokens(&output);

    if !output.is_empty() {
        let _ = session.add_assistant(&output);
    }

    let _ = event_tx.send(StreamEvent::Done(output));
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
    let Some(obj) = input.as_object() else {
        return String::new();
    };
    let parts: Vec<String> = obj
        .iter()
        .map(|(k, v)| {
            let val = match v {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Array(arr) => arr
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
                    .join(", "),
                other => other.to_string(),
            };
            format!("{k}={val}")
        })
        .collect();
    parts.join(", ")
}
