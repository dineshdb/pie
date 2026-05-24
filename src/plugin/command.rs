use crate::instructions::Instructions;
use crate::registry::Registry;
use agentsdk::PluginTools;
use agentsdk::core::plugin::{AgentPlugin, PluginContext, PluginToolCall};
use agentsdk::core::tools::ToolDefinition;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

#[derive(PluginTools, Serialize, Deserialize)]
enum CommandTools {
    /// Load instructions for a specific user command
    MentionedCommand(MentionedCommand),
}

#[derive(JsonSchema, Deserialize, Serialize)]
struct MentionedCommand {
    /// The name of the user command to load instructions for
    command: String,
}

pub struct UserCommandPlugin {
    registry: Arc<Registry>,
    current_command: Option<String>,
}

impl UserCommandPlugin {
    pub fn new(registry: Arc<Registry>, current_command: Option<String>) -> Self {
        Self {
            registry,
            current_command,
        }
    }
}

#[async_trait]
impl AgentPlugin for UserCommandPlugin {
    fn name(&self) -> &'static str {
        "cmd"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        CommandTools::definitions()
    }

    async fn run_tool(
        &mut self,
        _ctx: &mut PluginContext,
        call: &PluginToolCall,
    ) -> Result<Value, String> {
        match CommandTools::from_call(call)? {
            CommandTools::MentionedCommand(input) => {
                if let Some(agent) = self
                    .registry
                    .agents
                    .iter()
                    .find(|a| a.name == input.command)
                {
                    Ok(serde_json::json!(agent.content))
                } else {
                    Err(format!("Command not found: {}", input.command))
                }
            }
        }
    }

    async fn on_user_message(&mut self, _ctx: &mut PluginContext, text: String) -> String {
        let instructions = Instructions::new(&text);
        if instructions.mentions.is_empty() {
            return text;
        }

        let mut parts: Vec<String> = Vec::new();
        for mention in &instructions.mentions {
            if self.current_command.as_ref() == Some(mention) {
                continue;
            }
            if let Some(agent) = self.registry.agents.iter().find(|a| a.name == *mention) {
                tracing::debug!(command = %agent.name, "Injecting command instructions");
                parts.push(agent.content.clone());
            }
        }

        if parts.is_empty() {
            text
        } else {
            let prefix = parts.join("\n\n");
            format!("{prefix}\n\n{text}")
        }
    }
}
