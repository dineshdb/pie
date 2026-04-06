use crate::prompt::{subagent, system};
use crate::provider::Model;
use crate::skill::{get_all_skills, Skill};
use aisdk::core::tools::{Tool, ToolExecute};
use aisdk::core::utils::step_count_is;
use aisdk::core::LanguageModelRequest;
use aisdk::macros::tool;
use anyhow::{Context, Result};
use serde_json::json;
use std::process::Command;
use std::sync::Arc;
use tracing::{info, warn};

pub fn handle_list_skills() {
    let skills = get_all_skills();
    if skills.is_empty() {
        warn!("No skills found.");
        return;
    }
    println!("Available skills:");
    for s in &skills {
        println!(" - {}: {}", s.name, s.description);
    }
}

pub async fn handle_query(model: &mut Model, query: &str) -> Result<()> {
    let skills = get_all_skills();
    let system = system(query, &skills);

    tracing::debug!(prompt = %system, query, "agent:");
    let mut req = LanguageModelRequest::builder()
        .model(model.clone())
        .system(&system)
        .prompt(query)
        .with_tool(shell_tool())
        .with_tool(subagent_tool(model.clone(), skills))
        .stop_when(step_count_is(10))
        .build();

    let result = req.generate_text().await.context("generate_text failed")?;
    info!("{}", result.text().unwrap_or_default());
    Ok(())
}

#[tool]
/// Execute a shell command and return its stdout, stderr, and exit code.
fn shell_tool(cmd: String) -> Tool {
    tracing::debug!(cmd = %cmd, "shell:");
    let output = Command::new("sh").arg("-c").arg(&cmd).output();
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

fn subagent_tool(model: Model, skills: Vec<Skill>) -> Tool {
    let model = Arc::new(model);
    let skills = Arc::new(skills);
    Tool::builder()
        .name("subagent")
        .description("Delegate a task after adding more details such as /<skill-mentions>, requirments, details, etc.")
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
                let sys = subagent(&query_with_skill, &skills);
                tracing::debug!(skill = %skill_name, query, sys = %sys, "subagent");
                let mut req = LanguageModelRequest::builder()
                    .model(model)
                    .system(&sys)
                    .prompt(query)
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
