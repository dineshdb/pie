use agentsdk::core::plugin::PluginToolCall;
use agentsdk::core::tools::ToolDefinition;
use agentsdk::{AgentPlugin, PluginContext};
use async_trait::async_trait;
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

fn validate_path(path: &str) -> Result<PathBuf, String> {
    if path.contains(".env") {
        tracing::warn!("LLM is accessing environment file: {path}");
        crate::ui::notify::notify("pie: Env File Access", &format!("LLM is accessing: {path}"));
    }

    let p = Path::new(path);
    if p.is_absolute() {
        let current = std::env::current_dir().map_err(|e| e.to_string())?;
        if p.starts_with(&current) {
            return Ok(p.to_path_buf());
        }
        return Err("Absolute paths outside the workspace are not allowed".to_string());
    }
    for component in p.components() {
        if let std::path::Component::ParentDir = component {
            return Err("Path traversal (..) is not allowed".to_string());
        }
    }
    Ok(p.to_path_buf())
}

#[derive(Debug, Default)]
pub struct FileSystemPlugin;

impl FileSystemPlugin {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl AgentPlugin for FileSystemPlugin {
    fn name(&self) -> &'static str {
        "filesystem"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "read_file".into(),
                description: "Read the content of a file".into(),
                input_schema: schema_for!(ReadFileInput),
            },
            ToolDefinition {
                name: "write_file".into(),
                description: "Write content to a file. Overwrites if it exists".into(),
                input_schema: schema_for!(WriteFileInput),
            },
            ToolDefinition {
                name: "replace".into(),
                description: "Search and replace a specific string in a file. Fails if old_string is not found or is ambiguous.".into(),
                input_schema: schema_for!(ReplaceInput),
            },
            ToolDefinition {
                name: "list_directory".into(),
                description: "List the names of files and subdirectories within a specified path".into(),
                input_schema: schema_for!(ListDirectoryInput),
            },
            ToolDefinition {
                name: "glob".into(),
                description: "Find files matching a specific glob pattern".into(),
                input_schema: schema_for!(GlobInput),
            },
        ]
    }

    async fn run_tool(
        &mut self,
        _ctx: &mut PluginContext,
        call: &PluginToolCall,
    ) -> Result<Value, String> {
        match call.name.as_str() {
            "read_file" => {
                let input: ReadFileInput =
                    serde_json::from_value(call.arguments.clone()).map_err(|e| e.to_string())?;
                crate::tools::emit_tool_input("read_file", &json!(input));
                do_read_file(&input)
            }
            "write_file" => {
                let input: WriteFileInput =
                    serde_json::from_value(call.arguments.clone()).map_err(|e| e.to_string())?;
                crate::tools::emit_tool_input("write_file", &json!(input));
                do_write_file(&input)
            }
            "replace" => {
                let input: ReplaceInput =
                    serde_json::from_value(call.arguments.clone()).map_err(|e| e.to_string())?;
                crate::tools::emit_tool_input("replace", &json!(input));
                do_replace(&input)
            }
            "list_directory" => {
                let input: ListDirectoryInput =
                    serde_json::from_value(call.arguments.clone()).map_err(|e| e.to_string())?;
                crate::tools::emit_tool_input("list_directory", &json!(input));
                do_list_directory(&input)
            }
            "glob" => {
                let input: GlobInput =
                    serde_json::from_value(call.arguments.clone()).map_err(|e| e.to_string())?;
                crate::tools::emit_tool_input("glob", &json!(input));
                do_glob(&input)
            }
            _ => Err(format!("Unknown filesystem tool: {}", call.name)),
        }
    }
}

#[derive(JsonSchema, Deserialize, Serialize)]
struct ReadFileInput {
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
}

fn do_read_file(input: &ReadFileInput) -> Result<Value, String> {
    let validated_path = validate_path(&input.path)?;
    let content =
        fs::read_to_string(&validated_path).map_err(|e| format!("Failed to read file: {e}"))?;
    let lines: Vec<&str> = content.lines().collect();
    let start = input.start_line.unwrap_or(1).max(1);
    let end = input.end_line.unwrap_or(lines.len()).min(lines.len());

    if start > end {
        return Err("reached beyond the end of file".to_string());
    }

    let slice = lines
        .get(start.saturating_sub(1)..end)
        .ok_or("Invalid line range")?;
    let result_content = slice.join("\n");
    let redacted_content = jewels::redact(&result_content);

    Ok(json!({
        "path": input.path,
        "content": redacted_content,
        "start_line": start,
        "end_line": end,
        "total_lines": lines.len()
    }))
}

#[derive(JsonSchema, Deserialize, Serialize)]
struct WriteFileInput {
    path: String,
    content: String,
}

fn do_write_file(input: &WriteFileInput) -> Result<Value, String> {
    let validated_path = validate_path(&input.path)?;

    if let Some(parent) = validated_path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Failed to create directories: {e}"))?;
    }

    fs::write(&validated_path, &input.content).map_err(|e| format!("Failed to write file: {e}"))?;
    Ok(json!({ "status": "success", "path": input.path, "bytes": input.content.len() }))
}

#[derive(JsonSchema, Deserialize, Serialize)]
struct ReplaceInput {
    path: String,
    old_string: String,
    new_string: String,
}

fn do_replace(input: &ReplaceInput) -> Result<Value, String> {
    let validated_path = validate_path(&input.path)?;
    let content =
        fs::read_to_string(&validated_path).map_err(|e| format!("Failed to read file: {e}"))?;
    let occurrences = content.matches(&input.old_string).count();
    if occurrences == 0 {
        return Err(format!("String not found in {}", input.path));
    }
    if occurrences > 1 {
        return Err(format!(
            "String found {occurrences} times in {}. Please provide more context to make it unique.",
            input.path
        ));
    }

    let new_content = content.replace(&input.old_string, &input.new_string);
    fs::write(&validated_path, new_content).map_err(|e| format!("Failed to write file: {e}"))?;

    Ok(json!({ "status": "success", "path": input.path }))
}

#[derive(JsonSchema, Deserialize, Serialize)]
struct ListDirectoryInput {
    path: String,
}

fn do_list_directory(input: &ListDirectoryInput) -> Result<Value, String> {
    let validated_path = validate_path(&input.path)?;

    let entries =
        fs::read_dir(&validated_path).map_err(|e| format!("Failed to read directory: {e}"))?;
    let mut result = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("Error reading directory entry: {e}"))?;
        let file_name = entry.file_name().to_string_lossy().to_string();
        let file_type = entry
            .file_type()
            .map_err(|e| format!("Error reading file type: {e}"))?;
        let is_dir = file_type.is_dir();
        result.push(json!({
            "name": file_name,
            "is_directory": is_dir,
        }));
    }

    Ok(json!({ "path": input.path, "entries": result }))
}

#[derive(JsonSchema, Deserialize, Serialize)]
struct GlobInput {
    pattern: String,
}

fn do_glob(input: &GlobInput) -> Result<Value, String> {
    let mut matches = Vec::new();
    for entry in glob::glob(&input.pattern).map_err(|e| format!("Invalid glob pattern: {e}"))? {
        let path = entry.map_err(|e| format!("Glob error: {e}"))?;
        matches.push(path.to_string_lossy().to_string());
    }

    Ok(json!({ "pattern": input.pattern, "matches": matches }))
}
