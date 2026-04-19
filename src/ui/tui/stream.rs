use crate::handler::{build_request, extract_output_text, strip_control_tokens};
use crate::providers::Model;
use crate::session::Session;
use crate::ui::tui::event::AppEvent;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

pub fn spawn_stream(
    query: String,
    model: Model,
    sandbox: PathBuf,
    session_id: uuid::Uuid,
    pool: Arc<crate::db::DbPool>,
    event_tx: mpsc::UnboundedSender<AppEvent>,
    abort_rx: mpsc::UnboundedReceiver<()>,
) {
    tokio::spawn(run_stream(
        query, model, sandbox, session_id, pool, event_tx, abort_rx,
    ));
}

async fn run_stream(
    query: String,
    model: Model,
    sandbox: PathBuf,
    session_id: uuid::Uuid,
    pool: Arc<crate::db::DbPool>,
    event_tx: mpsc::UnboundedSender<AppEvent>,
    mut abort_rx: mpsc::UnboundedReceiver<()>,
) {
    use aisdk::core::LanguageModelStreamChunkType;
    use futures::StreamExt;

    let history = Session::load(pool.clone(), session_id)
        .map(|s| s.history_entries().to_vec())
        .unwrap_or_default();

    let query_for_req = query.strip_prefix('/').unwrap_or(&query);
    let mut req = build_request(&model, query_for_req, &history, sandbox);

    let stream_result = req.stream_text().await;

    let mut response = match stream_result {
        Ok(r) => r,
        Err(e) => {
            let _ = event_tx.send(AppEvent::StreamError(e.to_string()));
            return;
        }
    };

    let mut accumulated = String::new();

    loop {
        tokio::select! {
            chunk = response.stream.next() => {
                match chunk {
                    Some(LanguageModelStreamChunkType::TextDelta(delta)) => {
                        let cleaned = strip_control_tokens(&delta);
                        if !cleaned.is_empty() {
                            accumulated.push_str(&cleaned);
                            let _ = event_tx.send(AppEvent::StreamDelta(accumulated.clone()));
                        }
                    }
                    Some(LanguageModelStreamChunkType::Failed(err)) => {
                        let _ = event_tx.send(AppEvent::StreamError(err.clone()));
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

    if let Ok(mut session) = Session::load(pool, session_id) {
        let _ = session.add_user(&query);
        if !output.is_empty() {
            let _ = session.add_assistant(&output);
        }
    }

    let _ = event_tx.send(AppEvent::StreamDone(output));
}
