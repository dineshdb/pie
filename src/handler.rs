use crate::agent::get_all_agents;
use crate::output::{JsonResponse, OutputFormat};
use crate::prompt;
use crate::providers::Model;
use crate::session::{Role, Session};
use crate::skill::get_all_skills;
use crate::tools::{load_references_tool, load_skills_tool, shell_tool, subagent_tool};
use aisdk::core::LanguageModel;
use aisdk::core::utils::step_count_is;
use aisdk::core::{AssistantMessage, LanguageModelRequest, Message, UserMessage};
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub fn build_request(
    model: &Model,
    query: &str,
    history: &[crate::session::HistoryEntry],
    sandbox_settings: PathBuf,
) -> LanguageModelRequest<Model> {
    let skills = get_all_skills();

    let mut scan_sources: Vec<&str> = vec![query];
    for entry in history {
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
    for entry in history {
        let msg = match entry.role {
            Role::User => Message::User(UserMessage::new(&entry.content)),
            Role::Assistant => Message::Assistant(AssistantMessage::from(entry.content.clone())),
            Role::System | Role::Tool => continue,
        };
        messages.push(msg);
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

const CONTROL_TOKENS: &[&str] = &["<eos>", "<|end|>", "</think_end>", "<|end_of_turn|>"];

/// Strip provider control tokens that leak as text.
pub fn strip_control_tokens(text: &str) -> String {
    if !text.contains('<') {
        return text.to_string();
    }
    CONTROL_TOKENS
        .iter()
        .fold(text.to_string(), |acc, tok| acc.replace(tok, ""))
}

/// Extract the output text from a response (handles both text and tool results).
pub fn extract_output_text(
    text: &str,
    tool_results: Option<&[aisdk::core::ToolResultInfo]>,
) -> String {
    // If the LLM produced text, prefer it — unless the last tool call was
    // a subagent, in which case the subagent's structured output is the answer.
    if !text.is_empty() {
        if let Some(subagent_result) = tool_results.and_then(|results| {
            results
                .iter()
                .rfind(|r| r.tool.name == "subagent")
                .and_then(|r| r.output.as_ref().ok())
                .and_then(|v| v.as_str())
        }) && !subagent_result.is_empty()
        {
            return subagent_result.to_string();
        }
        return text.to_string();
    }
    // No text — fall back to last tool result
    tool_results
        .and_then(|results| {
            results
                .iter()
                .rfind(|r| r.tool.name == "shell_tool")
                .or_else(|| results.last())
                .and_then(|r| r.output.as_ref().ok())
                .and_then(|v| v.as_str())
        })
        .unwrap_or_default()
        .to_string()
}

pub async fn handle_query(
    model: &mut Model,
    query: &str,
    session: &mut Session,
    format: OutputFormat,
    sandbox_settings: PathBuf,
) -> Result<()> {
    let mut req = build_request(model, query, session.history_entries(), sandbox_settings);

    let response = req.generate_text().await.context("generate_text failed")?;
    let assistant_text = response.text().unwrap_or_default();
    let output = extract_output_text(&assistant_text, response.tool_results().as_deref());
    let output = strip_control_tokens(&output);

    session.add_user(query)?;

    if output.is_empty() {
        return Ok(());
    }

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
    session.add_assistant(&output)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aisdk::core::tools::ToolDetails;

    fn tool_result(tool_name: &str, output: &str) -> aisdk::core::ToolResultInfo {
        aisdk::core::ToolResultInfo {
            tool: ToolDetails {
                name: tool_name.to_string(),
                ..Default::default()
            },
            output: Ok(serde_json::json!(output)),
        }
    }

    #[test]
    fn strip_control_tokens_removes_all_variants() {
        let input = "a<eos>b<|end|>c</think_end>d<|end_of_turn|>e";
        assert_eq!(strip_control_tokens(input), "abcde");
    }

    #[test]
    fn extract_output_prefers_text_even_with_tool_results() {
        let tool_results = vec![tool_result("shell_tool", "tool output")];
        let result = extract_output_text("the answer", Some(&tool_results));
        assert_eq!(result, "the answer");
    }

    #[test]
    fn extract_output_prefers_subagent_result_over_text() {
        let tool_results = vec![tool_result("subagent", "subagent answer")];
        let result = extract_output_text("llm text", Some(&tool_results));
        assert_eq!(result, "subagent answer");
    }

    #[test]
    fn extract_output_falls_back_to_shell_tool_result_when_no_text() {
        let tool_results = vec![
            tool_result("other_tool", "other"),
            tool_result("shell_tool", "shell output"),
        ];
        let result = extract_output_text("", Some(&tool_results));
        assert_eq!(result, "shell output");
    }

    #[test]
    fn extract_output_falls_back_to_last_tool_result_when_no_shell() {
        let tool_results = vec![tool_result("other_tool", "last result")];
        let result = extract_output_text("", Some(&tool_results));
        assert_eq!(result, "last result");
    }

    #[test]
    fn extract_output_returns_empty_when_nothing_available() {
        assert!(extract_output_text("", None).is_empty());
    }
}
