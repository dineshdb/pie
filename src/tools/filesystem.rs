use aisdk::core::tools::{Tool, ToolExecute};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct ReadFileInput {
    /// Path to the file to read.
    path: String,
    /// Optional: Start line (1-indexed).
    start_line: Option<usize>,
    /// Optional: End line (1-indexed).
    end_line: Option<usize>,
}

#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct WriteFileInput {
    /// Path to the file to write.
    path: String,
    /// Complete content to write to the file.
    content: String,
}

#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct ReplaceInput {
    /// Path to the file to modify.
    path: String,
    /// Exact string to find and replace.
    old_string: String,
    /// New string to replace it with.
    new_string: String,
}

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

#[allow(clippy::unwrap_used)]
#[must_use]
pub fn read_file_tool() -> Tool {
    Tool::builder()
        .name("read_file")
        .description("Read the content of a file, optionally within a line range.")
        .input_schema(schemars::schema_for!(ReadFileInput))
        .execute(ToolExecute::from_sync(|_ctx, params| {
            super::emit_tool_input("read_file", &params);
            let path_str = params
                .get("path")
                .and_then(serde_json::Value::as_str)
                .ok_or("path is required")?;
            let path = validate_path(path_str)?;

            let content =
                fs::read_to_string(&path).map_err(|e| format!("Failed to read file: {e}"))?;
            let lines: Vec<&str> = content.lines().collect();

            let start = params
                .get("start_line")
                .and_then(serde_json::Value::as_u64)
                .map_or(1, |v| usize::try_from(v).unwrap_or(usize::MAX))
                .max(1);
            let end = params
                .get("end_line")
                .and_then(serde_json::Value::as_u64)
                .map_or(lines.len(), |v| usize::try_from(v).unwrap_or(usize::MAX))
                .min(lines.len());

            if start > end {
                return Ok(
                    json!({ "path": path_str, "content": "", "total_lines": lines.len() })
                        .to_string(),
                );
            }

            let slice = lines
                .get(start.saturating_sub(1)..end)
                .ok_or("Invalid line range")?;
            let result_content = slice.join("\n");

            Ok(json!({
                "path": path_str,
                "content": result_content,
                "start_line": start,
                "end_line": end,
                "total_lines": lines.len()
            })
            .to_string())
        }))
        .build()
        .unwrap()
}

#[allow(clippy::unwrap_used)]
#[must_use]
pub fn write_file_tool() -> Tool {
    Tool::builder()
        .name("write_file")
        .description("Write content to a file. Overwrites if it exists.")
        .input_schema(schemars::schema_for!(WriteFileInput))
        .execute(ToolExecute::from_sync(|_ctx, params| {
            super::emit_tool_input("write_file", &params);
            let path_str = params
                .get("path")
                .and_then(serde_json::Value::as_str)
                .ok_or("path is required")?;
            let content = params
                .get("content")
                .and_then(serde_json::Value::as_str)
                .ok_or("content is required")?;
            let path = validate_path(path_str)?;

            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("Failed to create directories: {e}"))?;
            }

            fs::write(&path, content).map_err(|e| format!("Failed to write file: {e}"))?;

            Ok(
                json!({ "status": "success", "path": path_str, "bytes": content.len() })
                    .to_string(),
            )
        }))
        .build()
        .unwrap()
}

#[allow(clippy::unwrap_used)]
#[must_use]
pub fn replace_tool() -> Tool {
    Tool::builder()
        .name("replace")
        .description("Search and replace a specific string in a file. Fails if old_string is not found or is ambiguous.")
        .input_schema(schemars::schema_for!(ReplaceInput))
        .execute(ToolExecute::from_sync(|_ctx, params| {
            super::emit_tool_input("replace", &params);
            let path_str = params
                .get("path")
                .and_then(serde_json::Value::as_str)
                .ok_or("path is required")?;
            let old_string = params
                .get("old_string")
                .and_then(serde_json::Value::as_str)
                .ok_or("old_string is required")?;
            let new_string = params
                .get("new_string")
                .and_then(serde_json::Value::as_str)
                .ok_or("new_string is required")?;
            let path = validate_path(path_str)?;

            let content = fs::read_to_string(&path).map_err(|e| format!("Failed to read file: {e}"))?;

            let occurrences = content.matches(old_string).count();
            if occurrences == 0 {
                return Err(format!("String not found in {path_str}"));
            }
            if occurrences > 1 {
                return Err(format!("String found {occurrences} times in {path_str}. Please provide more context to make it unique."));
            }

            let new_content = content.replace(old_string, new_string);
            fs::write(&path, new_content).map_err(|e| format!("Failed to write file: {e}"))?;

            Ok(json!({ "status": "success", "path": path_str }).to_string())
        }))
        .build()
        .unwrap()
}
