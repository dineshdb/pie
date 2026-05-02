use crate::agent::find_subsume_candidate;
use crate::instructions::Instructions;
use crate::output::{JsonResponse, OutputFormat};
use crate::prompt;
use crate::providers::Model;
use crate::session::{Role, Session};
use crate::tools::subagent::Subagent;
use crate::tools::tasks::task_tools;
use crate::tools::{
    execute_skill_script_tool, load_references_tool, load_skills_tool, read_file_tool,
    replace_tool, shell, subagent_tool, write_file_tool,
};
use agentsdk::core::LanguageModel;
use agentsdk::core::utils::step_count_is;
use agentsdk::core::{AssistantMessage, LanguageModelRequest, Message, UserMessage};
use anyhow::{Context, Result};
use p1e_sandbox::SandboxConfig;
use std::collections::HashSet;
use std::iter::once;
use std::sync::{Arc, Mutex};

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
    tool_results: Option<&[agentsdk::core::ToolResultInfo]>,
) -> String {
    // If the LLM produced text, prefer it — unless the last tool call was
    // a subagent, in which case the subagent's structured output is the answer.
    if !text.is_empty() {
        let subagent_res = tool_results.and_then(|results| {
            results
                .iter()
                .rfind(|r| r.tool.name == "subagent")
                .and_then(|r| r.output.as_ref().ok()?.as_str())
        });

        if let Some(res) = subagent_res
            && !res.is_empty()
        {
            return res.to_string();
        }
        return text.to_string();
    }

    // No text — fall back to last tool result (preferring shell)
    tool_results
        .and_then(|results| {
            results
                .iter()
                .rfind(|r| r.tool.name == "shell")
                .or_else(|| results.last())?
                .output
                .as_ref()
                .ok()?
                .as_str()
        })
        .unwrap_or_default()
        .to_string()
}

/// Print response to stdout and persist to session.
fn output_response(
    output: &str,
    session: &mut Session,
    format: OutputFormat,
    model: &Model,
) -> Result<()> {
    if output.is_empty() {
        return Ok(());
    }
    if format.is_json() {
        let json_resp = JsonResponse::new(
            output.to_string(),
            Some(session.id.to_string()),
            Some(model.name()),
        );
        println!("{}", serde_json::to_string(&json_resp)?);
    } else {
        println!("{output}");
    }
    session.add_assistant(output)?;
    Ok(())
}

/// Handle a query delegated to a subagent (subsumption path).
async fn subsume_agent(
    model: &Model,
    query: &Instructions,
    session: &mut Session,
    format: OutputFormat,
    sandbox_settings: Arc<SandboxConfig>,
    registry: Arc<crate::registry::Registry>,
    agent_name: String,
) -> Result<()> {
    let task_list = session.task_state.clone();
    let subagent = Subagent::new(model.clone(), registry, sandbox_settings, task_list);

    tracing::debug!(agent = %agent_name, "subsuming subagent role");
    tracing::debug!("TOOL: subsume {}", serde_json::json!({"agent": agent_name}));

    let output = subagent
        .execute(&agent_name, &query.raw, 0, None)
        .await
        .map_err(|e| anyhow::anyhow!("Subagent subsumption failed: {e}"))?;

    let output = strip_control_tokens(&output);
    session.add_user(&query.raw)?;
    output_response(&output, session, format, model)?;
    Ok(())
}

/// Handle a direct query (non-subsumption path).
async fn handle_direct(
    model: &Model,
    query: &Instructions,
    session: &mut Session,
    format: OutputFormat,
    sandbox_settings: Arc<SandboxConfig>,
    max_steps: u32,
    registry: Arc<crate::registry::Registry>,
) -> Result<()> {
    let history = session.history_entries().to_vec();
    let task_list = session.task_state.clone();
    let session_id = session.id.to_string();

    let response = crate::utils::execute_with_retry("generate_text", {
        let model = model.clone();
        let query = query.clone();
        let sandbox = sandbox_settings;
        let task_list = task_list.clone();
        let session_id = session_id.clone();

        move || {
            let model = model.clone();
            let query = query.clone();
            let history = history.clone();
            let sandbox = sandbox.clone();
            let registry = registry.clone();
            let task_list = task_list.clone();
            let session_id = session_id.clone();

            async move {
                let (system, loaded_skills) =
                    prepare_system_prompt(&registry, &history, &query, &session_id).await?;

                let messages = build_messages(&history, &query);
                let tools = build_tools(
                    model.clone(),
                    registry.clone(),
                    sandbox,
                    &task_list,
                    &session_id,
                    loaded_skills,
                )?;

                let mut builder = LanguageModelRequest::builder()
                    .model(model)
                    .system(&system)
                    .messages(messages)
                    .stop_when(step_count_is(max_steps as usize));

                for tool in tools {
                    builder = builder.with_tool(tool);
                }

                builder
                    .build()
                    .generate_text()
                    .await
                    .map_err(|e| anyhow::anyhow!(e))
            }
        }
    })
    .await
    .map_err(|e| anyhow::anyhow!(e).context("generate_text failed"))?;

    let assistant_text = response.text().unwrap_or_default();
    let output = extract_output_text(&assistant_text, response.tool_results().as_deref());
    let output = strip_control_tokens(&output);

    session.add_user(&query.raw)?;
    output_response(&output, session, format, model)?;
    Ok(())
}

