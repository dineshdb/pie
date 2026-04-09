use crate::core::prompt;
use crate::core::skill::Skill;
use crate::providers::Model;
use aisdk::core::LanguageModelRequest;
use aisdk::core::tools::{Tool, ToolExecute};
use aisdk::core::utils::step_count_is;
use serde_json::json;
use std::collections::HashSet;
use std::process::Command;
use std::sync::Arc;

#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct ShellInput {
    cmd: String,
}

/// Execute a shell command and return its stdout, stderr, and exit code.
pub fn shell_tool() -> Tool {
    Tool::builder()
        .name("shell_tool")
        .description("Execute a shell command and return its stdout, stderr, and exit code.")
        .input_schema(schemars::schema_for!(ShellInput))
        .execute(ToolExecute::from_sync(|_ctx, params| {
            let cmd = match params.get("cmd").and_then(|v| v.as_str()) {
                Some(c) => c.to_string(),
                None => return Err("cmd parameter is required".to_string()),
            };
            tracing::debug!(cmd = %cmd, "shell:");
            let output = Command::new("sh")
                .arg("-c")
                .arg(&cmd)
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("PAGER", "cat")
                .env("EDITOR", "true")
                .output();
            let result = match output {
                Ok(out) => {
                    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                    let exit_code = out.status.code().unwrap_or(-1);
                    tracing::debug!(exit_code, stdout_len = stdout.len(), "shell_tool done");
                    json!({
                        "cmd": cmd,
                        "exitCode": exit_code,
                        "stdout": stdout,
                        "stderr": stderr,
                        "success": exit_code == 0
                    })
                }
                Err(e) => {
                    tracing::debug!(error = %e, "shell_tool failed");
                    json!({
                        "cmd": cmd,
                        "exitCode": -1,
                        "stdout": "",
                        "stderr": e.to_string(),
                        "success": false
                    })
                }
            };
            Ok(serde_json::to_string(&result).unwrap_or_default())
        }))
        .build()
        .unwrap()
}

#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct LoadSkillsInput {
    /// List of skill names to load (e.g. ["filesystem", "developer"])
    skills: Vec<String>,
}

/// Load one or more skills by name. Recursively resolves any additional skills
/// mentioned within the loaded skill contents. Returns the full skill instructions
/// inline so the caller can use them directly.
pub fn load_skills_tool(skills: Vec<Skill>) -> Tool {
    let skills = Arc::new(skills);
    Tool::builder()
        .name("load_skills")
        .description("Load skill instructions by name. Recursively resolves all mentioned skills. Use this when you need skill knowledge to answer directly, without delegating to a subagent.")
        .input_schema(schemars::schema_for!(LoadSkillsInput))
        .execute(ToolExecute::from_sync(move |_ctx, params| {
            let skills = skills.clone();
            let names: Vec<String> = params
                .get("skills")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();

            if names.is_empty() {
                return Err("skills parameter must be a non-empty array of skill names".to_string());
            }

            // Recursively resolve: start with requested names, then scan their
            // content for mentions of other skills (using /<skill-name> pattern).
            let mut resolved: Vec<&Skill> = Vec::new();
            let mut seen: HashSet<String> = HashSet::new();
            let mut queue: Vec<String> = names;

            while let Some(name) = queue.pop() {
                if seen.contains(&name) {
                    continue;
                }
                let Some(skill) = skills.iter().find(|s| s.name == name) else {
                    continue;
                };
                seen.insert(name);
                // Scan this skill's content for mentions of other skills
                for other in skills.iter() {
                    if !seen.contains(&other.name)
                        && skill.content.contains(&format!("/{}", other.name))
                    {
                        queue.push(other.name.clone());
                    }
                }
                resolved.push(skill);
            }

            if resolved.is_empty() {
                return Err("No skills found matching the requested names".to_string());
            }

            resolved.sort_by_key(|s| &s.name);

            let mut output = String::new();
            for skill in &resolved {
                output.push_str(&format!("## Skill: {}\n{}\n---\n", skill.name, skill.content));
            }
            Ok(output)
        }))
        .build()
        .unwrap()
}

#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct SubagentInput {
    skill_name: String,
    query: String,
}

pub fn subagent_tool(model: Model, skills: Vec<Skill>) -> Tool {
    let model = Arc::new(model);
    let skills = Arc::new(skills);
    Tool::builder()
        .name("subagent")
        .description("Delegate a task after adding more details such as /<skill-mentions>, requirements, details, etc.")
        .input_schema(schemars::schema_for!(SubagentInput))
        .execute(ToolExecute::from_async(move |_ctx, params| {
            let model = (*model).clone();
            let skills = skills.clone();
            async move {
                let skill_name = params["skill_name"].as_str().unwrap_or_default();
                let query = params["query"].as_str().unwrap_or_default();
                if skill_name.is_empty() || query.is_empty() {
                    return Err("skill_name and query are required".to_string());
                }
                if !skills.iter().any(|s| s.name == skill_name) {
                    return Ok(format!("Skill '{}' not found.", skill_name));
                };

                let query_with_skill = format!("/{} {}", skill_name, query);

                // Build a minimal context for the subagent — avoid overwhelming small models
                let (date, pwd) = crate::core::prompt::context_vars();
                let sys = prompt::subagent_role();

                let mut user_content = String::new();
                if let Some(skills_msg) = prompt::mentioned_skills_message(&skills, &[&query_with_skill]) {
                    user_content.push_str(&skills_msg);
                    user_content.push_str("\n\n");
                }
                user_content.push_str(&format!("Date: {date} Working directory: {pwd}\n\n"));
                user_content.push_str(&format!("Query: {query}"));

                let messages: Vec<aisdk::core::Message> = vec![
                    aisdk::core::Message::User(aisdk::core::UserMessage::new(user_content)),
                ];

                tracing::debug!(skill = %skill_name, query, sys, "subagent");
                for (i, msg) in messages.iter().enumerate() {
                    tracing::debug!(i, ?msg, "subagent message");
                }
                let mut req = LanguageModelRequest::builder()
                    .model(model)
                    .system(sys)
                    .messages(messages)
                    .with_tool(shell_tool())
                    .stop_when(step_count_is(5))
                    .build();
                match req.generate_text().await {
                    Ok(r) => {
                        let text = r.text().unwrap_or_default();
                        tracing::debug!(skill = %skill_name, len = text.len(), text, "subagent done");
                        Ok(text)
                    }
                    Err(e) => Err(format!("Subagent failed: {e}")),
                }
            }
        }))
        .build()
        .unwrap()
}
