use crate::tools::tasks::SharedTaskList;
use agentsdk::core::tools::{Tool, ToolExecute};
use p1e_srt::{SandboxConfig, build_command};
use serde_json::json;
use std::sync::Arc;

#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct ShellInput {
    cmd: String,
}

/// Execute a shell command inside the sandbox and return its stdout, stderr, and exit code.
#[allow(clippy::unwrap_used)]
pub fn shell(sandbox_settings: Arc<SandboxConfig>, state: SharedTaskList) -> Tool {
    Tool::builder()
        .name("shell")
        .description("Execute a system command, bash tools, clis, etc. Requires task plan.")
        .input_schema(schemars::schema_for!(ShellInput))
        .execute(ToolExecute::from_sync(move |_ctx, params| {
            super::emit_tool_input("shell", &params);

            {
                let guard = super::safe_lock(&state);
                guard.enforce_planning("shell")?;
            }

            let Some(cmd) = params.get("cmd").and_then(|v| v.as_str()) else {
                return Err("cmd parameter is required".to_string());
            };
            let cmd = cmd.to_string();
            tracing::debug!(cmd = %cmd, "shell:");
            let output = build_command(&cmd, &sandbox_settings)
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("PAGER", "cat")
                .env("EDITOR", "true")
                .output();
            let (stdout, stderr, exit_code) = match output {
                Ok(out) => {
                    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                    let exit_code = out.status.code().unwrap_or(-1);
                    tracing::debug!(exit_code, stdout_len = stdout.len(), out = %stdout, "shell:");
                    (stdout, stderr, exit_code)
                }
                Err(e) => {
                    tracing::debug!(error = %e, "shell failed");
                    (String::new(), e.to_string(), -1)
                }
            };
            let result = json!({
                "cmd": cmd,
                "code": exit_code,
                "stdout": stdout,
                "stderr": stderr,
            });
            tracing::trace!(%result, "shell:");
            Ok(serde_json::to_string(&result).unwrap_or_default())
        }))
        .build()
        .unwrap()
}
