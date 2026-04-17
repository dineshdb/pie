use crate::agent::get_all_agents;
use crate::output::{JsonResponse, OutputFormat};
use crate::prompt;
use crate::providers::Model;
use crate::session::{Role, Session};
use crate::skill::get_all_skills;
use crate::tools::{load_references_tool, load_skills_tool, shell_tool, subagent_tool};
use crate::ui::markdown::MarkdownRenderer;
use aisdk::core::LanguageModel;
use aisdk::core::utils::step_count_is;
use aisdk::core::{AssistantMessage, LanguageModelRequest, Message, UserMessage};
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

fn build_request(
    model: &Model,
    query: &str,
    session: &Session,
    sandbox_settings: PathBuf,
) -> LanguageModelRequest<Model> {
    let skills = get_all_skills();
    let history_entries = session.history_entries().to_vec();

    let mut scan_sources: Vec<&str> = vec![query];
    for entry in &history_entries {
        if entry.role == Role::User {
            scan_sources.push(&entry.content);
        }
    }

    let format = OutputFormat::default();
    let agents = get_all_agents();

    // Resolve pre-loaded skills from query/history mentions + agent auto-loads
    let preloaded = prompt::resolve_preloaded_skills(&skills, &agents, &scan_sources);
    let preloaded_names: HashSet<String> = preloaded.iter().map(|s| s.name.clone()).collect();
    let loaded_skills = Arc::new(Mutex::new(preloaded_names));
    let system = prompt::system_prompt_with_loaded(&skills, &agents, format.is_json(), &preloaded);

    let mut messages: Vec<Message> = Vec::new();
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
    LanguageModelRequest::builder()
        .model(model.clone())
        .system(&system)
        .messages(messages)
        .with_tool(shell_tool(sandbox_settings.clone()))
        .with_tool(load_skills_tool(
            skills.clone(),
            Some(loaded_skills.clone()),
        ))
        .with_tool(load_references_tool(loaded_refs))
        .with_tool(subagent_tool(
            model.clone(),
            skills,
            agents,
            sandbox_settings,
        ))
        .stop_when(step_count_is(25))
        .build()
}

/// Strip provider control tokens that leak as text.
fn strip_control_tokens(text: &str) -> String {
    if !text.contains('<') {
        return text.to_string();
    }
    text.replace("<eos>", "")
        .replace("<|end|>", "")
        .replace("</think_end>", "")
        .replace("<|end_of_turn|>", "")
}

/// Extract the output text from a response (handles both text and tool results).
fn extract_output_text(
    text: &str,
    tool_results: &Option<Vec<aisdk::core::ToolResultInfo>>,
) -> String {
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
    let mut req = build_request(model, query, session, sandbox_settings);

    let response = req.generate_text().await.context("generate_text failed")?;
    let assistant_text = response.text().unwrap_or_default();
    let output = extract_output_text(&assistant_text, &response.tool_results());
    let output = strip_control_tokens(&output);

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

    let mut req = build_request(model, query, session, sandbox_settings);

    let mut response = req.stream_text().await.context("stream_text failed")?;
    let mut renderer = MarkdownRenderer::new();

    while let Some(chunk) = response.stream.next().await {
        match chunk {
            LanguageModelStreamChunkType::TextDelta(delta) => {
                let cleaned = strip_control_tokens(&delta);
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

    let tool_results = response.tool_results().await;
    let output = extract_output_text(&accumulated, &tool_results);
    let output = strip_control_tokens(&output);

    session.add_user(query)?;
    if !output.is_empty() {
        session.add_assistant(&output)?;
    }

    Ok(())
}
