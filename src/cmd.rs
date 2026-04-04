use crate::aisdk_appleai::AppleClient;
use crate::bash::{bash, execute_bash};
use crate::interactive::ParsedInput;
use crate::router::{build_orchestrator_prompt, find_mentioned_skills};
use crate::skill::{get_all_skills, load_skill};
use aisdk::core::language_model::LanguageModelResponseContentType;
use aisdk::core::messages::{AssistantMessage, Message};
use aisdk::core::tools::ToolCallInfo;
use anyhow::{Context, Result};
use serde::Deserialize;

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
    tracing::debug!(query = %args.query, "handling query");
    let client = AppleClient::new().context("Apple Intelligence not available")?;
    let rt = tokio::runtime::Runtime::new()?;
    let skills = get_all_skills();

    let (system_prompt, user_query) = resolve_prompt(args, &skills)?;

    // Truncate system prompt to fit Apple's on-device context window (~4K tokens)
    let max_prompt_chars = 3000;
    let system_prompt = if system_prompt.len() > max_prompt_chars {
        tracing::warn!(
            original = system_prompt.len(),
            truncated = max_prompt_chars,
            "system prompt truncated"
        );
        system_prompt[..max_prompt_chars].to_string()
    } else {
        system_prompt
    };

    let bash_tool = bash();
    let result = rt.block_on(run_agent_loop(
        &client,
        &system_prompt,
        &user_query,
        &[bash_tool],
    ))?;
    println!("{result}");
    Ok(())
}

/// Load a skill by name, returning an error if not found.
fn require_skill(name: &str) -> Result<String> {
    load_skill(name)?.context(format!("skill \"{name}\" not found"))
}

/// Determine the system prompt and user query from args.
fn resolve_prompt(args: &ParsedInput, skills: &[crate::skill::Skill]) -> Result<(String, String)> {
    match &args.skill {
        Some(name) => {
            let content = require_skill(name)?;
            let query = if args.query.is_empty() {
                name.clone()
            } else {
                args.query.clone()
            };
            println!("Using skill: {name}");
            Ok((content, query))
        }
        None => {
            let mentioned = find_mentioned_skills(&args.query, skills);
            match mentioned.len() {
                0 => {
                    let prompt = build_orchestrator_prompt(skills, &[], &args.query);
                    Ok((prompt, args.query.clone()))
                }
                1 => {
                    let name = &mentioned[0];
                    let content = require_skill(name)?;
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
                    Ok((content, query))
                }
                _ => {
                    let prompt = build_orchestrator_prompt(skills, &mentioned, &args.query);
                    Ok((prompt, args.query.clone()))
                }
            }
        }
    }
}

#[derive(Deserialize)]
struct InlineToolCall {
    #[allow(dead_code)]
    name: String,
    arguments: serde_json::Value,
}

