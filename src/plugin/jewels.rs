use agentsdk::core::history::History;
use agentsdk::core::messages::Message;
use agentsdk::openai::api::ChatCompletionRequestUserMessageContent;
use agentsdk::{AgentPlugin, Messages, PluginContext, PostToolAction, PreToolAction};
use async_trait::async_trait;
use std::borrow::Cow;

#[derive(Debug, Default)]
pub struct JewelsPlugin;

impl JewelsPlugin {
    pub fn new() -> Self {
        Self
    }

    pub fn redact(text: &str) -> String {
        jewels::redact(text)
    }

    fn redact_history(history: &mut Messages) {
        for msg in history.iter_mut() {
            match msg {
                Message::UserMessage(u) => {
                    if let Some(ChatCompletionRequestUserMessageContent::String(s)) = &mut u.content
                    {
                        *s = jewels::redact(s);
                    }
                }
                Message::ToolMessage(t) => {
                    if let Some(content) = &mut t.content {
                        *content = jewels::redact(content);
                    }
                }
                // we don't redact assistant messages or system messages per instruction
                _ => {}
            }
        }
    }
}

#[async_trait]
impl AgentPlugin for JewelsPlugin {
    fn name(&self) -> &'static str {
        "jewels"
    }

    async fn init(&mut self, ctx: &mut PluginContext) {
        if let Some(mut history) = ctx.get_mut::<History>() {
            Self::redact_history(&mut history.0);
        }
    }

    async fn on_user_message(&mut self, _ctx: &mut PluginContext, text: String) -> String {
        jewels::redact(&text)
    }

    async fn prepare_system_prompt(
        &mut self,
        ctx: &mut PluginContext,
        _history: &Messages,
    ) -> Option<Cow<'static, str>> {
        // Redact history before every iteration in case new messages were added
        // (e.g. tool results or user rejections)
        if let Some(mut history) = ctx.get_mut::<History>() {
            Self::redact_history(&mut history.0);
        }
        None
    }

    async fn on_tool_pre_execute(
        &mut self,
        _ctx: &mut PluginContext,
        _id: &str,
        _name: &str,
        args: &serde_json::Value,
    ) -> PreToolAction {
        let redacted = jewels::redact_json(args);
        if redacted == *args {
            PreToolAction::Continue(None)
        } else {
            PreToolAction::Continue(Some(redacted))
        }
    }

    async fn on_tool_post_execute(
        &mut self,
        _ctx: &mut PluginContext,
        _id: &str,
        _name: &str,
        result: &serde_json::Value,
    ) -> PostToolAction {
        let redacted = jewels::redact_json(result);
        if redacted == *result {
            PostToolAction::Continue(None)
        } else {
            PostToolAction::Continue(Some(redacted))
        }
    }
}
