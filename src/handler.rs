use crate::agent::find_subsume_candidate;
use crate::instructions::Instructions;
use crate::output::{JsonResponse, OutputFormat};
use crate::prompt;
use crate::providers::Model;
use crate::session::{Role, Session};
use crate::tools::subagent::Subagent;
use crate::tools::tasks::{SharedTaskList, TaskList, task_tools};
use crate::tools::{
    execute_skill_script_tool, load_references_tool, load_skills_tool, read_file_tool,
    replace_tool, shell, subagent_tool, write_file_tool,
};
use aisdk::core::LanguageModel;
use aisdk::core::utils::step_count_is;
use aisdk::core::{AssistantMessage, LanguageModelRequest, Message, UserMessage};
use anyhow::{Context, Result};
use p1e_srt::SandboxConfig;
use std::collections::HashSet;
use std::iter::once;
use std::sync::{Arc, Mutex};

pub fn build_request(
    model: &Model,
    query: &Instructions,
    history: &[crate::session::HistoryEntry],
    sandbox_settings: Arc<SandboxConfig>,
    max_steps: u32,
    registry: &Arc<crate::registry::Registry>,
    task_state: &SharedTaskList,
) -> Result<LanguageModelRequest<Model>> {
    let skills = &registry.skills;
    let agents = &registry.agents;

    // all mentioned skills from current and past user queries
    let mut query = query.clone();
    history
        .iter()
        .filter(|e| e.role == Role::User)
        .for_each(|e| query.merge_mentions(&e.content));

    let format = OutputFormat::Default;
    let sp = prompt::SystemPrompt::new(skills, agents)
        .resolve(&query)
        .with_json(format.is_json());

    let needed_skills = &sp.loaded_skills;
    let loaded_skills = Arc::new(Mutex::new(
        needed_skills
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
        .chain(once(Message::User(UserMessage::new(&query.raw))))
        .collect();

    tracing::debug!(system = %system, query = %query.raw, "agent:");
    let loaded_refs = Arc::new(Mutex::new(HashSet::new()));
    let mut builder = LanguageModelRequest::builder()
        .model(model.clone())
        .system(&system)
        .messages(messages)
        .with_tool(shell(sandbox_settings.clone()))
        .with_tool(read_file_tool())
        .with_tool(write_file_tool())
        .with_tool(replace_tool())
        .with_tool(load_skills_tool(
            registry.clone(),
            Some(loaded_skills.clone()),
        ))
        .with_tool(load_references_tool(loaded_refs))
        .with_tool(execute_skill_script_tool(sandbox_settings.clone()))
        .with_tool(subagent_tool(
            model.clone(),
            registry.clone(),
            sandbox_settings,
            task_state.clone(),
        ))
        .stop_when(step_count_is(max_steps as usize));

    for tool in task_tools(task_state).context("failed to build task tools")? {
        builder = builder.with_tool(tool);
    }

    Ok(builder.build())
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
                .rfind(|r| r.tool.name == "shell")
                .or_else(|| results.last())
                .and_then(|r| r.output.as_ref().ok())
                .and_then(|v| v.as_str())
        })
        .unwrap_or_default()
        .to_string()
}

/// CLI-mode only: format tool call result for stderr display.
fn format_tool_call_for_cli(result: &aisdk::core::ToolResultInfo) -> String {
    let name = &result.tool.name;
    let output = result
        .output
        .as_ref()
        .ok()
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if output.is_empty() {
        format!("Tool: {name}")
    } else {
        let first_line = output.lines().next().unwrap_or("");
        let truncated = if first_line.len() > 100 {
            format!("{}...", &first_line[..100])
        } else {
            first_line.to_string()
        };
        format!("Tool: {name} -> {truncated}")
    }
}

pub async fn handle_query(
    model: &mut Model,
    query: &Instructions,
    session: &mut Session,
    format: OutputFormat,
    sandbox_settings: Arc<SandboxConfig>,
    max_steps: u32,
    registry: Arc<crate::registry::Registry>,
) -> Result<()> {
    if let Some(agent_name) = find_subsume_candidate(query, &registry.agents) {
        let task_list = SharedTaskList::default();
        let subagent = Subagent::new(
            model.clone(),
            registry.clone(),
            sandbox_settings.clone(),
            task_list,
        );

        tracing::info!(agent = %agent_name, "subsuming subagent role");
        let result = subagent.execute(&agent_name, &query.raw, 0, None).await;
        match result {
            Ok(output) => {
                let output = strip_control_tokens(&output);
                session.add_user(&query.raw)?;
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
                    session.add_assistant(&output)?;
                }
                return Ok(());
            }
            Err(e) => {
                anyhow::bail!("Subagent subsumption failed: {e}");
            }
        }
    }

    let model_clone = model.clone();
    let query_clone = query.clone();
    let history_clone = session.history_entries().to_vec();
    let sandbox_clone = sandbox_settings.clone();
    let registry_clone = registry.clone();

    let task_list: SharedTaskList = Arc::new(Mutex::new(TaskList::default()));
    let task_list_clone = task_list.clone();

    let response = crate::utils::execute_with_retry("generate_text", move || {
        let model = model_clone.clone();
        let query = query_clone.clone();
        let history = history_clone.clone();
        let sandbox = sandbox_clone.clone();
        let registry = registry_clone.clone();
        let task_list = task_list_clone.clone();
        async move {
            let mut req = build_request(
                &model, &query, &history, sandbox, max_steps, &registry, &task_list,
            )?;
            req.generate_text().await.map_err(|e| anyhow::anyhow!(e))
        }
    })
    .await
    .map_err(|e| anyhow::anyhow!(e).context("generate_text failed"))?;

    for result in response.tool_results().iter().flatten() {
        if result.tool.name == "task_add" || result.tool.name == "task_update" {
            let guard = crate::tools::safe_lock(&task_list);
            eprintln!("Progress: {}", guard.progress_summary());
        } else {
            let tool = aisdk::core::ToolResultInfo {
                tool: result.tool.clone(),
                output: result.output.clone(),
            };
            let display = format_tool_call_for_cli(&tool);
            eprintln!("{display}");
        }
    }

    let assistant_text = response.text().unwrap_or_default();
    let output = extract_output_text(&assistant_text, response.tool_results().as_deref());
    let output = strip_control_tokens(&output);

    session.add_user(&query.raw)?;

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
        let tool_results = vec![tool_result("shell", "tool output")];
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
    fn extract_output_falls_back_to_shell_result_when_no_text() {
        let tool_results = vec![
            tool_result("other_tool", "other"),
            tool_result("shell", "shell output"),
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
