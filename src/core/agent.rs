use crate::core::output::{JsonResponse, OutputFormat};
use crate::core::prompt;
use crate::core::session::{Role, Session};
use crate::core::skill::get_all_skills;
use crate::core::tools::{load_skills_tool, shell_tool, subagent_tool};
use crate::providers::Model;
use aisdk::core::LanguageModel;
use aisdk::core::utils::step_count_is;
use aisdk::core::{AssistantMessage, LanguageModelRequest, Message, SystemMessage, UserMessage};
use anyhow::{Context, Result};
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

    // Layer 1: Core system prompt — compiled-in, immutable, most cacheable
    let core_sys = prompt::system_core(&system_skills);

    // Layer 2: Global config — user skills + global agents, cacheable across projects
    // Layer 3: Project context — per-project, cacheable within session
    let global_sys = prompt::global_config(&user_skills);
    let project_sys = prompt::project_context();

    // Build messages: global config → project context → skills → role → history → query
    let mut messages: Vec<Message> = Vec::new();
    messages.push(Message::System(SystemMessage::new(&global_sys)));
    messages.push(Message::System(SystemMessage::new(&project_sys)));
    if let Some(skills_msg) = prompt::mentioned_skills_message(&skills, &scan_sources) {
        messages.push(Message::User(UserMessage::new(skills_msg)));
    }
    messages.push(Message::System(SystemMessage::new(prompt::router_role())));
    let format_instructions = format.to_instructions();
    if !format_instructions.is_empty() {
        messages.push(Message::System(SystemMessage::new(format_instructions)));
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

    tracing::debug!(prompt = core_sys, query, "agent:");
    tracing::debug!(role = prompt::router_role(), "agent role");
    let mut req = {
        LanguageModelRequest::builder()
            .model(model.clone())
            .system(core_sys)
            .messages(messages)
            .with_tool(shell_tool())
            .with_tool(load_skills_tool(skills.clone()))
            .with_tool(subagent_tool(model.clone(), skills))
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
            .filter(|r| r.tool.name == "shell_tool")
            .last()
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
