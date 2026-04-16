use crate::core::output::{JsonResponse, OutputFormat};
use crate::core::prompt;
use crate::core::session::{Role, Session};
use crate::core::skill::get_all_skills;
use crate::core::tools::{load_references_tool, load_skills_tool, shell_tool, subagent_tool};
use crate::providers::Model;
use crate::ui::markdown::MarkdownRenderer;
use aisdk::core::utils::step_count_is;
use aisdk::core::{AssistantMessage, LanguageModelRequest, Message, UserMessage};
use aisdk::core::{LanguageModel, ToolResultInfo};
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tracing::warn;

pub fn handle_list_skills() {
    let skills = get_all_skills();
    if skills.is_empty() {
        warn!("No skills found.");
        return;
    }
    println!("Available skills:");
    for s in &skills {
        println!(" - {}: {}", s.name, s.description);
    }
}

fn build_request(
    model: &Model,
    query: &str,
    session: &Session,
    sandbox_settings: PathBuf,
) -> (LanguageModelRequest<Model>, Arc<Mutex<HashSet<String>>>) {
    let skills = get_all_skills();
    let history_entries = session.history_entries().to_vec();

    let mut scan_sources: Vec<&str> = vec![query];
    for entry in &history_entries {
        if entry.role == Role::User {
            scan_sources.push(&entry.content);
        }
    }

    let format = OutputFormat::default();
    let system = prompt::system_prompt(&skills, format.to_instructions());

    let mut messages: Vec<Message> = Vec::new();
    if let Some(skills_msg) = prompt::mentioned_skills_message(&skills, &scan_sources) {
        messages.push(Message::User(UserMessage::new(skills_msg)));
    }

    for entry in &history_entries {
        match entry.role {
            Role::User => messages.push(Message::User(UserMessage::new(&entry.content))),
            Role::Assistant => messages.push(Message::Assistant(AssistantMessage::from(
                entry.content.clone(),
            ))),
            Role::System => {}
        }
    }
    messages.push(Message::User(UserMessage::new(query)));

    tracing::debug!(system = %system, query, "agent:");
    let loaded_refs = Arc::new(Mutex::new(HashSet::new()));
    let req = LanguageModelRequest::builder()
        .model(model.clone())
        .system(&system)
        .messages(messages)
        .with_tool(shell_tool(sandbox_settings.clone()))
        .with_tool(load_skills_tool(skills.clone()))
        .with_tool(load_references_tool(loaded_refs.clone()))
        .with_tool(subagent_tool(model.clone(), skills, sandbox_settings))
        .stop_when(step_count_is(25))
        .build();

    (req, loaded_refs)
}

/// Extract the output text from a response (handles both text and tool results).
fn extract_output_text(text: &str, tool_results: &Option<Vec<ToolResultInfo>>) -> String {
    if !text.is_empty() {
        return text.to_string();
    }
    if let Some(results) = tool_results {
        results
            .iter()
            .rfind(|r| r.tool.name == "shell_tool")
            .or_else(|| results.last())
            .and_then(|r| r.output.as_ref().ok())
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    } else {
        String::new()
    }
}

pub async fn handle_query(
    model: &mut Model,
    query: &str,
    session: &mut Session,
    format: OutputFormat,
    sandbox_settings: PathBuf,
) -> Result<()> {
    let (mut req, _loaded_refs) = build_request(model, query, session, sandbox_settings);

    let response = req.generate_text().await.context("generate_text failed")?;
    let assistant_text = response.text().unwrap_or_default();
    let output = extract_output_text(&assistant_text, &response.tool_results());

    if !output.is_empty() {
        if format.is_json() {
            let json_resp = JsonResponse::new(
                output.clone(),
                Some(session.id.to_string()),
                Some(model.name()),
            );
            println!("{}", serde_json::to_string(&json_resp)?);
        } else {
            println!("{output}");
        }
    }

    session.add_user(query)?;
    if !output.is_empty() {
        session.add_assistant(&output)?;
    }

    Ok(())
}

pub async fn handle_query_streaming(
    model: &mut Model,
    query: &str,
    session: &mut Session,
    sandbox_settings: PathBuf,
) -> Result<()> {
    use aisdk::core::LanguageModelStreamChunkType;
    use futures::StreamExt;

    let (mut req, _loaded_refs) = build_request(model, query, session, sandbox_settings);

    let mut response = req.stream_text().await.context("stream_text failed")?;
    let mut renderer = MarkdownRenderer::new();

    while let Some(chunk) = response.stream.next().await {
        match chunk {
            LanguageModelStreamChunkType::TextDelta(delta) => {
                // Strip provider control tokens that leak as text
                let cleaned = delta
                    .replace("<eos>", "")
                    .replace("<|end|>", "")
                    .replace("<|endoftext|>", "")
                    .replace("<|end_of_turn|>", "");
                renderer.push_delta(&cleaned);
            }
            LanguageModelStreamChunkType::Failed(err) => {
                tracing::error!("Stream failed: {err}");
                break;
            }
            _ => {}
        }
    }

    let accumulated = renderer.finish();

    // Try to get the final text from the response for session persistence
    let final_text = response.text().await.unwrap_or_else(|| accumulated.clone());
    let tool_results = response.tool_results().await;
    let output = extract_output_text(&final_text, &tool_results);

    session.add_user(query)?;
    if !output.is_empty() {
        session.add_assistant(&output)?;
    }

    Ok(())
}
