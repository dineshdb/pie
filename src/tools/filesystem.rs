use agentsdk::core::tools::{Tool, ToolDefinition, ToolExecute};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};

fn validate_path(path: &str) -> Result<PathBuf, String> {
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

#[derive(JsonSchema, Deserialize, Serialize)]
struct ReadFileInput {
    /// The path to the file to read.
    path: String,
    /// Optional: The 1-based line number to start reading from.
    start_line: Option<usize>,
    /// Optional: The 1-based line number to end reading at (inclusive).
    end_line: Option<usize>,
}

/// Read the content of a file.
pub fn read_file_tool() -> anyhow::Result<Tool> {
    let schema = schemars::schema_for!(ReadFileInput);
    Ok(Tool::builder()
        .definition(
            ToolDefinition::builder()
                .name("read_file")
                .description("Read the content of a file")
                .input_schema(schema)
                .build()?,
        )
        .execute(ToolExecute::from_sync(|_ctx, params| {
            let input: ReadFileInput = serde_json::from_value(params).map_err(|e| e.to_string())?;
            super::emit_tool_input("read_file", &json!(input));

            let validated_path = validate_path(&input.path)?;
            let content = fs::read_to_string(&validated_path)
                .map_err(|e| format!("Failed to read file: {e}"))?;
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

            Ok(json!({
                "path": input.path,
                "content": result_content,
                "start_line": start,
                "end_line": end,
                "total_lines": lines.len()
            }))
        }))
        .build()?)
}

#[derive(JsonSchema, Deserialize, Serialize)]
struct WriteFileInput {
    /// The path to the file to write.
    path: String,
    /// The complete content to write to the file.
    content: String,
}

/// Write content to a file. Overwrites if it exists. Requires plan.
pub fn write_file_tool() -> anyhow::Result<Tool> {
    Ok(Tool::builder()
        .definition(
            ToolDefinition::builder()
                .name("write_file")
                .description("Write content to a file. Overwrites if it exists. Requires plan")
                .input_schema(schema_for!(WriteFileInput))
                .build()?,
        )
        .execute(ToolExecute::from_async(|_ctx, params| async move {
            let input: WriteFileInput =
                serde_json::from_value(params).map_err(|e| e.to_string())?;
            super::emit_tool_input("write_file", &json!(input));

            let validated_path = validate_path(&input.path)?;

            if let Some(parent) = validated_path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("Failed to create directories: {e}"))?;
            }

            fs::write(&validated_path, &input.content)
                .map_err(|e| format!("Failed to write file: {e}"))?;
            Ok(json!({ "status": "success", "path": input.path, "bytes": input.content.len() }))
        }))
        .build()?)
}

#[derive(JsonSchema, Deserialize, Serialize)]
struct ReplaceInput {
    /// The path to the file to modify.
    path: String,
    /// The exact literal text to replace.
    old_string: String,
    /// The exact literal text to replace it with.
    new_string: String,
}

/// Search and replace a specific string in a file. Fails if `old_string` is not found or is ambiguous. Requires plan.
pub fn replace_tool() -> anyhow::Result<Tool> {
    Ok(Tool::builder()
        .definition(
            ToolDefinition::builder()
                .name("replace")
                .description("Search and replace a specific string in a file. Fails if old_string is not found or is ambiguous.")
                .input_schema(schema_for!(ReplaceInput))
                .build()?,
        )
        .execute(ToolExecute::from_async(|_ctx, params| async move {
            let input: ReplaceInput = serde_json::from_value(params).map_err(|e| e.to_string())?;
            super::emit_tool_input("replace", &json!(input));

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
        }))
        .build()?)
}

#[derive(JsonSchema, Deserialize, Serialize)]
struct ListDirectoryInput {
    /// The path of the directory to list.
    path: String,
}

/// List the names of files and subdirectories within a specified directory path.
pub fn list_directory_tool() -> anyhow::Result<Tool> {
    Ok(Tool::builder()
        .definition(
            ToolDefinition::builder()
                .name("list_directory")
                .description("List the names of files and subdirectories within a specified path")
                .input_schema(schema_for!(ListDirectoryInput))
                .build()?,
        )
        .execute(ToolExecute::from_sync(|_ctx, params| {
            let input: ListDirectoryInput =
                serde_json::from_value(params).map_err(|e| e.to_string())?;
            super::emit_tool_input("list_directory", &json!(input));

            let validated_path = validate_path(&input.path)?;

            let entries = fs::read_dir(&validated_path)
                .map_err(|e| format!("Failed to read directory: {e}"))?;
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
        }))
        .build()?)
}

#[derive(JsonSchema, Deserialize, Serialize)]
struct GlobInput {
    /// The glob pattern to match against (e.g., `src/**/*.rs`, `**/*.md`).
    pattern: String,
}

/// Find files matching a specific glob pattern.
pub fn glob_tool() -> anyhow::Result<Tool> {
    Ok(Tool::builder()
        .definition(
            ToolDefinition::builder()
                .name("glob")
                .description("Find files matching a specific glob pattern")
                .input_schema(schema_for!(GlobInput))
                .build()?,
        )
        .execute(ToolExecute::from_sync(|_ctx, params| {
            let input: GlobInput = serde_json::from_value(params).map_err(|e| e.to_string())?;
            super::emit_tool_input("glob", &json!(input));

            let mut matches = Vec::new();
            for entry in
                glob::glob(&input.pattern).map_err(|e| format!("Invalid glob pattern: {e}"))?
            {
                let path = entry.map_err(|e| format!("Glob error: {e}"))?;
                matches.push(path.to_string_lossy().to_string());
            }

            Ok(json!({ "pattern": input.pattern, "matches": matches }))
        }))
        .build()?)
}
