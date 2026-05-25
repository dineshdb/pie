use crate::config::pie_home;
use agentsdk::core::history::History;
use agentsdk::{
    AgentPlugin, AgentSdkError, CompletionAction, PluginContext, PostToolAction, PreToolAction,
    ToolErrorAction,
};
use async_trait::async_trait;
use serde_json::{Map, Value};
use std::borrow::Cow;
use std::io::Write;
use std::path::PathBuf;

fn format_fields(obj: &Map<String, Value>) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (k, v) in obj {
        match v {
            Value::String(s) if s.contains(char::is_whitespace) => {
                parts.push(format!("{k}={s:?}"));
            }
            Value::String(s) => {
                parts.push(format!("{k}={s}"));
            }
            Value::Number(n) => {
                parts.push(format!("{k}={n}"));
            }
            Value::Bool(b) => {
                parts.push(format!("{k}={b}"));
            }
            Value::Null => {
                parts.push(format!("{k}=null"));
            }
            _ => {
                parts.push(format!("{k}={v}"));
            }
        }
    }
    parts.join(" ")
}

fn render_tool_args(value: &Value) -> String {
    match value {
        Value::Object(obj) => format_fields(obj),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn render_tool_result(value: &Value) -> String {
    match value {
        Value::Object(obj) => {
            let mut fields = obj.clone();

            let content_keys = ["content", "stdout", "stderr", "output"];
            let mut body = String::new();

            for key in &content_keys {
                if let Some(Value::String(s)) = fields.remove(*key)
                    && !s.is_empty()
                {
                    body.push_str(&s);
                    body.push('\n');
                }
            }

            let header = if fields.is_empty() {
                String::new()
            } else {
                let h = format_fields(&fields);
                if body.is_empty() {
                    h
                } else {
                    format!("{h}\n\n")
                }
            };

            format!("{header}{body}")
        }
        Value::String(s) => {
            // Try to parse as JSON object first
            if let Ok(obj) = serde_json::from_str::<Map<String, Value>>(s) {
                render_tool_result(&Value::Object(obj))
            } else {
                s.replace("\\n", "\n")
            }
        }
        other => other.to_string(),
    }
}

#[derive(Debug)]
pub struct DebugPlugin {
    log_path: PathBuf,
    last_logged_message_count: usize,
}

impl DebugPlugin {
    pub fn new(session_id: &str, system_prompt: &str) -> Self {
        let debug_dir = pie_home().join("debug").join(session_id);
        let _ = std::fs::create_dir_all(&debug_dir);
        let log_path = debug_dir.join("debug.md");

        let this = Self {
            log_path,
            last_logged_message_count: 0,
        };

        tracing::info!(session_id = %session_id, log_path = ?this.log_path, "Debug plugin initialized");

        let header = format!(
            "# Session Debug Log: {}\nGenerated: {}\n\n---\n",
            session_id,
            chrono::Local::now().to_rfc3339()
        );
        let _ = std::fs::write(&this.log_path, header);

        if !system_prompt.is_empty() {
            this.append_debug("System Prompt", system_prompt);
        }
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

    async fn init(&mut self, ctx: &mut PluginContext) -> Result<(), AgentSdkError> {
        if let Some(comp) = ctx.get::<crate::plugin::SystemPromptComponent>() {
            self.append_debug("System Prompt", &comp.0);
        }
        Ok(())
    }

    async fn prepare_system_prompt(
        &mut self,
        ctx: &mut PluginContext,
    ) -> Option<Cow<'static, str>> {
        let history = ctx.get::<History>()?;

        let new_msgs: Vec<_> = history
            .0
            .iter()
            .skip(self.last_logged_message_count)
            .collect();
        self.last_logged_message_count = history.0.len();

        if new_msgs.is_empty() {
            return None;
        }

        let mut content = String::new();
        for msg in &new_msgs {
            use std::fmt::Write;
            if let agentsdk::core::messages::Message::AssistantMessage(a) = msg
                && let Some(text) = &a.content
            {
                let _ = writeln!(content, "- {text}");
            }
        }

        if !content.is_empty() {
            self.append_debug("Assistant Messages", &content);
        }
        None
    }

    async fn on_tool_pre_execute(
        &mut self,
        _ctx: &mut PluginContext,
        id: &str,
        name: &str,
        arguments: &Value,
    ) -> PreToolAction {
        tracing::debug!(tool = %name, id = %id, args = %arguments, "Tool pre-execute");

        let content = format!("`{id}` **{name}**({})", render_tool_args(arguments));
        self.append_debug("Tool Call", &content);
        PreToolAction::Proceed(None)
    }

    async fn on_tool_post_execute(
        &mut self,
        _ctx: &mut PluginContext,
        id: &str,
        name: &str,
        result: &Value,
    ) -> PostToolAction {
        let content = format!("`{id}` **{name}** →\n{}", render_tool_result(result));
        self.append_debug("Tool Result", &content);
        PostToolAction::Proceed(None)
    }

    async fn on_tool_error(
        &mut self,
        _ctx: &mut PluginContext,
        id: &str,
        name: &str,
        error: &str,
    ) -> ToolErrorAction {
        tracing::error!(tool = %name, id = %id, error = %error, "Tool error");

        let content = format!("`{id}` **{name}** ❌ {error}");
        self.append_debug("Tool Error", &content);
        ToolErrorAction::Proceed(None)
    }

    async fn on_completion(&mut self, _ctx: &mut PluginContext, text: &str) -> CompletionAction {
        tracing::info!(length = text.len(), "Completion received");

        let content = format!("```markdown\n{text}\n```");
        self.append_debug("Response", &content);
        CompletionAction::Accept
    }
}
