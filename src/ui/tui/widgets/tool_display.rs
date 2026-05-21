//! TUI display formatting for tool call results.

use crate::ui::tui::widgets::truncate_str;
use std::fmt;

/// Parsed tool call result for display in the TUI.
pub enum ToolCallResult<'a> {
    Shell {
        exit_code: i32,
        stdout: String,
        stderr: String,
    },
    LoadSkills,
    LoadReferences,
    /// Fallback for unknown tools — show truncated output.
    Other {
        output: &'a str,
    },
}

impl ToolCallResult<'_> {
    /// Parse from stream event fields.
    pub fn new<'a>(name: &str, output: &'a str) -> ToolCallResult<'a> {
        match name {
            "shell" => parse_shell_output(output),
            "skills__load_skills" => ToolCallResult::LoadSkills,
            "skills__load_references" => ToolCallResult::LoadReferences,
            _ => ToolCallResult::Other { output },
        }
    }
}

fn parse_shell_output(output: &str) -> ToolCallResult<'static> {
    let val = serde_json::from_str::<serde_json::Value>(output).ok();
    let obj = val.as_ref().and_then(|v| v.as_object());

    let Some(obj) = obj else {
        return ToolCallResult::Shell {
            exit_code: 0,
            stdout: output.to_string(),
            stderr: String::new(),
        };
    };

    let exit_code = obj
        .get("code")
        .and_then(serde_json::Value::as_i64)
        .map_or(0, |v| {
            #[allow(clippy::cast_possible_truncation)]
            {
                v as i32
            }
        });
    let stdout = obj
        .get("stdout")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    let stderr = obj
        .get("stderr")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    ToolCallResult::Shell {
        exit_code,
        stdout,
        stderr,
    }
}

impl fmt::Display for ToolCallResult<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ToolCallResult::Shell {
                exit_code,
                stdout,
                stderr,
            } => {
                let output = if stdout.is_empty() {
                    stderr.as_str()
                } else {
                    stdout.as_str()
                };
                write!(f, "exit {exit_code}")?;
                if !output.is_empty() {
                    let truncated = truncate_str(output, 100);
                    write!(f, " │ {truncated}")?;
                }
                Ok(())
            }
            ToolCallResult::LoadSkills | ToolCallResult::LoadReferences => Ok(()),
            ToolCallResult::Other { output } => {
                let truncated = truncate_str(output, 120);
                if !truncated.is_empty() {
                    write!(f, "{truncated}")?;
                }
                Ok(())
            }
        }
    }
}