fn build_messages(history: &[crate::session::HistoryEntry], query: &Instructions) -> Vec<Message> {
    history
        .iter()
        .filter_map(|entry| match entry.role {
            Role::User => Some(Message::User(UserMessage::new(&entry.content))),
            Role::Assistant => Some(Message::Assistant(AssistantMessage::from(
                entry.content.clone(),
            ))),
            _ => None,
        })
        .chain(once(Message::User(UserMessage::new(&query.raw))))
        .collect()
}

fn build_tools(
    model: Model,
    registry: Arc<crate::registry::Registry>,
    sandbox: Arc<SandboxConfig>,
    task_list: &crate::tools::tasks::SharedTaskList,
    session_id: &str,
    loaded_skills: Arc<Mutex<HashSet<String>>>,
) -> Result<Vec<agentsdk::core::tools::Tool>> {
    let mut tools = vec![
        shell(sandbox.clone(), task_list.clone()),
        read_file_tool(),
        write_file_tool(task_list.clone()),
        replace_tool(task_list.clone()),
        load_skills_tool(registry.clone(), Some(loaded_skills)),
        load_references_tool(Arc::new(Mutex::new(HashSet::new()))),
        execute_skill_script_tool(sandbox.clone()),
        subagent_tool(model, registry, sandbox, task_list.clone()),
    ];

    for tool in task_tools(task_list).context("failed to build task tools")? {
        tools.push(tool);
    }

    Ok(crate::tools::wrap_tools_with_hooks(tools, session_id))
}

async fn prepare_system_prompt(
    registry: &crate::registry::Registry,
    history: &[crate::session::HistoryEntry],
    query: &Instructions,
    session_id: &str,
) -> Result<(String, Arc<Mutex<HashSet<String>>>)> {
    let mut query = query.clone();
    history
        .iter()
        .filter(|e| e.role == Role::User)
        .for_each(|e| query.merge_mentions(&e.content));

    let sp = prompt::SystemPrompt::new(&registry.skills, &registry.agents)
        .resolve(&query)
        .with_json(false)
        .with_mode(prompt::RunMode::Cli);

    let loaded_skills = Arc::new(Mutex::new(
        sp.loaded_skills
            .iter()
            .map(ToString::to_string)
            .collect::<HashSet<String>>(),
    ));

    let (mut system, warnings) = run_pre_prompt_hooks(session_id, sp.render()).await?;

    for warning in warnings {
        system.push_str("\n\n");
        system.push_str(&warning);
    }

    Ok((system, loaded_skills))
}

async fn run_pre_prompt_hooks(
    session_id: &str,
    system_prompt: String,
) -> Result<(String, Vec<String>)> {
    let mut system = system_prompt;
    let mut warnings = Vec::new();

    let Some(cfg) = crate::config::CONFIG.get() else {
        return Ok((system, warnings));
    };

    let ctx = crate::hook::HookContext {
        event: crate::hook::HookEvent::PrePrompt,
        cwd: std::env::current_dir()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string(),
        session_id: session_id.to_string(),
        data: crate::hook::HookContextData::Prompt {
            system: system.clone(),
        },
    };

    match cfg.hooks.run(crate::hook::HookEvent::PrePrompt, &ctx).await {
        Ok((outcomes, transformed_data)) => {
            let mut errors = Vec::new();
            for outcome in &outcomes {
                if let crate::hook::HookOutcome::Error { .. } = outcome {
                    errors.push(outcome.format());
                }
            }

            if !errors.is_empty() {
                return Err(anyhow::anyhow!(
                    "Prompt rejected by validation hooks:\n{}",
                    errors.join("\n")
                ));
            }

            if let Some(new_system) = transformed_data.get("system").and_then(|v| v.as_str()) {
                system = new_system.to_string();
            }

            for outcome in outcomes {
                if let crate::hook::HookOutcome::Warning { .. } = outcome {
                    warnings.push(outcome.format());
                }
            }
        }
        Err(e) => {
            tracing::warn!("prompt.pre infrastructure failure: {}", e);
            warnings.push(format!(
                "[Hook Error] prompt.pre infrastructure failure: {e}"
            ));
        }
    }

    Ok((system, warnings))
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
        subsume_agent(
            model,
            query,
            session,
            format,
            sandbox_settings,
            registry,
            agent_name,
        )
        .await
    } else {
        handle_direct(
            model,
            query,
            session,
            format,
            sandbox_settings,
            max_steps,
            registry,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentsdk::core::tools::ToolDetails;

    fn tool_result(tool_name: &str, output: &str) -> agentsdk::core::ToolResultInfo {
        agentsdk::core::ToolResultInfo {
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
