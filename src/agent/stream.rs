use crate::config::RetryConfig;
use crate::plugin::PermissionRequest;
use agentsdk::core::agent::{PostToolAction, PreToolAction, ToolErrorAction};
use agentsdk::core::retry::RetryAction;
use agentsdk::error::AgentSdkError;
use agentsdk::{AgentPlugin, PluginContext};
use async_trait::async_trait;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug)]
pub enum AgentEvent {
    Delta(String),
    Done(String),
    Error(String),
    UserMessage(String),
    ToolCall {
        name: String,
        display: String,
        output: String,
    },
    #[expect(dead_code)]
    PermissionRequest(PermissionRequest),
}

pub struct StreamPlugin {
    pub event_tx: UnboundedSender<AgentEvent>,
    pub api_error_count: u32,
    pub rate_limit_count: u32,
    pub retry: RetryConfig,
}

impl StreamPlugin {
    pub fn new(event_tx: UnboundedSender<AgentEvent>, retry: RetryConfig) -> Self {
        Self {
            event_tx,
            api_error_count: 0,
            rate_limit_count: 0,
            retry,
        }
    }
}

#[async_trait]
impl AgentPlugin for StreamPlugin {
    fn name(&self) -> &'static str {
        "stream"
    }

    async fn on_text_delta(&mut self, _ctx: &mut PluginContext, text: &str) {
        if !text.is_empty() {
            let _ = self.event_tx.send(AgentEvent::Delta(text.to_string()));
        }
    }

    async fn on_user_message(&mut self, _ctx: &mut PluginContext, text: String) -> String {
        text
    }

    async fn on_tool_pre_execute(
        &mut self,
        _ctx: &mut PluginContext,
        _id: &str,
        name: &str,
        arguments: &serde_json::Value,
    ) -> PreToolAction {
        let args_str = arguments.to_string();
        let args_redacted = if name == "websearch" {
            args_str
        } else {
            crate::plugin::JewelsPlugin::redact(&crate::utils::anonymize_path(&args_str))
        };

        tracing::debug!(tool = name, args = %args_redacted, "tool call");
        let _ = self.event_tx.send(AgentEvent::ToolCall {
            name: name.to_string(),
            display: format!("{name}({arguments})"),
            output: String::new(),
        });

        PreToolAction::Continue(None)
    }

    async fn on_tool_post_execute(
        &mut self,
        _ctx: &mut PluginContext,
        _id: &str,
        name: &str,
        result: &serde_json::Value,
    ) -> PostToolAction {
        let output = if let serde_json::Value::String(s) = result {
            s.clone()
        } else {
            result.to_string()
        };

        let output = if name == "websearch" {
            output
        } else {
            crate::plugin::JewelsPlugin::redact(&crate::utils::anonymize_path(&output))
        };

        tracing::debug!(tool = name, output = %output, "tool result");
        let _ = self.event_tx.send(AgentEvent::ToolCall {
            name: name.to_string(),
            display: String::new(),
            output,
        });

        PostToolAction::Continue(None)
    }

    async fn on_tool_error(
        &mut self,
        _ctx: &mut PluginContext,
        _id: &str,
        name: &str,
        error: &str,
    ) -> ToolErrorAction {
        tracing::debug!(tool = name, error = %error, "tool error");
        let _ = self.event_tx.send(AgentEvent::ToolCall {
            name: name.to_string(),
            display: String::new(),
            output: format!("Error: {error}"),
        });
        let _ = self
            .event_tx
            .send(AgentEvent::Error(format!("Tool {name} failed: {error}")));
        ToolErrorAction::Continue(None)
    }

    async fn on_api_error(
        &mut self,
        _ctx: &mut PluginContext,
        error: &AgentSdkError,
    ) -> RetryAction {
        self.api_error_count += 1;

        if let Some(status) = error.status_code() {
            if status == 429 {
                self.rate_limit_count += 1;
                if self.rate_limit_count > self.retry.rate_limit.max_errors {
                    let _ = self.event_tx.send(AgentEvent::Error(
                        "Too many rate limit errors, aborting".to_string(),
                    ));
                    return RetryAction::DoNotRetry;
                }
                tracing::warn!(status = %status, "rate limited, retrying");
                return RetryAction::Retry(std::time::Duration::from_secs(
                    self.retry.rate_limit.retry_delay_secs,
                ));
            }

            if status.is_server_error() {
                if self.api_error_count > self.retry.api_error.max_errors {
                    let _ = self.event_tx.send(AgentEvent::Error(
                        "Too many API errors, aborting".to_string(),
                    ));
                    return RetryAction::DoNotRetry;
                }
                tracing::warn!(status = %status, count = self.api_error_count, "server error, retrying");
                return RetryAction::Retry(std::time::Duration::from_secs(
                    self.retry.api_error.retry_delay_secs,
                ));
            }
        }

        RetryAction::DoNotRetry
    }
}
