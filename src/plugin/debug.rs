use crate::config::pie_home;
use agentsdk::core::messages::Message;
use agentsdk::openai::api::ChatCompletionRequestUserMessageContent;
use agentsdk::{
    AgentPlugin, CompletionAction, Messages, PluginContext, PostToolAction, PreToolAction,
    ToolErrorAction,
};
use async_trait::async_trait;
use std::borrow::Cow;
use std::io::Write;
use std::path::PathBuf;

use std::fmt::Write as _;

#[derive(Debug)]
pub struct DebugPlugin {
    #[allow(dead_code)]
    session_id: String,
    log_path: PathBuf,
}

impl DebugPlugin {
    pub fn new(session_id: &str, system_prompt: &str) -> Self {
        let debug_dir = pie_home().join("debug").join(session_id);
        let _ = std::fs::create_dir_all(&debug_dir);
        let log_path = debug_dir.join("debug.md");

        let this = Self {
            session_id: session_id.to_string(),
            log_path,
        };

        tracing::info!(session_id = %session_id, log_path = ?this.log_path, "Debug plugin initialized");

        let header = format!(
            "# Session Debug Log: {}\nGenerated: {}\n\n---\n",
            session_id,
            chrono::Local::now().to_rfc3339()
        );
        let _ = std::fs::write(&this.log_path, header);

        this.append_debug("System Prompt", system_prompt);
        this
    }

    fn append_debug(&self, title: &str, content: &str) {
        let timestamp = chrono::Local::now()
            .format("%Y-%m-%d %H:%M:%S.%f")
            .to_string();
        let section = format!("## {title} [{timestamp}]\n\n{content}\n\n---\n");

        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
        {
            let _ = file.write_all(section.as_bytes());
            tracing::debug!(title = %title, "Appended to debug log");
        } else {
            tracing::error!(path = ?self.log_path, "Failed to append to debug log");
        }
    }
}

#[async_trait]
impl AgentPlugin for DebugPlugin {
    fn name(&self) -> &'static str {
        "debug"
    }

    async fn prepare_system_prompt(
        &mut self,
        _ctx: &PluginContext,
        history: &Messages,
    ) -> Option<Cow<'static, str>> {
        let mut content = String::from("### Full Conversation History\n\n");
        for msg in history {
            match msg {
                Message::UserMessage(u) => {
                    content.push_str("#### User\n");
                    let text = match &u.content {
                        Some(ChatCompletionRequestUserMessageContent::String(s)) => s.clone(),
                        _ => String::new(),
                    };
                    content.push_str("```markdown\n");
                    content.push_str(&text);
                    content.push_str("\n```\n\n");
                }
                Message::AssistantMessage(a) => {
                    content.push_str("#### Assistant\n");
                    if let Some(text) = &a.content {
                        content.push_str("```markdown\n");
                        content.push_str(text);
                        content.push_str("\n```\n\n");
                    }
                    if let Some(tool_calls) = &a.tool_calls {
                        for tc in tool_calls {
                            let _ = writeln!(content, "##### Tool Call: {}\n", tc.function.name);
                            content.push_str("```json\n");
                            content.push_str(&tc.function.arguments);
                            content.push_str("\n```\n\n");
                        }
                    }
                }
                Message::ToolMessage(t) => {
                    let _ = writeln!(content, "#### Tool (ID: {})\n", t.tool_call_id);
                    if let Some(text) = &t.content {
                        content.push_str("```\n");
                        content.push_str(text);
                        content.push_str("\n```\n\n");
                    }
                }
                Message::SystemMessage(s) => {
                    content.push_str("#### System\n");
                    if let Some(text) = &s.content {
                        content.push_str("```markdown\n");
                        content.push_str(text);
                        content.push_str("\n```\n\n");
                    }
                }
                Message::FunctionMessage(_) => {}
            }
        }
        self.append_debug("Conversation History", &content);
        None
    }

    async fn on_tool_pre_execute(
        &mut self,
        _ctx: &PluginContext,
        id: &str,
        name: &str,
        arguments: &serde_json::Value,
    ) -> PreToolAction {
        let args_pretty = serde_json::to_string_pretty(arguments).unwrap_or_default();
        tracing::info!(tool = %name, id = %id, "Tool pre-execute");

        let content = format!(
            "**Action**: Pre-Execute Tool `{name}`\n**ID**: `{id}`\n\n**Arguments**:\n```json\n{args_pretty}\n```"
        );
        self.append_debug(&format!("Tool Pre-Execute: {name}"), &content);
        PreToolAction::Continue(None)
    }

    async fn on_tool_post_execute(
        &mut self,
        _ctx: &PluginContext,
        id: &str,
        name: &str,
        result: &serde_json::Value,
    ) -> PostToolAction {
        let result_pretty = serde_json::to_string_pretty(result).unwrap_or_default();
        tracing::info!(tool = %name, id = %id, "Tool post-execute");

        let content = format!(
            "**Action**: Post-Execute Tool `{name}`\n**ID**: `{id}`\n\n**Result**:\n```json\n{result_pretty}\n```"
        );
        self.append_debug(&format!("Tool Post-Execute: {name}"), &content);
        PostToolAction::Continue(None)
    }

    async fn on_tool_error(
        &mut self,
        _ctx: &PluginContext,
        id: &str,
        name: &str,
        error: &str,
    ) -> ToolErrorAction {
        tracing::error!(tool = %name, id = %id, error = %error, "Tool error");

        let content = format!(
            "**Action**: Tool Error `{name}`\n**ID**: `{id}`\n\n**Error**:\n```\n{error}\n```"
        );
        self.append_debug(&format!("Tool Error: {name}"), &content);
        ToolErrorAction::Continue(None)
    }

    async fn on_completion(&mut self, _ctx: &PluginContext, text: String) -> CompletionAction {
        tracing::info!(length = text.len(), "Completion received");

        let content = format!("**Response**:\n\n```markdown\n{text}\n```");
        self.append_debug("Final Completion", &content);
        CompletionAction::Accept(None)
    }
}
