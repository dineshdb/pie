use crate::core::prompt;
use crate::core::skill::Skill;
use crate::providers::Model;
use aisdk::core::tools::{Tool, ToolExecute};
use aisdk::core::utils::step_count_is;
use aisdk::core::LanguageModelRequest;
use aisdk::macros::tool;
use serde_json::json;
use std::process::Command;
use std::sync::Arc;

#[tool]
/// Execute a shell command and return its stdout, stderr, and exit code.
pub fn shell_tool(cmd: String) -> Tool {
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
                let sys = prompt::subagent();

                // Build messages: mentioned skills → query
                let mut messages: Vec<aisdk::core::Message> = Vec::new();
                if let Some(skills_msg) = prompt::mentioned_skills_message(&skills, &[&query_with_skill]) {
                    messages.push(aisdk::core::Message::User(aisdk::core::UserMessage::new(skills_msg)));
                }
                messages.push(aisdk::core::Message::User(aisdk::core::UserMessage::new(query)));

                tracing::debug!(skill = %skill_name, query, sys = %sys, "subagent");
                let mut req = LanguageModelRequest::builder()
                    .model(model)
                    .system(&sys)
                    .messages(messages)
                    .with_tool(shell_tool())
                    .stop_when(step_count_is(5))
                    .build();
                match req.generate_text().await {
                    Ok(r) => {
                        let text = r.text().unwrap_or_default();
                        tracing::debug!(skill = %skill_name, len = text.len(), "subagent done");
                        Ok(text)
                    }
                    Err(e) => Err(format!("Subagent failed: {e}")),
                }
            }
        }))
        .build()
        .unwrap()
}
