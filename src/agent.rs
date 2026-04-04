use crate::afm::AppleClient;
use crate::skill::{find_mentioned_skills, get_all_skills, load_skill};
use aisdk::core::language_model::LanguageModelResponseContentType;
use aisdk::core::messages::Message;
use aisdk::core::tools::{Tool, ToolCallInfo};
use aisdk::macros::tool;
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;
use std::process::Command;
use tracing::info;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Parsed user input with optional skill selection.
pub struct ParsedInput {
    pub skill: Option<String>,
    pub query: String,
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const AUTO_SKILL: &str = "auto";
const MAX_TOOL_OUTPUT: usize = 2000;

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

pub fn handle_list_skills() {
    let skills = get_all_skills();
    if skills.is_empty() {
        println!("No skills found.");
        return;
    }
    println!("Available skills:");
    for s in &skills {
        println!(" - {}: {}", s.name, s.description);
    }
}

pub async fn handle_query(args: &ParsedInput) -> Result<()> {
    tracing::debug!(query = %args.query, "query");
    let client = AppleClient::new().context("Apple Intelligence not available")?;

    let tools = vec![shell_tool()];
    let skills = get_all_skills();

    let (skill_name, prompt, query) = resolve_skill(args, &skills);

    tracing::debug!(skill = %skill_name, query = %query, "executing");

    // Auto mode must NOT have the shell tool — Apple Intelligence is too small
    // to follow routing instructions when a tool is available; it calls the tool
    // directly instead of outputting /<skill-name> <query> text.
    // Subagents get the shell tool via the delegation path in run_agent_loop.
    let tools = if skill_name == AUTO_SKILL { vec![] } else { tools };

    let result = run_agent_loop(&client, &prompt, &query, &tools, &skills).await?;
    info!("{result}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Skill resolution
// ---------------------------------------------------------------------------

fn resolve_skill(args: &ParsedInput, skills: &[crate::skill::Skill]) -> (String, String, String) {
    // 1. Explicit --skill flag
    if let Some(name) = &args.skill {
        if let Ok(Some(prompt)) = load_skill(name) {
            let query = if args.query.is_empty() {
                name.clone()
            } else {
                args.query.clone()
            };
            return (name.clone(), prompt, query);
        }
    }

    // 2. /skillname mentioned in query
    let mentioned = find_mentioned_skills(&args.query, skills);
    if mentioned.len() == 1 {
        let name = &mentioned[0];
        if let Ok(Some(prompt)) = load_skill(name) {
            let query = args
                .query
                .replace(&format!("/{name}"), "")
                .trim()
                .to_string();
            let query = if query.is_empty() {
                name.clone()
            } else {
                query
            };
            return (name.clone(), prompt, query);
        }
    }

    // 3. LLM decides: embed all skill instructions and let the model choose
    (
        AUTO_SKILL.to_string(),
        auto_skill_prompt(skills),
        args.query.clone(),
    )
}

fn auto_skill_prompt(skills: &[crate::skill::Skill]) -> String {
    let skill_list = skills
        .iter()
        .map(|s| format!("- {}: {}", s.name, s.description))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "Route the query to exactly one skill. Respond with ONLY one line.\n\n\
         Available skills:\n{skill_list}\n\n\
         Rules:\n\
         - Facts, news, people, current events → /web-search <query>\n\
         - Shell commands, file ops, system tasks → /bash <query>\n\
         - Another skill matches → /<skill-name> <query>\n\
         - No skill fits → answer directly"
    )
}

fn parse_skill_delegation(text: &str) -> Option<(String, String)> {
    let text = text.trim();
    if !text.starts_with('/') {
        return None;
    }
    let after = &text[1..];
    let name_end = after
        .find(|c: char| c.is_whitespace())
        .unwrap_or(after.len());
    let skill_name = &after[..name_end];
    if skill_name.is_empty() {
        return None;
    }
    let query = after[name_end..].trim().to_string();
    Some((skill_name.to_string(), query))
}

// ---------------------------------------------------------------------------
// Shell tool (was bash.rs)
// ---------------------------------------------------------------------------

/// Create the shell tool definition for the agent.
#[tool]
fn shell_tool(cmd: String) -> Tool {
    let result = execute(&cmd);
    Ok(serde_json::to_string(&result).unwrap_or_default())
}

/// Execute a bash command and return structured JSON result.
fn execute(cmd: &str) -> serde_json::Value {
    let output = Command::new("sh").arg("-c").arg(cmd).output();

    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let exit_code = out.status.code().unwrap_or(-1);
            json!({
                "cmd": cmd,
                "exitCode": exit_code,
                "stdout": stdout,
                "stderr": stderr,
                "success": exit_code == 0
            })
        }
        Err(e) => json!({
            "cmd": cmd,
            "exitCode": -1,
            "stdout": "",
            "stderr": e.to_string(),
            "success": false
        }),
    }
}

// ---------------------------------------------------------------------------
// Tool call helpers
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct InlineToolCall {
    #[allow(dead_code)]
    name: String,
    arguments: serde_json::Value,
}

