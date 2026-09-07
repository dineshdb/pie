//! TUI display formatting for tool call results.
//!
//! The request line (`ToolName{key = value}`) already shows the arguments,
//! so a result line must not echo them back. The JSON results that repeat
//! their arguments (`Bash`'s `cmd`, `Glob`'s `pattern`, `Read`/`Write`/
//! `Edit`'s `path`) are parsed and reduced to what is new: exit code,
//! output, counts. Anything else falls through to the raw truncated text.

use crate::ui::tui::widgets::truncate_str;
use serde_json::Value;
use std::fmt;

/// How much of a result's payload (stdout, content, match list) a line
/// shows. Arguments live on the request line; the result line carries the
/// answer's headline, not the whole answer.
const PREVIEW_LIMIT: usize = 100;

/// Parsed tool call result for display in the TUI.
pub enum ToolCallResult<'a> {
    /// `Bash`: `{cmd, code, stdout, stderr}` — `cmd` repeats the request's
    /// command and is dropped.
    Shell {
        exit_code: i32,
        stdout: String,
        stderr: String,
    },
    /// `Glob`: `{pattern, matches}` — `pattern` is dropped.
    Glob {
        matches: Vec<String>,
    },
    /// `Read`: `{path, content, start_line, end_line, total_lines}` — the
    /// line range is the news; the path is on the request line.
    Read {
        start_line: usize,
        end_line: usize,
        total_lines: usize,
        content: String,
    },
    /// `Write`/`Edit`: `{status, path[, bytes]}` — the path is dropped.
    Saved {
        status: String,
        bytes: Option<usize>,
    },
    /// A call that errored. `message` is the error without its `Error: `
    /// prefix — the standalone error spelling would repeat the word.
    Failed {
        message: String,
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
    pub fn new<'a>(name: &str, output: &'a str, failed: bool) -> ToolCallResult<'a> {
        if failed {
            return ToolCallResult::Failed {
                message: output.strip_prefix("Error: ").unwrap_or(output).to_string(),
            };
        }
        match name {
            "Bash" => parse_shell_output(output),
            "Glob" => parse_glob_output(output),
            "Read" => parse_read_output(output),
            "Write" | "Edit" => parse_saved_output(output),
            "skills__load_skills" => ToolCallResult::LoadSkills,
            "skills__load_references" => ToolCallResult::LoadReferences,
            _ => ToolCallResult::Other { output },
        }
    }
}

fn parse_shell_output(output: &str) -> ToolCallResult<'_> {
    let Some(obj) = parse_json_object(output) else {
        return ToolCallResult::Shell {
            exit_code: 0,
            stdout: output.to_string(),
            stderr: String::new(),
        };
    };

    ToolCallResult::Shell {
        #[allow(clippy::cast_possible_truncation)]
        exit_code: obj.get("code").and_then(Value::as_i64).unwrap_or(0) as i32,
        stdout: str_field(&obj, "stdout"),
        stderr: str_field(&obj, "stderr"),
    }
}

fn parse_glob_output(output: &str) -> ToolCallResult<'_> {
    match parse_json_object(output) {
        Some(obj) => ToolCallResult::Glob {
            matches: obj
                .get("matches")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
        },
        None => ToolCallResult::Other { output },
    }
}

fn parse_read_output(output: &str) -> ToolCallResult<'_> {
    match parse_json_object(output) {
        Some(obj) => ToolCallResult::Read {
            start_line: num_field(&obj, "start_line"),
            end_line: num_field(&obj, "end_line"),
            total_lines: num_field(&obj, "total_lines"),
            content: str_field(&obj, "content"),
        },
        None => ToolCallResult::Other { output },
    }
}

fn parse_saved_output(output: &str) -> ToolCallResult<'_> {
    match parse_json_object(output) {
        Some(obj) => ToolCallResult::Saved {
            status: str_field(&obj, "status"),
            #[allow(clippy::cast_possible_truncation)]
            bytes: obj.get("bytes").and_then(Value::as_u64).map(|b| b as usize),
        },
        None => ToolCallResult::Other { output },
    }
}

/// Parse the output as a JSON object; `None` when it is anything else
/// (plain text, an error string, a JSON array).
fn parse_json_object(output: &str) -> Option<serde_json::Map<String, Value>> {
    serde_json::from_str::<Value>(output)
        .ok()?
        .as_object()
        .cloned()
}

