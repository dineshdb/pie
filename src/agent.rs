use crate::provider::Model;
use crate::skill::{find_mentioned_skills, get_all_skills, load_skill};
use aisdk::core::tools::Tool;
use aisdk::core::utils::step_count_is;
use aisdk::core::LanguageModelRequest;
use aisdk::macros::tool;
use anyhow::{Context, Result};
use serde_json::json;
use std::process::Command;
use tracing::info;

/// Parsed user input with optional skill selection.
pub struct ParsedInput {
    pub skill: Option<String>,
    pub query: String,
}

const AUTO_SKILL: &str = "auto";

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

pub async fn handle_query(model: &mut Model, args: &ParsedInput) -> Result<()> {
    tracing::debug!(query = %args.query, "query");

    let skills = get_all_skills();
    let (skill_name, prompt, query) = resolve_skill(args, &skills);

    tracing::debug!(skill = %skill_name, query = %query, "executing");

    let result = run_agent_loop(model, &prompt, &query, &skill_name, &skills).await?;
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

#[tool]
fn shell_tool(cmd: String) -> Tool {
    let result = execute(&cmd);
    Ok(serde_json::to_string(&result).unwrap_or_default())
}

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

fn run_agent_loop<'a>(
    model: &'a mut Model,
    instructions: &'a str,
    user_query: &'a str,
    skill_name: &'a str,
    skills: &'a [crate::skill::Skill],
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>> {
    Box::pin(async move {
        // Auto-skill routing: first call without tools to get delegation
        if skill_name == AUTO_SKILL {
            let mut req = LanguageModelRequest::builder()
                .model(model.clone())
                .system("You are a helpful assistant.")
                .prompt(format!(
                    "Instructions:\n{instructions}\n\nQuestion: {user_query}"
                ))
                .build();

            let result = req.generate_text().await.context("generate_text failed")?;
            let text = result.text().unwrap_or_default();

            if let Some((delegated_skill, query)) = parse_skill_delegation(&text) {
                if let Ok(Some(skill_content)) = load_skill(&delegated_skill) {
                    let query = if query.is_empty() {
                        user_query.to_string()
                    } else {
                        query
                    };
                    tracing::debug!(skill = %delegated_skill, query = %query, "subagent");
                    return run_agent_loop(model, &skill_content, &query, &delegated_skill, skills)
                        .await;
                }
            }
            tracing::debug!(text, "result");
            return Ok(text);
        }

        // Direct skill execution with tools
        let mut req = LanguageModelRequest::builder()
            .model(model.clone())
            .system("You are a helpful assistant. Follow the instructions and use tools to answer.")
            .prompt(format!(
                "Instructions:\n{instructions}\n\nQuestion: {user_query}"
            ))
            .with_tool(shell_tool())
            .stop_when(step_count_is(50))
            .build();

        let result = req.generate_text().await.context("generate_text failed")?;
        Ok(result.text().unwrap_or_default())
    })
}