fn extract_inline_tool_calls(text: &str) -> Vec<InlineToolCall> {
    let mut calls = Vec::new();
    for block in text.split("```") {
        let block = block.trim();
        let json_str = block
            .strip_prefix("function")
            .or_else(|| block.strip_prefix("text"))
            .unwrap_or(block)
            .trim();
        if let Ok(parsed) = serde_json::from_str::<Vec<InlineToolCall>>(json_str) {
            calls.extend(parsed);
        } else if let Ok(obj) = serde_json::from_str::<serde_json::Value>(json_str) {
            if let Some(cmd) = obj.get("cmd").and_then(|v| v.as_str()) {
                calls.push(InlineToolCall {
                    name: "bash".to_string(),
                    arguments: serde_json::json!({ "cmd": cmd }),
                });
            }
        }
    }
    calls
}

/// Truncate to `max_len` bytes on a char boundary.
fn truncate(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        return s;
    }
    let mut end = max_len;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn exec_to_text(cmd: &str) -> String {
    let result = execute(cmd);
    let stdout = truncate(result["stdout"].as_str().unwrap_or(""), MAX_TOOL_OUTPUT);
    let stderr = truncate(result["stderr"].as_str().unwrap_or(""), MAX_TOOL_OUTPUT);
    let exit_code = result["exitCode"].as_i64().unwrap_or(-1);

    tracing::debug!(cmd, exit_code, "shell-tool");

    let mut out = format!("$ {cmd}\n");
    if exit_code != 0 {
        out.push_str(&format!("exit code: {exit_code}\n"));
    }
    if !stdout.is_empty() {
        out.push_str(&format!("{stdout}\n"));
    }
    if !stderr.is_empty() {
        out.push_str(&format!("stderr: {stderr}\n"));
    }
    out
}

fn extract_cmd(args: &serde_json::Value) -> &str {
    args["cmd"]
        .as_str()
        .or_else(|| args["command"].as_str())
        .unwrap_or("")
}

fn execute_tool_calls(calls: &[&serde_json::Value]) -> String {
    let mut results = String::new();
    for args in calls {
        results.push_str(&exec_to_text(extract_cmd(args)));
    }
    results
}

// ---------------------------------------------------------------------------
// Agent loop
// ---------------------------------------------------------------------------

#[allow(clippy::only_used_in_recursion)]
fn run_agent_loop<'a>(
    client: &'a AppleClient,
    instructions: &'a str,
    user_query: &'a str,
    tools: &'a [Tool],
    skills: &'a [crate::skill::Skill],
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>> {
    Box::pin(async move {
        let system_prompt =
            "You are a helpful assistant. Follow the instructions and use tools to answer.";
        let user_msg = format!("Instructions:\n{instructions}\n\nQuestion: {user_query}");

        let mut messages: Vec<Message> = vec![Message::User(user_msg.into())];

        for step in 0..50 {
            tracing::debug!(step, messages = messages.len(), "calling model");
            let response = client.generate(system_prompt, &messages, tools).await?;

            let mut text_parts = Vec::new();
            let mut tool_call_infos: Vec<ToolCallInfo> = Vec::new();

            for content in &response.contents {
                match content {
                    LanguageModelResponseContentType::Text(text) => text_parts.push(text.clone()),
                    LanguageModelResponseContentType::ToolCall(info) => {
                        tool_call_infos.push(info.clone())
                    }
                    _ => {}
                }
            }

            let text = text_parts.join("\n");

            if tool_call_infos.is_empty() {
                let inline_calls = extract_inline_tool_calls(&text);
                if inline_calls.is_empty() {
                    if let Some((skill_name, query)) = parse_skill_delegation(&text) {
                        if let Ok(Some(skill_content)) = load_skill(&skill_name) {
                            let query = if query.is_empty() {
                                user_query.to_string()
                            } else {
                                query
                            };
                            tracing::debug!(skill = %skill_name, query = %query, "subagent");
                            let subagent_tools = vec![shell_tool()];
                            return run_agent_loop(
                                client,
                                &skill_content,
                                &query,
                                &subagent_tools,
                                skills,
                            )
                            .await;
                        }
                    }
                    tracing::debug!(response = %text.chars().take(80).collect::<String>(), "final response");
                    return Ok(text);
                }
                let args: Vec<&serde_json::Value> =
                    inline_calls.iter().map(|c| &c.arguments).collect();
                let results = execute_tool_calls(&args);
                messages.push(Message::User(
                    format!(
                        "Tool results:\n{results}\nProvide a clear summary of the results above."
                    )
                    .into(),
                ));
                continue;
            }

            tracing::debug!(tool_calls = tool_call_infos.len(), "executing tool calls");

            let args: Vec<&serde_json::Value> =
                tool_call_infos.iter().map(|tc| &tc.input).collect();
            let results_text = execute_tool_calls(&args);

            messages.push(Message::User(
                format!(
                    "Tool results:\n{results_text}\nProvide a clear answer based on the results above."
                )
                .into(),
            ));
        }
        Ok("Max steps reached.".to_string())
    })
}
