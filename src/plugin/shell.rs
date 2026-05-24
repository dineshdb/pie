use agentsdk::core::plugin::{AgentPlugin, PluginContext, PluginToolCall};
use agentsdk::core::sandbox::Sandbox;
use agentsdk::core::tools::ToolDefinition;
use anyhow::Result;
use async_trait::async_trait;
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Default, Clone)]
pub struct ShellPlugin;

impl ShellPlugin {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl AgentPlugin for ShellPlugin {
    fn name(&self) -> &'static str {
        "shell"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "execute".into(),
            description: "Execute a system command, bash tools, clis, etc".into(),
            input_schema: schema_for!(ShellInput),
        }]
    }

    async fn run_tool(
        &mut self,
        ctx: &mut PluginContext,
        call: &PluginToolCall,
    ) -> Result<Value, String> {
        match call.name.as_str() {
            "execute" => {
                let input: ShellInput =
                    serde_json::from_value(call.arguments.clone()).map_err(|e| e.to_string())?;

                let sandbox = ctx.get::<Sandbox>().ok_or("No sandbox registered")?;

                let out = sandbox
                    .0
                    .exec(&input.command)
                    .await
                    .map_err(|e| e.to_string())?;

                Ok(json!({
                    "cmd": input.command,
                    "code": out.exit_code,
                    "stdout": out.stdout,
                    "stderr": out.stderr,
                }))
            }
            _ => Err(format!("Unknown tool: {}", call.name)),
        }
    }
}

#[derive(JsonSchema, Deserialize, Serialize)]
struct ShellInput {
    /// The system command to execute in the sandbox.
    command: String,
}
