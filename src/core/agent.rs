use crate::core::output::{JsonResponse, OutputFormat};
use crate::core::prompt;
use crate::core::session::{Role, Session};
use crate::core::skill::get_all_skills;
use crate::core::tools::{load_skills_tool, shell_tool, subagent_tool};
use crate::providers::Model;
use aisdk::core::LanguageModel;
use aisdk::core::utils::step_count_is;
use aisdk::core::{AssistantMessage, LanguageModelRequest, Message, UserMessage};
use anyhow::{Context, Result};
use std::path::PathBuf;
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

pub async fn handle_query(
    model: &mut Model,
    query: &str,
    session: &mut Session,
    format: OutputFormat,
    sandbox_settings: PathBuf,
) -> Result<()> {
    let skills = get_all_skills();
    let (system_skills, user_skills): (Vec<_>, Vec<_>) =
        skills.iter().cloned().partition(|s| s.is_embedded);

    let history_entries = session.history_entries().to_vec();
    let mut scan_sources: Vec<&str> = vec![query];
    for entry in &history_entries {
        if entry.role == Role::User {
            scan_sources.push(&entry.content);
        }
    }

    // Single system prompt rendered from template with all context
    let system = prompt::system_prompt(&system_skills, &user_skills, format.to_instructions());

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
    let mut req = {
        LanguageModelRequest::builder()
            .model(model.clone())
            .system(&system)
            .messages(messages)
            .with_tool(shell_tool(sandbox_settings.clone()))
            .with_tool(load_skills_tool(skills.clone()))
            .with_tool(subagent_tool(model.clone(), skills, sandbox_settings))
            .stop_when(step_count_is(10))
            .build()
    };

    let response = req.generate_text().await.context("generate_text failed")?;
    let assistant_text = response.text().unwrap_or_default();

    let output = if !assistant_text.is_empty() {
        assistant_text
    } else if let Some(results) = response.tool_results() {
        // Find shell_tool results first, then fall back to last result
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
    };

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
