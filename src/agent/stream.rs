use crate::config::RetryConfig;
use crate::plugin::PermissionRequest;
use agentsdk::core::agent::{PostToolAction, PreToolAction};
use agentsdk::core::retry::RetryAction;
use agentsdk::error::AgentSdkError;
use agentsdk::{AgentPlugin, PluginContext};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::time::Instant;
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
    /// Wall-time instrumentation: tool-call id -> start instant.
    tool_starts: HashMap<String, Instant>,
    /// Wall-time instrumentation: current iteration start.
    iter_start: Option<Instant>,
}

impl StreamPlugin {
    pub fn new(event_tx: UnboundedSender<AgentEvent>, retry: RetryConfig) -> Self {
        Self {
            event_tx,
            api_error_count: 0,
            rate_limit_count: 0,
            retry,
            tool_starts: HashMap::new(),
            iter_start: None,
        }
    }
}

#[async_trait]
impl AgentPlugin for StreamPlugin {
    fn name(&self) -> &'static str {
        "stream"
    }

    fn on_text_delta(&mut self, _ctx: &mut PluginContext, text: &str) {
        if !text.is_empty() {
            let _ = self.event_tx.send(AgentEvent::Delta(text.to_string()));
        }
    }

    async fn on_user_message(&mut self, _ctx: &mut PluginContext, text: String) -> String {
        text
    }

    async fn on_iteration_start(&mut self, _ctx: &mut PluginContext, iteration: usize) {
        self.iter_start = Some(Instant::now());
        tracing::info!(iteration, "timing: iteration start");
    }

    async fn on_iteration_end(
        &mut self,
        _ctx: &mut PluginContext,
        iteration: usize,
        had_tool_calls: bool,
    ) {
        if let Some(start) = self.iter_start.take() {
            tracing::info!(
                iteration,
                had_tool_calls,
                ms = start.elapsed().as_millis() as u64,
                "timing: iteration end"
            );
        }
    }

    async fn on_tool_pre_execute(
        &mut self,
        _ctx: &mut PluginContext,
        id: &str,
        name: &str,
        arguments: &Value,
    ) -> PreToolAction {
        self.tool_starts.insert(id.to_string(), Instant::now());
        let _ = self.event_tx.send(AgentEvent::ToolCall {
            name: name.to_string(),
            display: format!("{name}({arguments})"),
            output: String::new(),
        });

        PreToolAction::Proceed(None)
    }

    async fn on_tool_post_execute(
        &mut self,
        _ctx: &mut PluginContext,
        id: &str,
        name: &str,
        result: &Result<Value, String>,
    ) -> PostToolAction {
        if let Some(start) = self.tool_starts.remove(id) {
            tracing::info!(
                tool = name,
                ms = start.elapsed().as_millis() as u64,
                ok = result.is_ok(),
                "timing: tool done"
            );
        }
        let output = match result {
            Ok(value) => {
                let text = if let Value::String(s) = value {
                    s.clone()
                } else {
                    value.to_string()
                };
                if name == "web_search" {
                    text
                } else {
                    jewels::redact(&crate::utils::anonymize_path(&text)).into_owned()
                }
            }
            Err(error) => {
                tracing::debug!(tool = name, error = %error, "tool error");
                let _ = self
                    .event_tx
                    .send(AgentEvent::Error(format!("Tool {name} failed: {error}")));
                format!("Error: {error}")
            }
        };

        let _ = self.event_tx.send(AgentEvent::ToolCall {
            name: name.to_string(),
            display: String::new(),
            output,
        });

        PostToolAction::Proceed(None)
    }

    async fn on_api_error(
        &mut self,
        _ctx: &mut PluginContext,
        error: &AgentSdkError,
    ) -> RetryAction {
        self.api_error_count += 1;

        let Some(status) = error.status_code() else {
            return RetryAction::GiveUp;
        };

        if status == 429 {
            self.rate_limit_count += 1;
            if self.rate_limit_count > self.retry.rate_limit.max_errors {
                let _ = self.event_tx.send(AgentEvent::Error(
                    "Too many rate limit errors, aborting".to_string(),
                ));
                return RetryAction::GiveUp;
            }
            tracing::warn!(status = %status, "rate limited, retrying");
            return RetryAction::RetryAfter(std::time::Duration::from_secs(
                self.retry.rate_limit.retry_delay_secs,
            ));
        }

        if status.is_server_error() {
            if self.api_error_count > self.retry.api_error.max_errors {
                let _ = self.event_tx.send(AgentEvent::Error(
                    "Too many API errors, aborting".to_string(),
                ));
                return RetryAction::GiveUp;
            }
            tracing::warn!(status = %status, count = self.api_error_count, "server error, retrying");
            return RetryAction::RetryAfter(std::time::Duration::from_secs(
                self.retry.api_error.retry_delay_secs,
            ));
        }

        RetryAction::GiveUp
    }
}
