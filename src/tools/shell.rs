use agentsdk::core::tools::{Tool, ToolExecute};
use p1e_sandbox::SandboxConfig;
use serde_json::json;
use std::sync::Arc;

#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct ShellInput {
    command: String,
}

/// Execute a shell command inside the sandbox and return its stdout, stderr, and exit code.
#[allow(clippy::unwrap_used)]
pub fn shell(
    sandbox_settings: Arc<SandboxConfig>,
    pool: Arc<crate::db::DbPool>,
    session_id: String,
) -> Tool {
    Tool::builder()
        .name("shell")
        .description("Execute a system command, bash tools, clis, etc. Requires plan.")
        .input_schema(schemars::schema_for!(ShellInput))
        .execute(ToolExecute::from_async(move |_ctx, params| {
            let pool = pool.clone();
            let session_id = session_id.clone();
            let sandbox_settings = sandbox_settings.clone();
            async move {
                super::emit_tool_input("shell", &params);

                let cmd_str = params.get("command").and_then(|v| v.as_str());
                crate::tools::plan::enforce_planning(&pool, &session_id, "shell").await?;

                let Some(cmd) = cmd_str else {
                    return Err("command parameter is required".to_string());
                };
                let cmd = cmd.to_string();
                tracing::debug!(cmd = %cmd, "shell:");
                let out = super::run_sandboxed_command(&cmd, &sandbox_settings);
                tracing::debug!(exit_code = out.exit_code, stdout_len = out.stdout.len(), out = %out.stdout, "shell:");
                let result = json!({
                    "cmd": cmd,
                    "code": out.exit_code,
                    "stdout": out.stdout,
                    "stderr": out.stderr,
                });
                tracing::trace!(%result, "shell:");
                Ok(serde_json::to_string(&result).unwrap_or_default())
            }
        }))
        .build()
        .unwrap()
}