/// Compact a bash execution result into a concise string for the LLM context window.
fn compact_bash_result(result: &serde_json::Value) -> serde_json::Value {
    let stdout = result["stdout"].as_str().unwrap_or("");
    let stderr = result["stderr"].as_str().unwrap_or("");
    let exit_code = result["exitCode"].as_i64().unwrap_or(-1);

    let max_len = 2000;
    let stdout_compact = if stdout.len() > max_len {
        format!(
            "{}... (truncated, {} bytes total)",
            &stdout[..max_len],
            stdout.len()
        )
    } else {
        stdout.to_string()
    };
    let stderr_compact = if stderr.len() > max_len {
        format!(
            "{}... (truncated, {} bytes total)",
            &stderr[..max_len],
            stderr.len()
        )
    } else {
        stderr.to_string()
    };

    serde_json::json!({
        "exitCode": exit_code,
        "stdout": stdout_compact,
        "stderr": stderr_compact,
    })
}
///
/// Apple Foundation Models sometimes return tool calls as formatted text instead of
/// structured `toolCalls`:
///   ```function\n[{"name":"bash","arguments":{"cmd":"ls"}}]```
///   ```text\n{"cmd":"ls"}\n```
fn extract_inline_tool_calls(text: &str) -> Vec<InlineToolCall> {
    let mut calls = Vec::new();
    // Match ```function [...]``` blocks with array of tool calls
    for block in text.split("```") {
        let block = block.trim();
        // Strip the language tag (function, text, etc.)
        let json_str = block.strip_prefix("function").unwrap_or(block).trim();
        if let Ok(parsed) = serde_json::from_str::<Vec<InlineToolCall>>(json_str) {
            calls.extend(parsed);
        }
    }

    // If no array-style calls found, try single-object style: ```text\n{"cmd":"..."}\n```
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

/// Run the agentic tool loop using Apple Foundation Models via FFI.
async fn run_agent_loop(
    client: &crate::aisdk_appleai::AppleClient,
    system_prompt: &str,
    user_query: &str,
    tools: &[aisdk::core::tools::Tool],
) -> Result<String> {
    let mut messages: Vec<Message> = vec![Message::User(user_query.into())];

    for step in 0..50 {
        tracing::debug!(step, messages = messages.len(), "calling model");
        let response = client.generate(system_prompt, &messages, tools).await?;

        let mut text_parts = Vec::new();
        let mut tool_call_infos: Vec<ToolCallInfo> = Vec::new();

        for content in &response.contents {
            match content {
                LanguageModelResponseContentType::Text(text) => {
                    text_parts.push(text.clone());
                }
                LanguageModelResponseContentType::ToolCall(info) => {
                    tool_call_infos.push(info.clone());
                }
                _ => {}
            }
        }

        let text = text_parts.join("\n");

        // If no structured tool calls, check for inline tool calls in text
        if tool_call_infos.is_empty() {
            let inline_calls = extract_inline_tool_calls(&text);
            if inline_calls.is_empty() {
                tracing::debug!(response = %text.chars().take(80).collect::<String>(), "final response");
                return Ok(text);
            }
            tracing::debug!(
                inline_calls = inline_calls.len(),
                "extracted inline tool calls"
            );
            // Execute inline tool calls and feed results as a compact user message
            let mut results = String::new();
            for call in &inline_calls {
                let cmd = call.arguments["cmd"]
                    .as_str()
                    .or_else(|| call.arguments["command"].as_str())
                    .unwrap_or("");
                let result = execute_bash(cmd);
                tracing::debug!(cmd = %cmd, exit_code = %result["exitCode"], "inline tool result");
                let compacted = compact_bash_result(&result);
                let stdout = compacted["stdout"].as_str().unwrap_or("");
                results.push_str(&format!("$ {cmd}\n{stdout}\n"));
            }
            // Use user message so the model understands it needs to summarize
            messages.push(Message::User(
                format!("Tool results:\n{results}\nProvide a clear summary of the results above.")
                    .into(),
            ));
            continue;
        }

        tracing::debug!(tool_calls = tool_call_infos.len(), "executing tool calls");

        let assistant_text = text_parts.join("\n");
        if !assistant_text.is_empty() {
            messages.push(Message::Assistant(AssistantMessage::new(
                LanguageModelResponseContentType::Text(assistant_text),
                None,
            )));
        }

        for tc in &tool_call_infos {
            messages.push(Message::Assistant(AssistantMessage::new(
                LanguageModelResponseContentType::ToolCall(tc.clone()),
                None,
            )));

            let cmd = tc.input["cmd"]
                .as_str()
                .or_else(|| tc.input["command"].as_str())
                .unwrap_or("");

            let result = execute_bash(cmd);
            tracing::debug!(cmd = %cmd, exit_code = %result["exitCode"], "tool result");

            let mut tool_result = aisdk::core::tools::ToolResultInfo::new(&tc.tool.name);
            tool_result.id(tc.tool.id.clone());
            tool_result.output(compact_bash_result(&result));
            messages.push(Message::Tool(tool_result));
        }
    }
    Ok("Max steps reached.".to_string())
}
