use crate::agent::OutputMode;
use agentsdk::{AgentPlugin, Messages, PluginContext};
use async_trait::async_trait;
use std::borrow::Cow;

#[derive(Debug)]
pub struct ConversationModePlugin {
    output_mode: OutputMode,
}

impl ConversationModePlugin {
    pub fn new(output_mode: OutputMode) -> Self {
        Self { output_mode }
    }
}

#[async_trait]
impl AgentPlugin for ConversationModePlugin {
    fn name(&self) -> &'static str {
        "conversation_mode"
    }

    async fn prepare_system_prompt(
        &mut self,
        _ctx: &mut PluginContext,
        _history: &Messages,
    ) -> Option<Cow<'static, str>> {
        Some(Cow::Owned(self.output_mode.prompt()))
    }
}
