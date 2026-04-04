use crate::aisdk_appleai::AppleClient;
use crate::bash::{execute, shell};
use crate::interactive::ParsedInput;
use crate::skill::{find_mentioned_skills, get_all_skills, load_skill};
use aisdk::core::language_model::LanguageModelResponseContentType;
use aisdk::core::messages::Message;
use aisdk::core::tools::{Tool, ToolCallInfo};
use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::info;

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

pub fn handle_query(args: &ParsedInput) -> Result<()> {
    tracing::debug!(query = %args.query, "query");
    let client = AppleClient::new().context("Apple Intelligence not available")?;
    let rt = tokio::runtime::Runtime::new()?;

    let shell_tool = shell();
    let tools = vec![shell_tool];
    let skills = get_all_skills();

    let (skill_name, prompt, query) = resolve_skill(args, &skills);

    tracing::debug!(skill = %skill_name, query = %query, "executing");

    let result = rt.block_on(run_agent_loop(&client, &prompt, &query, &tools, &skills))?;
    info!("{result}");
    Ok(())
}

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
        auto_skill_name(),
        auto_skill_prompt(skills),
        args.query.clone(),
    )
}

/// Build a lean prompt listing skill names and descriptions only.
/// The model delegates to a skill by outputting: use /<skill-name> to answer following query: <query>
/// Full skill content is loaded only when a skill is actually selected.
fn auto_skill_prompt(skills: &[crate::skill::Skill]) -> String {
    let skill_list = skills
        .iter()
        .map(|s| format!("- {}: {}", s.name, s.description))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "You have a shell tool. Each call executes a command and returns stdout directly.\n\n\
         Available skills:\n{skill_list}\n\n\
         If a non-bash skill matches, respond with ONLY this line:\n\
         use /<skill-name> to answer following query: <original query>\n\n\
         For bash/file operations, use the shell tool directly. Do NOT ask for clarification."
    )
}

fn auto_skill_name() -> String {
    "auto".into()
}

/// Parse a skill delegation from model text output.
/// Finds patterns like `use /<skill-name> to answer following query: <query>`
/// or `use <skill-name> to answer following query: <query>` (without /)
fn parse_skill_delegation(text: &str) -> Option<(String, String)> {
    // Try with / prefix first, then without
    if let Some(result) = parse_skill_delegation_with_marker(text, "use /") {
        return Some(result);
    }
    parse_skill_delegation_with_marker(text, "use ")
}

fn parse_skill_delegation_with_marker(text: &str, marker: &str) -> Option<(String, String)> {
    let start = text.find(marker)?;
    let after = &text[start + marker.len()..];
    // Extract skill name (alphanumeric, -, _)
    let name_end = after
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_')
        .unwrap_or(after.len());
    let skill_name = &after[..name_end];
    if skill_name.is_empty() {
        return None;
    }
    // Look for the query after the skill name
    let rest = &after[name_end..];
    let delimiter = " to answer following query: ";
    if let Some(pos) = rest.find(delimiter) {
        let query = rest[pos + delimiter.len()..]
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        if !query.is_empty() {
            return Some((skill_name.to_string(), query));
        }
    }
    None
}

#[derive(Deserialize)]
struct InlineToolCall {
    #[allow(dead_code)]
    name: String,
    arguments: serde_json::Value,
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

fn extract_inline_tool_calls(text: &str) -> Vec<InlineToolCall> {
    let mut calls = Vec::new();
    for block in text.split("```") {
        let block = block.trim();
        let json_str = block.strip_prefix("function").unwrap_or(block).trim();
        if let Ok(parsed) = serde_json::from_str::<Vec<InlineToolCall>>(json_str) {
            calls.extend(parsed);
        }
    }
    if calls.is_empty() {
        for block in text.split("```") {
            let block = block.trim();
            let json_str = block
                .strip_prefix("text")
                .or_else(|| block.strip_prefix("function"))
                .unwrap_or(block)
                .trim();
            if let Ok(obj) = serde_json::from_str::<serde_json::Value>(json_str) {
                if let Some(cmd) = obj.get("cmd").and_then(|v| v.as_str()) {
                    calls.push(InlineToolCall {
                        name: "bash".to_string(),
                        arguments: serde_json::json!({ "cmd": cmd }),
                    });
                }
            }
        }
    }
    calls
}

/// Execute a command and return formatted results text.
fn exec_to_text(cmd: &str) -> String {
    let result = execute(cmd);
    let stdout = truncate(result["stdout"].as_str().unwrap_or(""), 2000);
    let stderr = truncate(result["stderr"].as_str().unwrap_or(""), 2000);
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
                    // Check if model is delegating to a skill
                    if let Some((skill_name, query)) = parse_skill_delegation(&text) {
                        if let Ok(Some(skill_content)) = load_skill(&skill_name) {
                            tracing::debug!(skill = %skill_name, query = %query, "subagent: loading skill");
                            return run_agent_loop(client, &skill_content, &query, tools, skills)
                                .await;
                        }
                    }
                    tracing::debug!(response = %text.chars().take(80).collect::<String>(), "final response");
                    return Ok(text);
                }
                let mut results = String::new();
                for call in &inline_calls {
                    let cmd = call.arguments["cmd"]
                        .as_str()
                        .or_else(|| call.arguments["command"].as_str())
                        .unwrap_or("");
                    results.push_str(&exec_to_text(cmd));
                }
                messages.push(Message::User(
                    format!(
                        "Tool results:\n{results}\nProvide a clear summary of the results above."
                    )
                    .into(),
                ));
                continue;
            }

            tracing::debug!(tool_calls = tool_call_infos.len(), "executing tool calls");

            let mut results_text = String::new();
            for tc in &tool_call_infos {
                let cmd = tc.input["cmd"]
                    .as_str()
                    .or_else(|| tc.input["command"].as_str())
                    .unwrap_or("");
                results_text.push_str(&exec_to_text(cmd));
            }

            messages.push(Message::User(
                format!(
                "Tool results:\n{results_text}Provide a clear answer based on the results above."
            )
                .into(),
            ));
        }
        Ok("Max steps reached.".to_string())
    })
}
