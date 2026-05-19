use agentsdk::core::tools::{Tool, ToolDefinition, ToolExecute};
use p1e_sandbox::SandboxConfig;
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;

#[derive(JsonSchema, Deserialize, Serialize)]
struct ShellInput {
    /// The system command to execute in the sandbox.
    command: String,
}

/// Execute a system command, bash tools, clis, etc. Requires plan.
pub fn shell() -> anyhow::Result<Tool> {
    Ok(Tool::builder()
        .definition(
            ToolDefinition::builder()
                .name("shell")
                .description("Execute a system command, bash tools, clis, etcs")
                .input_schema(schema_for!(ShellInput))
                .build()?,
        )
        .execute(ToolExecute::from_async(|ctx, params| async move {
            let input: ShellInput = serde_json::from_value(params).map_err(|e| e.to_string())?;
            let sandbox = ctx
                .options
                .extensions
                .get::<Arc<SandboxConfig>>()
                .ok_or_else(|| "SandboxConfig not found in extensions".to_string())?;

            super::emit_tool_input("shell", &json!(input));

            tracing::debug!(cmd = %input.command, "shell:");
            let out = super::run_sandboxed_command(&input.command, &sandbox);
            tracing::debug!(exit_code = out.exit_code, stdout_len = out.stdout.len(), out = %out.stdout, "shell:");

            let result = json!({
                "cmd": input.command,
                "code": out.exit_code,
                "stdout": out.stdout,
                "stderr": out.stderr,
            });
            tracing::trace!(%result, "shell:");
            Ok(result)
        }))
        .build()?)
}
