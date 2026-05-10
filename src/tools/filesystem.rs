use agentsdk::core::tools::{Tool, ToolExecute};
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

#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct ListDirectoryInput {
    /// Path to the directory to list.
    path: String,
}

#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct GlobInput {
    /// The glob pattern to match against (e.g., '**/*.rs', 'src/*.ts').
    pattern: String,
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
            let input: ReadFileInput =
                serde_json::from_value(params).map_err(|e| format!("Invalid input: {e}"))?;
            super::emit_tool_input(
                "read_file",
                &serde_json::to_value(&input).unwrap_or_default(),
            );

            let path = validate_path(&input.path)?;
            let content =
                fs::read_to_string(&path).map_err(|e| format!("Failed to read file: {e}"))?;
            let lines: Vec<&str> = content.lines().collect();
            let start = input.start_line.unwrap_or(1).max(1);
            let end = input.end_line.unwrap_or(lines.len()).min(lines.len());

            if start > end {
                return Err(json!({ "error": "reached beyond the end of file"}).to_string());
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
        .description("Write content to a file. Overwrites if it exists. Requires plan.")
        .input_schema(schemars::schema_for!(WriteFileInput))
        .execute(ToolExecute::from_async(move |_ctx, params| async move {
            super::emit_tool_input("write_file", &params);
            let input: WriteFileInput =
                serde_json::from_value(params).map_err(|e| format!("Invalid input: {e}"))?;

            let path = validate_path(&input.path)?;

            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("Failed to create directories: {e}"))?;
            }

            fs::write(&path, &input.content).map_err(|e| format!("Failed to write file: {e}"))?;
            Ok(
                json!({ "status": "success", "path": input.path, "bytes": input.content.len() })
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
        .description("Search and replace a specific string in a file. Fails if old_string is not found or is ambiguous. Requires plan.")
        .input_schema(schemars::schema_for!(ReplaceInput))
        .execute(ToolExecute::from_async(move |_ctx, params| {
            async move {
                super::emit_tool_input("replace", &params);

                let input: ReplaceInput =
                    serde_json::from_value(params).map_err(|e| format!("Invalid input: {e}"))?;

                let path = validate_path(&input.path)?;
                let content =
                    fs::read_to_string(&path).map_err(|e| format!("Failed to read file: {e}"))?;
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
                fs::write(&path, new_content).map_err(|e| format!("Failed to write file: {e}"))?;

                Ok(json!({ "status": "success", "path": input.path }).to_string())
            }
        }))
        .build()
        .unwrap()
}

#[allow(clippy::unwrap_used)]
#[must_use]
pub fn list_directory_tool() -> Tool {
    Tool::builder()
        .name("list_directory")
        .description(
            "List the names of files and subdirectories within a specified directory path.",
        )
        .input_schema(schemars::schema_for!(ListDirectoryInput))
        .execute(ToolExecute::from_sync(|_ctx, params| {
            let input: ListDirectoryInput =
                serde_json::from_value(params).map_err(|e| format!("Invalid input: {e}"))?;
            super::emit_tool_input(
                "list_directory",
                &serde_json::to_value(&input).unwrap_or_default(),
            );

            let path = validate_path(&input.path)?;

            let entries =
                fs::read_dir(&path).map_err(|e| format!("Failed to read directory: {e}"))?;
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

            Ok(json!({ "path": input.path, "entries": result }).to_string())
        }))
        .build()
        .unwrap()
}

#[allow(clippy::unwrap_used)]
#[must_use]
pub fn glob_tool() -> Tool {
    Tool::builder()
        .name("glob")
        .description("Find files matching a specific glob pattern.")
        .input_schema(schemars::schema_for!(GlobInput))
        .execute(ToolExecute::from_sync(|_ctx, params| {
            super::emit_tool_input("glob", &params);
            let input: GlobInput =
                serde_json::from_value(params).map_err(|e| format!("Invalid input: {e}"))?;

            let mut matches = Vec::new();
            for entry in
                glob::glob(&input.pattern).map_err(|e| format!("Invalid glob pattern: {e}"))?
            {
                let path = entry.map_err(|e| format!("Glob error: {e}"))?;
                matches.push(path.to_string_lossy().to_string());
            }

            Ok(json!({ "pattern": input.pattern, "matches": matches }).to_string())
        }))
        .build()
        .unwrap()
}
