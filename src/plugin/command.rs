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
        text
    }

    async fn prepare_history(
        &mut self,
        _ctx: &mut PluginContext,
        history: &mut agentsdk::core::messages::Messages,
    ) {
        let Some(msg) = history.last() else {
            return;
        };
        let Some(text) = agentsdk::core::messages::extract_user_text(msg) else {
            return;
        };

        let instructions = Instructions::new(&text);
        if instructions.mentions.is_empty() {
            return;
        }

        for mention in &instructions.mentions {
            if self.current_command.as_ref() == Some(mention) {
                continue;
            }
            if let Some(agent) = self.registry.agents.iter().find(|a| a.name == *mention) {
                let call_id = format!("inject_{}", agent.name);
                let func_name = "cmd__mentioned_command".to_string();

                history.push(agentsdk::core::messages::assistant_tool_call(
                    func_name,
                    call_id.clone(),
                    &serde_json::json!({"command": agent.name}),
                ));

                history.push(agentsdk::core::messages::tool(
                    agent.content.clone(),
                    call_id,
                ));
            }
        }
    }
}
