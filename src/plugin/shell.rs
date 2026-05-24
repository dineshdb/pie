use agentsdk::PluginTools;
use agentsdk::core::plugin::{AgentPlugin, PluginContext, PluginToolCall};
use agentsdk::core::sandbox::Sandbox;
use agentsdk::core::tools::ToolDefinition;
use anyhow::Result;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Default, Clone)]
pub struct ShellPlugin;

impl ShellPlugin {
    pub fn new() -> Self {
        Self
    }
}

#[derive(PluginTools, Serialize, Deserialize)]
enum ShellTools {
    /// Execute a system command, bash tools, clis, etc
    Execute(ShellInput),
}

#[async_trait]
impl AgentPlugin for ShellPlugin {
    fn name(&self) -> &'static str {
        "shell"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        ShellTools::definitions()
    }

    async fn run_tool(
        &mut self,
        ctx: &mut PluginContext,
        call: &PluginToolCall,
    ) -> Result<Value, String> {
        match ShellTools::from_call(call)? {
            ShellTools::Execute(input) => {
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
        }
    }
}

#[derive(JsonSchema, Deserialize, Serialize)]
struct ShellInput {
    /// The system command to execute in the sandbox.
    command: String,
}
