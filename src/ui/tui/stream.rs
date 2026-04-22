use crate::handler::{build_request, extract_output_text, strip_control_tokens};
use crate::providers::Model;
use crate::session::Session;
use crate::ui::tui::realm::StreamEvent;
use crate::ui::tui::widgets::tool_display::ToolCallResult;
use p1e_srt::SandboxConfig;
use std::sync::Arc;
use tokio::sync::mpsc;

pub fn spawn_stream(
    query: String,
    model: Model,
    sandbox: Arc<SandboxConfig>,
    session_id: uuid::Uuid,
    pool: Arc<crate::db::DbPool>,
    event_tx: mpsc::UnboundedSender<StreamEvent>,
    abort_rx: mpsc::UnboundedReceiver<()>,
    max_steps: u32,
) {
    tokio::spawn(run_stream(
        query, model, sandbox, session_id, pool, event_tx, abort_rx, max_steps,
    ));
}

async fn run_stream(
    query: String,
    model: Model,
    sandbox: Arc<SandboxConfig>,
    session_id: uuid::Uuid,
    pool: Arc<crate::db::DbPool>,
    event_tx: mpsc::UnboundedSender<StreamEvent>,
    mut abort_rx: mpsc::UnboundedReceiver<()>,
    max_steps: u32,
) {
    use aisdk::core::LanguageModelStreamChunkType;
    use futures::StreamExt;

    let mut session = match Session::load(pool, session_id) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("failed to load session: {e}");
            let _ = event_tx.send(StreamEvent::Error(e.to_string()));
            return;
        }
    };

    let history = session.history_entries().to_vec();

    // Persist user message before streaming so tool calls land after it in DB order.
    let _ = session.add_user(&query);

    let query_for_req = query.strip_prefix('/').unwrap_or(&query);
    let mut req = build_request(&model, query_for_req, &history, sandbox, max_steps);

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
                        tracing::debug!(?other, "stream chunk skipped");
                    }
                }
            }
            _ = abort_rx.recv() => {
                break;
            }
        }
    }

    let tool_results = response.tool_results().await;
    tracing::debug!(
        accumulated_len = accumulated.len(),
        ?tool_results,
        "stream finished"
    );
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
