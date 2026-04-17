use crate::prompt;
use crate::skill::Skill;
use aisdk::core::tools::{Tool, ToolExecute};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct LoadSkillsInput {
    /// List of skill names to load (e.g. ["filesystem", "developer"])
    skills: Vec<String>,
}

/// Load one or more skills by name. Auto-resolves `needs` dependencies.
///
/// When `loaded` is `Some`, already-loaded skills are skipped with a message
/// and newly loaded ones are recorded. Pass `None` for the main agent (no
/// tracking).
pub fn load_skills_tool(skills: Vec<Skill>, loaded: Option<Arc<Mutex<HashSet<String>>>>) -> Tool {
    let skills = Arc::new(skills);
    Tool::builder()
        .name("load_skills")
        .description(
            "Load skill instructions by name. Auto-resolves needs dependencies. \
             Already-loaded skills are skipped with a message.",
        )
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

            let resolved = prompt::resolve_with_needs(&names, &skills);

            if resolved.is_empty() {
                return Err("No skills found matching the requested names".to_string());
            }

            let mut output = String::new();
            for skill in &resolved {
                if let Some(ref loaded) = loaded {
                    let mut guard = loaded.lock().unwrap();
                    if guard.contains(&skill.name) {
                        output.push_str(&format!(
                            "Skill '{}' is already loaded — skipping.\n",
                            skill.name
                        ));
                        continue;
                    }
                    guard.insert(skill.name.clone());
                }
                output.push_str(&format!(
                    "## Skill: {}\n{}\n---\n",
                    skill.name, skill.content
                ));
            }
            Ok(output)
        }))
        .build()
        .unwrap()
}
