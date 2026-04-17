use crate::prompt;
use crate::skill::{self, Skill};
use aisdk::core::tools::{Tool, ToolExecute};
use std::collections::HashSet;
use std::fmt::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct LoadSkillsInput {
    /// List of skill names to load (e.g. [`"filesystem"`, `"developer"`])
    skills: Vec<String>,
}

/// Load one or more skills by name. Auto-resolves `needs` dependencies.
///
/// When `loaded` is `Some`, already-loaded skills are skipped with a message
/// and newly loaded ones are recorded.
#[allow(clippy::unwrap_used)]
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
                        .filter_map(|v| v.as_str().map(ToString::to_string))
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
                    let mut guard = loaded
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if guard.contains(&skill.name) {
                        writeln!(
                            output,
                            "Skill '{}' is already loaded — skipping.",
                            skill.name
                        )
                        .ok();
                        continue;
                    }
                    guard.insert(skill.name.clone());
                }
                write!(output, "## Skill: {}\n{}\n---\n", skill.name, skill.content).ok();
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
    /// Reference filenames to load (e.g. [`"checklist.md"`]).
    /// Already-loaded references are skipped automatically.
    references: Vec<String>,
}

/// Load reference files from a skill directory. Tracks what's already loaded.
#[allow(clippy::unwrap_used)]
pub fn load_references_tool(loaded_refs: Arc<Mutex<HashSet<String>>>) -> Tool {
    Tool::builder()
        .name("load_references")
        .description("Load reference files from a skill directory")
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
                        .filter_map(|v| v.as_str().map(ToString::to_string))
                        .collect()
                })
                .unwrap_or_default();

            if ref_names.is_empty() {
                return Err("references parameter must be a non-empty array of filenames".to_string());
            }

            for name in &ref_names {
                if name.contains("..") || name.starts_with('/') || name.starts_with('.') {
                    return Err(format!(
                        "Invalid reference '{name}': path traversal, absolute paths, and hidden files are not allowed"
                    ));
                }
                if !Path::new(name)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
                {
                    return Err(format!(
                        "Invalid reference '{name}': only .md files are allowed"
                    ));
                }
            }

            if !skill::skill_exists(&skill_name) {
                return Err(format!("Skill '{skill_name}' not found"));
            }

            let mut output = String::new();
            for ref_name in &ref_names {
                let key = format!("{skill_name}/{ref_name}");
                {
                    let loaded = loaded_refs.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    if loaded.contains(&key) {
                        writeln!(output, "Reference {key} already loaded — skipping.").ok();
                        continue;
                    }
                }
                match skill::load_reference(&skill_name, ref_name) {
                    Some(content) => {
                        write!(output, "### Reference: {key}\n{content}\n---\n").ok();
                        loaded_refs.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(key);
                    }
                    None => {
                        writeln!(
                            output,
                            "Error: reference '{ref_name}' not found for skill '{skill_name}'"
                        ).ok();
                    }
                }
            }
            Ok(output)
        }))
        .build()
        .unwrap()
}
