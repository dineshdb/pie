use agentsdk::core::history::History;
use agentsdk::core::messages::{Message, Messages};
use agentsdk::openai::api::ChatCompletionRequestUserMessageContent;
use agentsdk::{AgentPlugin, AgentSdkError, PluginContext, PostToolAction, PreToolAction};
use async_trait::async_trait;
use std::borrow::Cow;

#[derive(Debug, Default)]
pub struct JewelsPlugin {
    last_redacted_idx: usize,
}

impl JewelsPlugin {
    pub fn new() -> Self {
        Self {
            last_redacted_idx: 0,
        }
    }

    pub fn redact(text: &str) -> String {
        jewels::redact(text).into_owned()
    }

    fn redact_history(&mut self, history: &mut Messages) {
        for msg in history.iter_mut().skip(self.last_redacted_idx) {
            match msg {
                Message::UserMessage(u) => {
                    if let Some(ChatCompletionRequestUserMessageContent::String(s)) = &mut u.content
                        && let Cow::Owned(redacted) = jewels::redact(s)
                    {
                        *s = redacted;
                    }
                }
                Message::ToolMessage(t) => {
                    if let Some(content) = &mut t.content
                        && let Cow::Owned(redacted) = jewels::redact(content)
                    {
                        *content = redacted;
                    }
                }
                // we don't redact assistant messages or system messages per instruction
                _ => {}
            }
        }
        self.last_redacted_idx = history.len();
    }
}

#[async_trait]
impl AgentPlugin for JewelsPlugin {
    fn name(&self) -> &'static str {
        "jewels"
    }

    async fn init(&mut self, ctx: &mut PluginContext) -> Result<(), AgentSdkError> {
        if let Some(mut history) = ctx.get_mut::<History>() {
            self.redact_history(&mut history.0);
        }
        Ok(())
    }

    async fn on_user_message(&mut self, _ctx: &mut PluginContext, text: String) -> String {
        jewels::redact(&text).into_owned()
    }

    async fn prepare_system_prompt(
        &mut self,
        ctx: &mut PluginContext,
    ) -> Option<Cow<'static, str>> {
        // Redact history before every iteration in case new messages were added
        // (e.g. tool results or user rejections)
        if let Some(mut history) = ctx.get_mut::<History>() {
            self.redact_history(&mut history.0);
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
            PreToolAction::Proceed(None)
        } else {
            PreToolAction::Proceed(Some(redacted))
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
            PostToolAction::Proceed(None)
        } else {
            PostToolAction::Proceed(Some(redacted))
        }
    }
}
