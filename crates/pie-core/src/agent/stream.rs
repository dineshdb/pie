use crate::config::RetryConfig;
use crate::plugin::PermissionRequest;
use crate::usage::RunUsage;
use agentsdk::core::agent::{CompletionAction, PostToolAction, PreToolAction};
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
    /// A tool call. Emitted twice per call: once before execution
    /// (`display` carries `name(args)`, `output` empty) and once after
    /// (`display` empty, `output` the result text, `failed` whether it
    /// errored). `id` pairs the two halves — consumers building call →
    /// result views (TUI, ACP) key on it.
    ToolCall {
        id: String,
        name: String,
        display: String,
        output: String,
        failed: bool,
    },
    /// Emitted once per run, right before [`AgentEvent::Done`], with the
    /// token totals the provider reported and their cost (`None` when the
    /// model has no configured pricing or reported no usage).
    Usage {
        usage: RunUsage,
        cost_usd: Option<f64>,
    },
    PermissionRequest(PermissionRequest),
}

pub struct StreamPlugin {
    pub event_tx: UnboundedSender<AgentEvent>,
    pub api_error_count: u32,
    pub rate_limit_count: u32,
    pub retry: RetryConfig,
    /// Wall-time instrumentation: tool-call id -> start instant + args.
    tool_starts: HashMap<String, ToolStart>,
    /// Wall-time instrumentation: current iteration start.
    iter_start: Option<Instant>,
    /// Empty-final-completions rejected so far (bounded retry).
    empty_rejects: u32,
}

/// A pending tool call being timed.
struct ToolStart {
    start: Instant,
    /// Compact JSON of the call arguments, truncated for logging.
    args: String,
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
            empty_rejects: 0,
        }
    }
}

/// Maximum characters of a tool result fed back into the conversation.
/// Oversized results (e.g. a runaway Glob or a huge file read) are
/// head-truncated with a notice — unbounded tool output can otherwise
/// explode the prompt past the model's context window mid-run.
const TOOL_OUTPUT_LIMIT: usize = 16_000;

fn clamp_tool_output(text: &str) -> Option<String> {
    if text.len() <= TOOL_OUTPUT_LIMIT {
        return None;
    }
    // Cut on a char boundary near the limit.
    let mut end = TOOL_OUTPUT_LIMIT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    Some(format!(
        "[output truncated: showing first {end} of {} chars]\n{}",
        text.len(),
        &text[..end]
    ))
}

/// Maximum characters of tool-call arguments shown in timing logs and
/// non-interactive progress lines. Full arguments can carry entire file
/// bodies (Write/Edit) — logging those unclamped would flood stderr.
const TOOL_ARGS_LOG_LIMIT: usize = 160;

pub(crate) fn truncate_for_log(text: &str) -> String {
    if text.len() <= TOOL_ARGS_LOG_LIMIT {
        return text.to_string();
    }
    let mut end = TOOL_ARGS_LOG_LIMIT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[+{} chars]", &text[..end], text.len() - end)
}

