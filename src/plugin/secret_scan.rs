use agentsdk::core::messages::Message;
use agentsdk::openai::api::ChatCompletionRequestUserMessageContent;
use agentsdk::{
    AgentPlugin, CompletionAction, Messages, PluginContext, PostToolAction, PreToolAction,
};
use async_trait::async_trait;
use jewels::{redact, redact_json, scan};
use std::borrow::Cow;
use tracing::warn;

#[derive(Debug, Default)]
pub struct SecretScanningPlugin;

impl SecretScanningPlugin {
    pub fn new() -> Self {
        Self
    }

    fn scan_and_warn(text: &str, context: &str) {
        let matches = scan(text);
        if !matches.is_empty() {
            let kinds: Vec<_> = matches.iter().map(|m| m.kind).collect();
            let msg = format!("Secrets detected in {context}: {kinds:?}");
            warn!("{msg}");
            crate::ui::notify::notify("pie: Secret Detected", &msg);
        }
    }

    fn check_env_file_access(tool_name: &str, arguments: &serde_json::Value) {
        if (tool_name == "read_file" || tool_name == "replace" || tool_name == "write_file")
            && let Some(path) = arguments.get("path").and_then(|v| v.as_str())
            && path.contains(".env")
        {
            let msg = format!("LLM is accessing environment file: {path}");
            warn!("{msg}");
            crate::ui::notify::notify("pie: Env File Access", &msg);
        }
    }
}

#[async_trait]
impl AgentPlugin for SecretScanningPlugin {
    fn name(&self) -> &'static str {
        "secret_scanning"
    }

    async fn prepare_system_prompt(
        &mut self,
        _ctx: &PluginContext,
        history: &Messages,
    ) -> Option<Cow<'static, str>> {
        for msg in history {
            match msg {
                Message::UserMessage(u) => {
                    if let Some(ChatCompletionRequestUserMessageContent::String(s)) = &u.content {
                        Self::scan_and_warn(s, "user message");
                    }
                }
                Message::AssistantMessage(a) => {
                    if let Some(s) = &a.content {
                        Self::scan_and_warn(s, "assistant message");
                    }
                }
                Message::ToolMessage(t) => {
                    if let Some(s) = &t.content {
                        Self::scan_and_warn(s, &format!("tool output (id: {})", t.tool_call_id));
                    }
                }
                Message::SystemMessage(s) => {
                    if let Some(content) = &s.content {
                        Self::scan_and_warn(content, "system message");
                    }
                }
                Message::FunctionMessage(_) => {}
            }
        }
        None
    }

    async fn on_tool_pre_execute(
        &mut self,
        _ctx: &PluginContext,
        _id: &str,
        name: &str,
        arguments: &serde_json::Value,
    ) -> PreToolAction {
        Self::check_env_file_access(name, arguments);

        let redacted = redact_json(arguments);
        if redacted == *arguments {
            PreToolAction::Continue(None)
        } else {
            warn!("Secrets redacted from tool {name} arguments");
            PreToolAction::Continue(Some(redacted))
        }
    }

    async fn on_tool_post_execute(
        &mut self,
        _ctx: &PluginContext,
        _id: &str,
        name: &str,
        result: &serde_json::Value,
    ) -> PostToolAction {
        let redacted = redact_json(result);
        if redacted == *result {
            PostToolAction::Continue(None)
        } else {
            warn!("Secrets redacted from tool {name} output");
            PostToolAction::Continue(Some(redacted))
        }
    }

    async fn on_completion(&mut self, _ctx: &PluginContext, text: String) -> CompletionAction {
        let redacted = redact(&text);
        if redacted == text {
            CompletionAction::Accept(None)
        } else {
            warn!("Secrets redacted from LLM completion");
            CompletionAction::Accept(Some(redacted))
        }
    }
}
