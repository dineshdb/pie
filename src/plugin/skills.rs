use crate::config::EMBEDDED_PIE_DIR;
use crate::registry::Registry;
use crate::skill::{Skill, format_skills_markdown};
use agentsdk::core::plugin::PluginToolCall;
use agentsdk::core::tools::ToolDefinition;
use agentsdk::{AgentPlugin, Messages, PluginContext};
use async_trait::async_trait;
use p1e_sandbox::SandboxConfig;
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt::Write;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Debug)]
pub struct SkillsPlugin {
    registry: Arc<Registry>,
    sandbox: Arc<SandboxConfig>,
    loaded_skills: Arc<Mutex<HashSet<String>>>,
    loaded_refs: Arc<Mutex<HashSet<String>>>,
}

impl SkillsPlugin {
    pub fn new(registry: Arc<Registry>, sandbox: Arc<SandboxConfig>) -> Self {
        Self {
            registry,
            sandbox,
            loaded_skills: Arc::new(Mutex::new(HashSet::new())),
            loaded_refs: Arc::new(Mutex::new(HashSet::new())),
        }
    }
}

#[async_trait]
impl AgentPlugin for SkillsPlugin {
    fn name(&self) -> &'static str {
        "skills"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "load_skills".into(),
                description: "Load one or more skills by name".into(),
                input_schema: schema_for!(LoadSkillsInput),
            },
            ToolDefinition {
                name: "load_references".into(),
                description: "Load reference files of a skill for extra knowledge".into(),
                input_schema: schema_for!(LoadReferencesInput),
            },
            ToolDefinition {
                name: "execute_skill_script".into(),
                description: "Execute a script from a skill directory".into(),
                input_schema: schema_for!(ExecuteSkillScriptInput),
            },
        ]
    }

    async fn run_tool(
        &mut self,
        _ctx: &mut PluginContext,
        call: &PluginToolCall,
    ) -> Result<Value, String> {
        match call.name.as_str() {
            "load_skills" => {
                let input: LoadSkillsInput =
                    serde_json::from_value(call.arguments.clone()).map_err(|e| e.to_string())?;
                crate::tools::emit_tool_input("load_skills", &json!(input));
                self.do_load_skills(&input)
            }
            "load_references" => {
                let input: LoadReferencesInput =
                    serde_json::from_value(call.arguments.clone()).map_err(|e| e.to_string())?;
                crate::tools::emit_tool_input("load_references", &json!(input));
                self.do_load_references(&input)
            }
            "execute_skill_script" => {
                let input: ExecuteSkillScriptInput =
                    serde_json::from_value(call.arguments.clone()).map_err(|e| e.to_string())?;
                crate::tools::emit_tool_input("execute_skill_script", &json!(input));
                self.do_execute_skill_script(&input)
            }
            _ => Err(format!("Unknown skills tool: {}", call.name)),
        }
    }

    async fn prepare_system_prompt(
        &mut self,
        _ctx: &PluginContext,
        _history: &Messages,
    ) -> Option<Cow<'static, str>> {
        let mut skills = Vec::new();

        for item in &self.registry.completions {
            if matches!(item.kind, crate::registry::CompletionKind::Skill) {
                skills.push(format!("- [s] {}: {}", item.label, item.description));
            }
        }

        if skills.is_empty() {
            return None;
        }

        let content = format!("{SKILLS_SECTION}\n{}", skills.join("\n"));
        Some(Cow::Owned(content))
    }
}

impl SkillsPlugin {
    fn do_load_skills(&self, input: &LoadSkillsInput) -> Result<Value, String> {
        let resolved = Skill::resolve(&self.registry.skills, &input.skills);
        if resolved.is_empty() {
            return Err("No skills found matching the requested names".to_string());
        }

        let mut output = String::new();
        let mut to_format = Vec::new();
        for skill in &resolved {
            let mut guard = crate::tools::safe_lock(&self.loaded_skills);
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
            to_format.push(*skill);
        }
        output.push_str(&format_skills_markdown(&to_format));
        Ok(json!(output))
    }

