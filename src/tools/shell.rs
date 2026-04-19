use crate::sandbox;
use aisdk::core::tools::{Tool, ToolExecute};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct ShellInput {
    cmd: String,
}

/// Execute a shell command inside the sandbox and return its stdout, stderr, and exit code.
#[allow(clippy::unwrap_used)]
pub fn shell_tool(sandbox_settings: PathBuf) -> Tool {
    let sandbox_settings = Arc::new(sandbox_settings);
    Tool::builder()
        .name("shell_tool")
        .description("Execute a shell command and return its stdout, stderr, and exit code.")
        .input_schema(schemars::schema_for!(ShellInput))
        .execute(ToolExecute::from_sync(move |_ctx, params| {
            let Some(cmd) = params.get("cmd").and_then(|v| v.as_str()) else {
                return Err("cmd parameter is required".to_string());
            };
            let cmd = cmd.to_string();
            tracing::debug!(cmd = %cmd, "shell:");
            let output = sandbox::build_command(&cmd, &sandbox_settings)
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("PAGER", "cat")
                .env("EDITOR", "true")
                .output();
            let result = match output {
                Ok(out) => {
                    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                    let exit_code = out.status.code().unwrap_or(-1);
                    tracing::debug!(exit_code, stdout_len = stdout.len(), out = %stdout, "shell:");
                    json!({
                        "cmd": cmd,
                        "exitCode": exit_code,
                        "stdout": stdout,
                        "stderr": stderr,
                        "success": exit_code == 0
                    })
                }
                Err(e) => {
                    tracing::debug!(error = %e, "shell_tool failed");
                    json!({
                        "cmd": cmd,
                        "exitCode": -1,
                        "stdout": "",
                        "stderr": e.to_string(),
                        "success": false
                    })
                }
            };
            tracing::trace!(%result, "shell:");
            Ok(serde_json::to_string(&result).unwrap_or_default())
        }))
        .build()
        .unwrap()
}
