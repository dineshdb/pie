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

fn extract_string_array(params: &serde_json::Value, key: &str) -> Vec<String> {
    params
        .get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(ToString::to_string))
                .collect()
        })
        .unwrap_or_default()
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
        .description("Load skill knowledge")
        .input_schema(schemars::schema_for!(LoadSkillsInput))
        .execute(ToolExecute::from_sync(move |_ctx, params| {
            let names = extract_string_array(&params, "skills");

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
                    let mut guard = super::safe_lock(loaded);
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

/// Validate a reference filename: no path traversal, absolute paths, hidden files; must be .md.
fn validate_ref_name(name: &str) -> Result<(), String> {
    if name.contains("..") || name.starts_with('/') || name.starts_with('.') {
        return Err(format!(
            "Invalid reference '{name}': path traversal, absolute paths, and hidden files are not allowed"
        ));
    }
    let is_md = Path::new(name)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"));
    if !is_md {
        return Err(format!(
            "Invalid reference '{name}': only .md files are allowed"
        ));
    }
    Ok(())
}

/// Load reference files from a skill directory. Tracks what's already loaded.
#[allow(clippy::unwrap_used)]
pub fn load_references_tool(loaded_refs: Arc<Mutex<HashSet<String>>>) -> Tool {
    Tool::builder()
        .name("load_references")
        .description("Load reference files of a skill for extra knowledge")
        .input_schema(schemars::schema_for!(LoadReferencesInput))
        .execute(ToolExecute::from_sync(move |_ctx, params| {
            let Some(skill_name) = params.get("skill").and_then(|v| v.as_str()) else {
                return Err("skill parameter is required".to_string());
            };
            let skill_name = skill_name.to_string();

            let ref_names = extract_string_array(&params, "references");

            if ref_names.is_empty() {
                return Err(
                    "references parameter must be a non-empty array of filenames".to_string(),
                );
            }

            for name in &ref_names {
                validate_ref_name(name)?;
            }

            if !skill::skill_exists(&skill_name) {
                return Err(format!("Skill '{skill_name}' not found"));
            }

            let mut output = String::new();
            for ref_name in &ref_names {
                let key = format!("{skill_name}/{ref_name}");
                if super::safe_lock(&loaded_refs).contains(&key) {
                    writeln!(output, "Reference {key} already loaded — skipping.").ok();
                    continue;
                }
                let Some(content) = skill::load_reference(&skill_name, ref_name) else {
                    writeln!(
                        output,
                        "Error: reference '{ref_name}' not found for skill '{skill_name}'"
                    )
                    .ok();
                    continue;
                };
                write!(output, "### Reference: {key}\n{content}\n---\n").ok();
                super::safe_lock(&loaded_refs).insert(key);
            }
            Ok(output)
        }))
        .build()
        .unwrap()
}