use crate::bash::{bash, execute_bash};
use crate::interactive::ParsedInput;
use crate::router::{build_orchestrator_prompt, find_mentioned_skills};
use crate::skill::get_all_skills;
use aisdk::core::language_model::LanguageModelResponseContentType;
use aisdk::core::messages::{AssistantMessage, Message};
use aisdk::core::tools::ToolCallInfo;
use anyhow::{Context, Result};

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
    let client = crate::aisdk_appleai::AppleClient::new()
        .context("Apple Intelligence not available")?;
    let rt = tokio::runtime::Runtime::new()?;
    let skills = get_all_skills();

    let (system_prompt, user_query) = resolve_prompt(args, &skills)?;

    // Get the tool from #[tool] macro
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

/// Determine the system prompt and user query from args.
fn resolve_prompt(args: &ParsedInput, skills: &[crate::skill::Skill]) -> Result<(String, String)> {
    match &args.skill {
        Some(name) => {
            let content = crate::skill::load_skill(name)?
                .context(format!("skill \"{name}\" not found"))?;
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
                    let content = crate::skill::load_skill(name)?
                        .context(format!("skill \"{name}\" not found"))?;
                    println!("Using skill: {name}");
                    let query = args.query.replace(&format!("/{name}"), "").trim().to_string();
                    let query = if query.is_empty() { name.clone() } else { query };
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

/// Run the agentic tool loop using Apple Foundation Models via FFI.
///
/// Uses `aisdk` types (`Message`, `Tool`, `LanguageModelResponse`) throughout,
/// with the FFI conversion happening inside `AppleClient::generate()`.
async fn run_agent_loop(
    client: &crate::aisdk_appleai::AppleClient,
    system_prompt: &str,
    user_query: &str,
    tools: &[aisdk::core::tools::Tool],
) -> Result<String> {
    let mut messages: Vec<Message> = vec![Message::User(user_query.into())];

    for _step in 0..50 {
        let response = client.generate(system_prompt, &messages, tools).await?;

        // Extract text and tool calls from the response
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

        // If no tool calls, return the text
        if tool_call_infos.is_empty() {
            return Ok(text_parts.join("\n"));
        }

        // Add assistant message to history
        let assistant_text = text_parts.join("\n");
        if !assistant_text.is_empty() {
            messages.push(Message::Assistant(AssistantMessage::new(
                LanguageModelResponseContentType::Text(assistant_text),
                None,
            )));
        }

        // Process each tool call
        for tc in &tool_call_infos {
            // Record the tool call as an assistant message
            messages.push(Message::Assistant(AssistantMessage::new(
                LanguageModelResponseContentType::ToolCall(tc.clone()),
                None,
            )));

            // Execute the bash tool
            let cmd = tc.input["cmd"]
                .as_str()
                .or_else(|| tc.input["command"].as_str())
                .unwrap_or("");

            let result = execute_bash(cmd);

            // Record the tool result
            let mut tool_result = aisdk::core::tools::ToolResultInfo::new(&tc.tool.name);
            tool_result.id(tc.tool.id.clone());
            tool_result.output(result);
            messages.push(Message::Tool(tool_result));
        }
    }

    Ok("Max steps reached.".to_string())
}
