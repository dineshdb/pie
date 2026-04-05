use crate::provider::Model;
use crate::skill::{find_mentioned_skills, get_all_skills, load_skill};
use aisdk::core::tools::Tool;
use aisdk::core::utils::step_count_is;
use aisdk::core::LanguageModelRequest;
use aisdk::macros::tool;
use anyhow::{Context, Result};
use serde_json::json;
use std::process::Command;

pub struct ParsedInput {
    pub skill: Option<String>,
    pub query: String,
}

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
    let skills = get_all_skills();
    let instructions = resolve_instructions(args, &skills);
    let query = resolve_query(args, &skills);

    let system = system_prompt();
    let user_msg = format!("Instructions:\n{instructions}\n\nQuestion: {query}");

    tracing::debug!(query = %query, "agent:");

    let mut req = LanguageModelRequest::builder()
        .model(model.clone())
        .system(&system)
        .prompt(&user_msg)
        .with_tool(shell_tool())
        .stop_when(step_count_is(10))
        .build();

    let result = req.generate_text().await.context("generate_text failed")?;
    println!("{}", result.text().unwrap_or_default());
    Ok(())
}

/// Resolve instructions: load skill content if found, otherwise include all skills as reference.
fn resolve_instructions(args: &ParsedInput, skills: &[crate::skill::Skill]) -> String {
    // Explicit -s flag
    if let Some(name) = &args.skill {
        if let Ok(Some(prompt)) = load_skill(name) {
            return prompt;
        }
    }

    // Inline /skill-name in query
    let mentioned = find_mentioned_skills(&args.query, skills);
    if mentioned.len() == 1 {
        if let Ok(Some(prompt)) = load_skill(&mentioned[0]) {
            return prompt;
        }
    }

    // No skill found — load all skill contents so model knows the commands
    let contents: Vec<String> = skills
        .iter()
        .filter_map(|s| load_skill(&s.name).ok().flatten())
        .collect();
    if contents.is_empty() {
        "Use shell_tool to execute commands and answer the question.".into()
    } else {
        contents.join("\n\n---\n\n")
    }
}

/// Resolve query: strip /skill-name prefix if present.
fn resolve_query(args: &ParsedInput, skills: &[crate::skill::Skill]) -> String {
    let mentioned = find_mentioned_skills(&args.query, skills);
    if mentioned.len() == 1 {
        let name = &mentioned[0];
        let stripped = args
            .query
            .replace(&format!("/{name}"), "")
            .trim()
            .to_string();
        if !stripped.is_empty() {
            return stripped;
        }
    }
    if args.query.is_empty() {
        args.skill.clone().unwrap_or_default()
    } else {
        args.query.clone()
    }
}

fn system_prompt() -> String {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    format!(
        "You are a helpful assistant. Today's date: {today}.\n\n\
         Follow the instructions carefully. Use shell_tool to execute commands when needed.\n\
         After receiving tool results, provide your final answer immediately.\n\
         Be concise and accurate."
    )
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
