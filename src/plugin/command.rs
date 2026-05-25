use crate::instructions::Instructions;
use crate::registry::Registry;
use agentsdk::PluginTools;
use agentsdk::core::history::History;
use agentsdk::core::messages::{
    self, ChatCompletionRequestAssistantMessage, ChatCompletionRequestAssistantMessageRole,
    Message, ToolCall, ToolCallType, ToolFunction,
};
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

    async fn on_user_message(&mut self, ctx: &mut PluginContext, text: String) -> String {
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

        if !parts.is_empty() {
            let instructions_text = parts.join("---");
            if let Some(mut history) = ctx.get_mut::<History>() {
                let call_id = chrono::Utc::now()
                    .timestamp_nanos_opt()
                    .unwrap_or(0)
                    .to_string();
                history.0.push(Message::AssistantMessage(
                    ChatCompletionRequestAssistantMessage {
                        content: None,
                        name: None,
                        tool_calls: Some(vec![ToolCall {
                            id: call_id.clone(),
                            r#type: ToolCallType::Function,
                            function: ToolFunction {
                                name: "command__load".into(),
                                arguments: "{}".into(),
                            },
                        }]),
                        role: ChatCompletionRequestAssistantMessageRole::Assistant,
                        function_call: None,
                    },
                ));
                history.0.push(messages::tool(instructions_text, call_id));
            }
        }

        text
    }
}
