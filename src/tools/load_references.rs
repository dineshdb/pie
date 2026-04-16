use crate::skill;
use aisdk::core::tools::{Tool, ToolExecute};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

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
                if !name.ends_with(".md") {
                    return Err(format!(
                        "Invalid reference '{}': only .md files are allowed",
                        name
                    ));
                }
            }

            // Check that the skill exists (filesystem or embedded)
            if !skill::skill_exists(&skill_name) {
                return Err(format!("Skill '{}' not found", skill_name));
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
                match skill::load_reference(&skill_name, ref_name) {
                    Some(content) => {
                        output.push_str(&format!(
                            "### Reference: {}\n{}\n---\n",
                            key, content
                        ));
                        loaded_refs.lock().unwrap().insert(key);
                    }
                    None => {
                        output.push_str(&format!(
                            "Error: reference '{ref_name}' not found for skill '{skill_name}'\n"
                        ));
                    }
                }
            }
            Ok(output)
        }))
        .build()
        .unwrap()
}