/// Human-friendly single-line rendering of tool arguments for progress and
/// chat lines: `key = value` pairs instead of JSON escapes. Newlines inside
/// values are re-escaped — a display must stay one terminal line.
fn display_args(arguments: &Value) -> String {
    let Value::Object(map) = arguments else {
        return arguments.to_string();
    };
    if map.is_empty() {
        return "{}".to_string();
    }
    let pairs: Vec<String> = map
        .iter()
        .map(|(k, v)| {
            let value = match v {
                Value::String(s) => s
                    .replace('\n', "\\n")
                    .replace('\r', "\\r")
                    .replace('\t', "\\t"),
                other => other.to_string(),
            };
            format!("{k} = {value}")
        })
        .collect();
    format!("{{{}}}", pairs.join(", "))
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
        tracing::debug!(iteration, "timing: iteration start");
    }

    async fn on_iteration_end(
        &mut self,
        _ctx: &mut PluginContext,
        iteration: usize,
        had_tool_calls: bool,
    ) {
        if let Some(start) = self.iter_start.take() {
            tracing::debug!(
                iteration,
                had_tool_calls,
                ms = start.elapsed().as_millis() as u64,
                "timing: iteration end"
            );
        }
    }

    /// A final completion with no tool calls and no visible text is a
    /// truncated/degenerate ending (e.g. a reasoning-budget cut mid-thought
    /// collapsing straight to EOS). Reject it so the model retries with a
    /// correction — but only twice, then accept whatever we have.
    async fn on_completion(&mut self, _ctx: &mut PluginContext, text: &str) -> CompletionAction {
        if text.trim().is_empty() && self.empty_rejects < 2 {
            self.empty_rejects += 1;
            tracing::warn!(
                attempt = self.empty_rejects,
                "empty final completion, retrying"
            );
            return CompletionAction::Reject {
                reason: "Your final answer was empty. Answer the user's question directly \
                         with visible text now."
                    .to_string(),
            };
        }
        CompletionAction::Accept
    }

    async fn on_tool_pre_execute(
        &mut self,
        _ctx: &mut PluginContext,
        id: &str,
        name: &str,
        arguments: &Value,
    ) -> PreToolAction {
        self.tool_starts.insert(
            id.to_string(),
            ToolStart {
                start: Instant::now(),
                args: truncate_for_log(&arguments.to_string()),
            },
        );
        let _ = self.event_tx.send(AgentEvent::ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            display: format!("{name}{}", display_args(arguments)),
            output: String::new(),
            failed: false,
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
        if let Some(tool_start) = self.tool_starts.remove(id) {
            tracing::debug!(
                tool = name,
                ms = tool_start.start.elapsed().as_millis() as u64,
                ok = result.is_ok(),
                args = %tool_start.args,
                "timing: tool done"
            );
        }
        let (output, clamped_value) = match result {
            Ok(value) => {
                let text = if let Value::String(s) = value {
                    s.clone()
                } else {
                    value.to_string()
                };
                let text = if name == "web_search" {
                    text
                } else {
                    jewels::redact(&crate::utils::anonymize_path(&text)).into_owned()
                };
                match clamp_tool_output(&text) {
                    Some(clamped) => (clamped.clone(), Some(Value::String(clamped))),
                    None => (text, None),
                }
            }
            Err(error) => {
                tracing::debug!(tool = name, error = %error, "tool error");
                // No AgentEvent::Error here: the ToolCall event below
                // already carries the failure (`failed` + the reason), and
                // Error means the run itself is ending. Emitting both made
                // every frontend show the failure twice — and the TUI treat
                // a recoverable tool failure as a dead stream.
                (format!("Error: {error}"), None)
            }
        };

        let _ = self.event_tx.send(AgentEvent::ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            display: String::new(),
            output,
            failed: result.is_err(),
        });

        PostToolAction::Proceed(clamped_value)
    }

    async fn on_api_error(
        &mut self,
        _ctx: &mut PluginContext,
        error: &AgentSdkError,
    ) -> RetryAction {
        self.api_error_count += 1;

        let Some(status) = error.status_code() else {
            // A dropped/truncated stream carries no HTTP status. Nothing was
            // committed to history, so re-issuing the request is safe — a
            // gateway blip shouldn't kill the whole turn.
            if !error.is_transport() {
                return RetryAction::GiveUp;
            }
            if self.api_error_count > self.retry.api_error.max_errors {
                let _ = self.event_tx.send(AgentEvent::Error(
                    "Too many API errors, aborting".to_string(),
                ));
                return RetryAction::GiveUp;
            }
            tracing::warn!(
                count = self.api_error_count,
                error = %error,
                "connection to the model provider failed mid-response, retrying"
            );
            return RetryAction::RetryAfter(std::time::Duration::from_secs(
                self.retry.api_error.retry_delay_secs,
            ));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_outputs_pass_through_unchanged() {
        assert_eq!(clamp_tool_output("hello"), None);
        assert_eq!(clamp_tool_output(&"x".repeat(TOOL_OUTPUT_LIMIT)), None);
    }

    #[test]
    fn oversized_outputs_are_truncated_with_notice() {
        let big = "é".repeat(TOOL_OUTPUT_LIMIT); // multibyte: boundary safety
        let clamped = clamp_tool_output(&big).expect("must clamp");
        assert!(clamped.starts_with("[output truncated: showing first"));
        assert!(clamped.len() < big.len() + 100);
        assert!(clamped.is_char_boundary(clamped.len()));
    }

    #[test]
    fn display_args_shows_pairs_without_json_escapes() {
        let args: Value = serde_json::from_str(r#"{"command":"rg -n \"pie_home\" file"}"#).unwrap();
        assert_eq!(display_args(&args), r#"{command = rg -n "pie_home" file}"#);
    }

    #[test]
    fn display_args_joins_pairs_and_keeps_one_line() {
        let args: Value =
            serde_json::from_str(r#"{"content":"line1\nline2","path":"a.rs"}"#).unwrap();
        assert_eq!(
            display_args(&args),
            r#"{content = line1\nline2, path = a.rs}"#
        );

        assert_eq!(display_args(&serde_json::json!({})), "{}");
        assert_eq!(display_args(&serde_json::json!(null)), "null");
    }

    /// Mints a real transport error by hanging up mid-body: reqwest decode
    /// errors have no public constructor, so the only honest way to get one
    /// is an actual truncated response. (`ApiError::Builder` — an SSE parse
    /// failure — must NOT be treated as transport, so a hand-built error
    /// wouldn't do either.)
    async fn transport_error_from_truncated_stream() -> crate::error::Result<AgentSdkError> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        tokio::spawn(async move {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0u8; 4096];
            let _ = tokio::io::AsyncReadExt::read(&mut sock, &mut buf).await;
            let body = "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[]}\n\n";
            let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 600\r\nconnection: close\r\n\r\n";
            use tokio::io::AsyncWriteExt as _;
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(body.as_bytes()).await;
            let _ = sock.shutdown().await;
        });

        let model = agentsdk::OpenAI::new(agentsdk::ModelConfig {
            base_url: format!("http://{addr}"),
            api_key: "test".into(),
            model: "test".into(),
        });
        let options = agentsdk::AgentOptions::default();
        let mut stream = model
            .stream(&options, &[agentsdk::core::messages::user("Hi")])
            .await?;
        while let Some(chunk) = futures::StreamExt::next(&mut stream).await {
            match chunk {
                Ok(_) => continue,
                Err(e) => {
                    assert!(e.is_transport(), "fixture must mint a transport error");
                    return Ok(e);
                }
            }
        }
        Err(crate::error::AppError::Config(
            "truncated body must produce an error".into(),
        ))
    }

    // A dropped stream retried within the api_error budget, then give up —
    // previously any status-less error (every mid-stream drop) killed the
    // turn on the first failure.
    #[tokio::test]
    async fn transport_errors_retry_within_budget_then_give_up() -> crate::error::Result<()> {
        let error = transport_error_from_truncated_stream().await?;

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut plugin = StreamPlugin::new(tx, RetryConfig::default());

        let mut world = agentsdk::hecs::World::new();
        let entity = world.spawn(());
        let mut ctx = PluginContext::new(world, entity);

        for count in 1..=plugin.retry.api_error.max_errors {
            plugin.api_error_count = count - 1;
            let action = plugin.on_api_error(&mut ctx, &error).await;
            assert!(
                matches!(action, RetryAction::RetryAfter(_)),
                "drop {count} must be retryable, got {action:?}"
            );
        }
        let action = plugin.on_api_error(&mut ctx, &error).await;
        assert_eq!(action, RetryAction::GiveUp);
        Ok(())
    }
}