fn str_field(obj: &serde_json::Map<String, Value>, key: &str) -> String {
    obj.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

#[allow(clippy::cast_possible_truncation)]
fn num_field(obj: &serde_json::Map<String, Value>, key: &str) -> usize {
    obj.get(key).and_then(Value::as_u64).unwrap_or(0) as usize
}

/// Collapse a payload into one display line — newlines re-escaped like the
/// request line's arguments — and preview it.
fn one_line(text: &str, limit: usize) -> String {
    let flat = text.replace('\n', "\\n").replace('\r', "\\r");
    truncate_str(&flat, limit)
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
                    write!(f, " │ {}", one_line(output, PREVIEW_LIMIT))?;
                }
                Ok(())
            }
            ToolCallResult::Glob { matches } if matches.is_empty() => {
                write!(f, "no matches")
            }
            ToolCallResult::Glob { matches } => write!(
                f,
                "{} matches │ {}",
                matches.len(),
                one_line(&matches.join(", "), PREVIEW_LIMIT)
            ),
            ToolCallResult::Read {
                start_line,
                end_line,
                total_lines,
                content,
            } => {
                write!(f, "lines {start_line}-{end_line} of {total_lines}")?;
                if !content.is_empty() {
                    write!(f, " │ {}", one_line(content, PREVIEW_LIMIT))?;
                }
                Ok(())
            }
            ToolCallResult::Saved { status, bytes } => match bytes {
                Some(n) => write!(f, "{status} · {n} bytes"),
                None => write!(f, "{status}"),
            },
            ToolCallResult::Failed { message } => {
                write!(f, "failed │ {}", one_line(message, PREVIEW_LIMIT))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_result_drops_the_command_echo() {
        let out = r#"{"cmd":"ls -la ~/.pie/","code":1,"stdout":"total 368","stderr":""}"#;
        assert_eq!(
            ToolCallResult::new("Bash", out, false).to_string(),
            "exit 1 │ total 368"
        );
    }

    #[test]
    fn bash_stderr_shown_when_stdout_empty_and_non_json_falls_through() {
        let out = r#"{"cmd":"x","code":2,"stdout":"","stderr":"No such file"}"#;
        assert_eq!(
            ToolCallResult::new("Bash", out, false).to_string(),
            "exit 2 │ No such file"
        );
        assert_eq!(
            ToolCallResult::new("Bash", "plain text", false).to_string(),
            "exit 0 │ plain text"
        );
    }

    #[test]
    fn glob_result_drops_the_pattern() {
        let out = r#"{"pattern":"**/*.rs","matches":["a.rs","b.rs"]}"#;
        assert_eq!(
            ToolCallResult::new("Glob", out, false).to_string(),
            "2 matches │ a.rs, b.rs"
        );
        assert_eq!(
            ToolCallResult::new("Glob", r#"{"pattern":"x","matches":[]}"#, false).to_string(),
            "no matches"
        );
    }

    #[test]
    fn read_result_shows_range_and_flattened_preview_without_path() {
        let out = r#"{"path":"a.rs","content":"let x = 1;\nlet y = 2;","start_line":3,"end_line":4,"total_lines":80}"#;
        assert_eq!(
            ToolCallResult::new("Read", out, false).to_string(),
            "lines 3-4 of 80 │ let x = 1;\\nlet y = 2;"
        );
    }

    #[test]
    fn write_and_edit_results_drop_the_path() {
        let out = r#"{"status":"success","path":"f.rs","bytes":120}"#;
        assert_eq!(
            ToolCallResult::new("Write", out, false).to_string(),
            "success · 120 bytes"
        );
        let out = r#"{"status":"success","path":"f.rs"}"#;
        assert_eq!(
            ToolCallResult::new("Edit", out, false).to_string(),
            "success"
        );
    }

    #[test]
    fn failed_call_renders_as_failure_without_the_error_prefix() {
        assert_eq!(
            ToolCallResult::new("Read", "Error: path not allowed for reading", true).to_string(),
            "failed │ path not allowed for reading"
        );
    }

    #[test]
    fn long_payloads_are_cut_on_one_line() {
        let out = format!(r#"{{"pattern":"x","matches":["{}"]}}"#, "m".repeat(300));
        let line = ToolCallResult::new("Glob", &out, false).to_string();
        assert!(line.starts_with("1 matches │ "), "{line}");
        assert!(
            line.chars().count() <= PREVIEW_LIMIT + "1 matches │ ".len() + 1,
            "{line}"
        );
    }

    #[test]
    fn unknown_tools_keep_their_raw_output() {
        assert_eq!(
            ToolCallResult::new("mem__search", r#"{"hits":[]}"#, false).to_string(),
            r#"{"hits":[]}"#
        );
    }
}
