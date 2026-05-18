use crate::registry::Registry;
use crate::skill::{self, Skill};
use agentsdk::core::tools::{Tool, ToolDefinition, ToolExecute};
use p1e_sandbox::SandboxConfig;
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashSet;
use std::fmt::Write;
use std::sync::{Arc, Mutex};

#[derive(JsonSchema, Deserialize, Serialize)]
struct LoadSkillsInput {
    /// List of skill names to load (e.g., `["filesystem"]`).
    pub skills: Vec<String>,
}

/// Load one or more skills by name. Auto-resolves `needs` dependencies.
#[allow(clippy::unwrap_used)]
pub fn load_skills_tool() -> Tool {
    Tool::builder()
        .definition(
            ToolDefinition::builder()
                .name("load_skills")
                .description("Load one or more skills by name")
                .input_schema(schema_for!(LoadSkillsInput))
                .build()
                .unwrap(),
        )
        .execute(ToolExecute::from_sync(|ctx, params| {
            let input: LoadSkillsInput =
                serde_json::from_value(params).map_err(|e| e.to_string())?;
            let registry = ctx
                .options
                .extensions
                .get::<Arc<Registry>>()
                .ok_or_else(|| "Registry not found in extensions".to_string())?;
            let loaded = ctx.options.extensions.get::<Arc<Mutex<HashSet<String>>>>();

            super::emit_tool_input("load_skills", &json!(input));

            let resolved = Skill::resolve(&registry.skills, &input.skills);
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
            Ok(json!(output))
        }))
        .build()
        .unwrap()
}

#[derive(JsonSchema, Deserialize, Serialize)]
struct LoadReferencesInput {
    /// The name of the skill whose references to load.
    pub skill: String,
    /// List of reference filenames to load (e.g., `["checklist.md"]`).
    pub references: Vec<String>,
}

/// Load reference files of a skill for extra knowledge.
#[allow(clippy::unwrap_used)]
pub fn load_references_tool() -> Tool {
    Tool::builder()
        .definition(
            ToolDefinition::builder()
                .name("load_references")
                .description("Load reference files of a skill for extra knowledge")
                .input_schema(schema_for!(LoadReferencesInput))
                .build()
                .unwrap(),
        )
        .execute(ToolExecute::from_sync(|ctx, params| {
            let input: LoadReferencesInput =
                serde_json::from_value(params).map_err(|e| e.to_string())?;
            let loaded_refs = ctx
                .options
                .extensions
                .get::<Arc<Mutex<HashSet<String>>>>()
                .ok_or_else(|| "loaded_refs not found in extensions".to_string())?;

            super::emit_tool_input("load_references", &json!(input));

            for name in &input.references {
                validate_filename(name, &["md"])?;
            }

            if !skill::skill_exists(&input.skill) {
                return Err(format!("Skill '{}' not found", input.skill));
            }

            let mut output = String::new();
            for ref_name in &input.references {
                let key = format!("{}/{}", input.skill, ref_name);
                if super::safe_lock(&loaded_refs).contains(&key) {
                    writeln!(output, "Reference {key} already loaded — skipping.").ok();
                    continue;
                }
                let Some(content) = skill::load_reference(&input.skill, ref_name) else {
                    writeln!(
                        output,
                        "Error: reference '{ref_name}' not found for skill '{}'",
                        input.skill
                    )
                    .ok();
                    continue;
                };
                write!(output, "### Reference: {key}\n{content}\n---\n").ok();
                super::safe_lock(&loaded_refs).insert(key);
            }
            Ok(json!(output))
        }))
        .build()
        .unwrap()
}

#[derive(JsonSchema, Deserialize, Serialize)]
struct ExecuteSkillScriptInput {
    /// The name of the skill containing the script.
    pub skill: String,
    /// The filename of the script to execute.
    pub script: String,
    /// Optional: Command-line arguments for the script.
    pub args: Option<String>,
}

const SCRIPT_EXTENSIONS: &[&str] = &["sh", "bash", "py", "js", "ts", "rb", "pl"];

/// Execute a script from a skill directory. Runs inside a sandbox with read access to the skill directory.
#[allow(clippy::unwrap_used)]
pub fn execute_skill_script_tool() -> Tool {
    Tool::builder()
        .definition(
            ToolDefinition::builder()
                .name("execute_skill_script")
                .description("Execute a script from a skill directory")
                .input_schema(schema_for!(ExecuteSkillScriptInput))
                .build()
                .unwrap(),
        )
        .execute(ToolExecute::from_sync(|ctx, params| {
            let input: ExecuteSkillScriptInput =
                serde_json::from_value(params).map_err(|e| e.to_string())?;
            let sandbox_settings = ctx
                .options
                .extensions
                .get::<Arc<SandboxConfig>>()
                .ok_or_else(|| "SandboxConfig not found in extensions".to_string())?;

            super::emit_tool_input("execute_skill_script", &json!(input));

            validate_filename(&input.script, SCRIPT_EXTENSIONS)?;

            let Some(dir) = skill::skill_dir(&input.skill) else {
                return Err(format!("Skill '{}' not found", input.skill));
            };

            let script_path = dir.join(&input.script);
            if !script_path.exists() {
                return Err(format!(
                    "Script '{}' not found for skill '{}'",
                    input.script, input.skill
                ));
            }

            let args_str = input.args.unwrap_or_default();

            let cmd = if args_str.is_empty() {
                format!("\"{}\"", script_path.display())
            } else {
                format!("\"{}\" {}", script_path.display(), args_str)
            };

            let mut sandbox = (**sandbox_settings).clone();
            let skill_path = dir.to_string_lossy().to_string();
            if !sandbox.allow_read.contains(&skill_path) {
                sandbox.allow_read.push(skill_path);
            }

            let out = super::run_sandboxed_command(&cmd, &sandbox);
            Ok(json!({
                "cmd": cmd,
                "code": out.exit_code,
                "stdout": out.stdout,
                "stderr": out.stderr,
            }))
        }))
        .build()
        .unwrap()
}

/// Validate a filename: no path traversal, absolute paths, or hidden files; must match allowed extensions.
fn validate_filename(name: &str, allowed_exts: &[&str]) -> Result<(), String> {
    if name.contains("..") || name.starts_with('/') || name.starts_with('.') {
        return Err(format!(
            "Invalid filename '{name}': path traversal, absolute paths, and hidden files are not allowed"
        ));
    }
    let ext = std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str());
    let valid = ext.is_some_and(|e| {
        allowed_exts
            .iter()
            .any(|allowed| e.eq_ignore_ascii_case(allowed))
    });
    if !valid {
        let list = allowed_exts.join(", .");
        return Err(format!(
            "Invalid filename '{name}': only .{list} files are allowed"
        ));
    }
    Ok(())
}
