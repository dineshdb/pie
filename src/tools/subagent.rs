use crate::prompt;
use crate::providers::Model;
use crate::skill::Skill;
use crate::tools::{load_references_tool, load_skills_tool, shell_tool};
use aisdk::core::LanguageModelRequest;
use aisdk::core::tools::{Tool, ToolExecute};
use aisdk::core::utils::step_count_is;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

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
                let (date, pwd) = prompt::context_vars();
                let sys = prompt::subagent_prompt(crate::utils::git_repo_root());

                // Auto-load the target skill and its needs deps
                let query_with_skill = format!("/{} {}", skill_name, query);
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