    fn do_load_references(&self, input: &LoadReferencesInput) -> Result<Value, String> {
        for name in &input.references {
            validate_filename(name, &["md"])?;
        }

        if !skill_exists(&input.skill) {
            return Err(format!("Skill '{}' not found", input.skill));
        }

        let mut output = String::new();
        for ref_name in &input.references {
            let key = format!("{}/{}", input.skill, ref_name);
            if crate::tools::safe_lock(&self.loaded_refs).contains(&key) {
                writeln!(output, "Reference {key} already loaded — skipping.").ok();
                continue;
            }
            let Some(content) = load_reference(&input.skill, ref_name) else {
                writeln!(
                    output,
                    "Error: reference '{ref_name}' not found for skill '{}'",
                    input.skill
                )
                .ok();
                continue;
            };
            write!(output, "### Reference: {key}\n{content}\n---\n").ok();
            crate::tools::safe_lock(&self.loaded_refs).insert(key);
        }
        Ok(json!(output))
    }

    fn do_execute_skill_script(&self, input: &ExecuteSkillScriptInput) -> Result<Value, String> {
        validate_filename(&input.script, SCRIPT_EXTENSIONS)?;

        let Some(dir) = skill_dir(&input.skill) else {
            return Err(format!("Skill '{}' not found", input.skill));
        };

        let script_path = dir.join(&input.script);
        if !script_path.exists() {
            return Err(format!(
                "Script '{}' not found for skill '{}'",
                input.script, input.skill
            ));
        }

        let args_str = input.args.clone().unwrap_or_default();

        let cmd = if args_str.is_empty() {
            format!("\"{}\"", script_path.display())
        } else {
            format!("\"{}\" {}", script_path.display(), args_str)
        };

        let mut sandbox = self.sandbox.as_ref().clone();
        let skill_path = dir.to_string_lossy().to_string();
        if !sandbox.allow_read.contains(&skill_path) {
            sandbox.allow_read.push(skill_path);
        }

        let out = crate::tools::run_sandboxed_command(&cmd, &sandbox);
        Ok(json!({
            "cmd": cmd,
            "code": out.exit_code,
            "stdout": out.stdout,
            "stderr": out.stderr,
        }))
    }
}

fn skills_root() -> PathBuf {
    crate::config::pie_home().join("skills")
}

fn embedded_skills_dir() -> Option<&'static include_dir::Dir<'static>> {
    EMBEDDED_PIE_DIR.get_dir("skills")
}

fn skill_dir(name: &str) -> Option<PathBuf> {
    let dir = skills_root().join(name);
    dir.join("SKILL.md").exists().then_some(dir)
}

fn skill_exists(name: &str) -> bool {
    skill_dir(name).is_some()
        || embedded_skills_dir().is_some_and(|dir| dir.get_dir(name).is_some())
}

fn load_reference(skill_name: &str, ref_name: &str) -> Option<String> {
    if let Some(dir) = skill_dir(skill_name)
        && let Ok(content) = fs::read_to_string(dir.join(ref_name))
    {
        return Some(content);
    }
    let full_path = format!("{skill_name}/{ref_name}");
    let path = std::path::Path::new(&full_path);
    embedded_skills_dir().and_then(|dir| {
        dir.dirs()
            .find(|d| d.path() == std::path::Path::new(skill_name))
            .and_then(|skill_dir| skill_dir.files().find(|f| f.path() == path))
            .and_then(|file| file.contents_utf8())
            .map(ToString::to_string)
    })
}

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

const SKILLS_SECTION: &str = r"
## Skills
Skills are extra knowledge you can load on-demand using `skills__load_skills` tool.
Skills can't be invoked directly as tools.
Available skills:
";

#[derive(JsonSchema, Deserialize, Serialize)]
struct LoadSkillsInput {
    /// List of skill names to load (e.g., `["filesystem"]`).
    pub skills: Vec<String>,
}

#[derive(JsonSchema, Deserialize, Serialize)]
struct LoadReferencesInput {
    /// The name of the skill whose references to load.
    pub skill: String,
    /// List of reference filenames to load (e.g., `["checklist.md"]`).
    pub references: Vec<String>,
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
