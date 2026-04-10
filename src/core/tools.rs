use crate::core::prompt;
use crate::core::sandbox;
use crate::core::skill::Skill;
use crate::providers::Model;
use aisdk::core::LanguageModelRequest;
use aisdk::core::tools::{Tool, ToolExecute};
use aisdk::core::utils::step_count_is;
use serde_json::json;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct ShellInput {
    cmd: String,
}

/// Execute a shell command inside the sandbox and return its stdout, stderr, and exit code.
pub fn shell_tool(sandbox_settings: PathBuf) -> Tool {
    let sandbox_settings = Arc::new(sandbox_settings);
    Tool::builder()
        .name("shell_tool")
        .description("Execute a shell command and return its stdout, stderr, and exit code.")
        .input_schema(schemars::schema_for!(ShellInput))
        .execute(ToolExecute::from_sync(move |_ctx, params| {
            let cmd = match params.get("cmd").and_then(|v| v.as_str()) {
                Some(c) => c.to_string(),
                None => return Err("cmd parameter is required".to_string()),
            };
            tracing::debug!(cmd = %cmd, "shell:");
            let output = sandbox::build_command(&cmd, &sandbox_settings)
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

/// Resolve skill names to skills, including their `needs` dependencies.
fn resolve_with_needs<'a>(names: &[String], skills: &'a [Skill]) -> Vec<&'a Skill> {
    let mut resolved = Vec::new();
    let mut seen = HashSet::new();

    for name in names {
        if let Some(skill) = skills.iter().find(|s| s.name == *name) {
            if seen.insert(skill.name.clone()) {
                resolved.push(skill);
                // Auto-load needs deps
                for need in &skill.needs {
                    if seen.insert(need.clone()) {
                        if let Some(dep) = skills.iter().find(|s| s.name == *need) {
                            resolved.push(dep);
                        }
                    }
                }
            }
        }
    }
    resolved
}

/// Load one or more skills by name. Auto-resolves `needs` dependencies.
pub fn load_skills_tool(skills: Vec<Skill>) -> Tool {
    let skills = Arc::new(skills);
    Tool::builder()
        .name("load_skills")
        .description("Load skill instructions by name. Auto-resolves needs dependencies. Use this when you need skill knowledge to answer directly, without delegating to a subagent.")
        .input_schema(schemars::schema_for!(LoadSkillsInput))
        .execute(ToolExecute::from_sync(move |_ctx, params| {
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

            let resolved = resolve_with_needs(&names, &skills);

            if resolved.is_empty() {
                return Err("No skills found matching the requested names".to_string());
            }

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
struct LoadReferencesInput {
    /// Skill name whose references to load.
    skill: String,
    /// Reference filenames to load (e.g. ["checklist.md"]).
    /// Already-loaded references are skipped automatically.
    references: Vec<String>,
}

/// Load reference files from a skill directory. Tracks what's already loaded.
pub fn load_references_tool(loaded_refs: Arc<Mutex<HashSet<String>>>) -> Tool {
    Tool::builder()
        .name("load_references")
        .description("Load reference files from a skill directory. Pass a skill name and list of .md filenames (e.g. [\"checklist.md\"]). Already-loaded references are tracked and skipped.")
        .input_schema(schemars::schema_for!(LoadReferencesInput))
        .execute(ToolExecute::from_sync(move |_ctx, params| {
            let skill_name = match params.get("skill").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => return Err("skill parameter is required".to_string()),
            };

            let ref_names: Vec<String> = params
                .get("references")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();

            if ref_names.is_empty() {
                return Err("references parameter must be a non-empty array of filenames".to_string());
            }

            // Validate reference names
            for name in &ref_names {
                if name.contains("..") || name.starts_with('/') || name.starts_with('.') {
                    return Err(format!(
                        "Invalid reference '{}': path traversal, absolute paths, and hidden files are not allowed",
                        name
                    ));
                }
            }

            let mut output = String::new();
            for ref_name in &ref_names {
                let key = format!("{}/{}", skill_name, ref_name);
                {
                    let loaded = loaded_refs.lock().unwrap();
                    if loaded.contains(&key) {
                        output.push_str(&format!(
                            "Reference {} already loaded — skipping.\n",
                            key
                        ));
                        continue;
                    }
                }
                match crate::core::skill::read_reference(&skill_name, ref_name) {
                    Ok(content) => {
                        output.push_str(&format!(
                            "### Reference: {}\n{}\n---\n",
                            key, content
                        ));
                        loaded_refs.lock().unwrap().insert(key);
                    }
                    Err(e) => {
                        output.push_str(&format!(
                            "Error loading reference {}: {}\n",
                            ref_name, e
                        ));
                    }
                }
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

pub fn subagent_tool(model: Model, skills: Vec<Skill>, sandbox_settings: PathBuf) -> Tool {
    let model = Arc::new(model);
    let skills = Arc::new(skills);
    let sandbox_settings = Arc::new(sandbox_settings);
    Tool::builder()
        .name("subagent")
        .description("Delegate a task after adding more details such as /<skill-mentions>, requirements, details, etc.")
        .input_schema(schemars::schema_for!(SubagentInput))
        .execute(ToolExecute::from_async(move |_ctx, params| {
            let model = (*model).clone();
            let skills = skills.clone();
            let sandbox_ref = sandbox_settings.clone();
            async move {
                let skill_name = params["skill_name"].as_str().unwrap_or_default();
                let query = params["query"].as_str().unwrap_or_default();
                if skill_name.is_empty() || query.is_empty() {
                    return Err("skill_name and query are required".to_string());
                }
                if !skills.iter().any(|s| s.name == skill_name) {
                    return Ok(format!("Skill '{}' not found.", skill_name));
                };

                // Build a minimal context for the subagent
                let (date, pwd) = crate::core::prompt::context_vars();
                let sys = prompt::subagent_prompt(prompt::git_repo_root());

                let mut user_content = String::new();
                user_content.push_str(&format!("Date: {date} Working directory: {pwd}\n\n"));
                user_content.push_str(&format!("Query: {query}"));

                let messages: Vec<aisdk::core::Message> = vec![
                    aisdk::core::Message::User(aisdk::core::UserMessage::new(user_content)),
                ];

                tracing::debug!(skill = %skill_name, query, %sys, "subagent");
                let mut req = LanguageModelRequest::builder()
                    .model(model)
                    .system(sys)
                    .messages(messages)
                    .with_tool(shell_tool((*sandbox_ref).clone()))
                    .with_tool(load_skills_tool((*skills).clone()))
                    .with_tool(load_references_tool(Arc::new(Mutex::new(HashSet::new()))))
                    .stop_when(step_count_is(10))
                    .build();
                match req.generate_text().await {
                    Ok(r) => {
                        let text = r.text().unwrap_or_default();
                        tracing::debug!(skill = %skill_name, len = text.len(), %text, "subagent done");
                        Ok(text)
                    }
                    Err(e) => Err(format!("Subagent failed: {e}")),
                }
            }
        }))
        .build()
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_with_needs_basic() {
        let skills = vec![
            Skill {
                name: "review".into(),
                description: "code review".into(),
                content: "content".into(),
                needs: vec!["filesystem".into(), "developer".into()],
            },
            Skill {
                name: "filesystem".into(),
                description: "file ops".into(),
                content: "content".into(),
                needs: vec![],
            },
            Skill {
                name: "developer".into(),
                description: "dev workflow".into(),
                content: "content".into(),
                needs: vec![],
            },
        ];
        let resolved = resolve_with_needs(&["review".to_string()], &skills);
        let names: Vec<&str> = resolved.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"review"), "requested skill must resolve");
        assert!(names.contains(&"filesystem"), "needs dep must auto-load");
        assert!(names.contains(&"developer"), "needs dep must auto-load");
    }

    #[test]
    fn resolve_with_needs_deduplicates() {
        let skills = vec![
            Skill {
                name: "a".into(),
                description: "a".into(),
                content: "a".into(),
                needs: vec!["b".into()],
            },
            Skill {
                name: "b".into(),
                description: "b".into(),
                content: "b".into(),
                needs: vec!["a".into()],
            },
        ];
        let resolved = resolve_with_needs(&["a".to_string(), "b".to_string()], &skills);
        assert_eq!(resolved.len(), 2, "circular needs must not duplicate");
    }

    #[test]
    fn resolve_with_needs_empty_for_unknown() {
        let skills = vec![Skill {
            name: "a".into(),
            description: "a".into(),
            content: "a".into(),
            needs: vec![],
        }];
        let resolved = resolve_with_needs(&["nonexistent".to_string()], &skills);
        assert!(resolved.is_empty());
    }
}
