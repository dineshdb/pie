//! Apple Foundation Model provider for the pie CLI agent.

use aisdk::core::language_model::{
    LanguageModel, LanguageModelOptions, LanguageModelResponse, LanguageModelResponseContentType,
    LanguageModelStreamChunk, ProviderStream,
};
use aisdk::core::messages::{AssistantMessage, Message};
use aisdk::core::tools::{Tool, ToolCallInfo, ToolList};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::ffi::CString;
use std::os::raw::{c_char, c_double, c_int};

type ChunkCallback = unsafe extern "C" fn(chunk: *const c_char);
type ToolCallback = unsafe extern "C" fn(tool_id: u64, args_json: *const c_char);

#[link(name = "apple_ai_bridge")]
extern "C" {
    fn apple_ai_init() -> bool;
    fn apple_ai_prewarm() -> bool;
    fn apple_ai_check_availability() -> i32;
    fn apple_ai_get_availability_reason() -> *mut c_char;
    fn apple_ai_free_string(ptr: *mut c_char);
    fn apple_ai_register_tool_callback(callback: Option<ToolCallback>);
    #[allow(dead_code)]
    fn apple_ai_tool_result_callback(tool_id: u64, result_json: *const c_char);
    #[allow(clippy::too_many_arguments)]
    fn apple_ai_generate_unified(
        messages_json: *const c_char,
        tools_json: *const c_char,
        schema_json: *const c_char,
        temperature: c_double,
        max_tokens: c_int,
        stream: bool,
        stop_after_tool_calls: bool,
        on_chunk: Option<ChunkCallback>,
    ) -> *mut c_char;
}

unsafe fn ptr_to_string(ptr: *mut c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let s = std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned();
    apple_ai_free_string(ptr);
    Some(s)
}

unsafe extern "C" fn tool_call_noop(_tool_id: u64, _args_json: *const c_char) {}

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let boundary = s
        .char_indices()
        .take_while(|(i, _)| *i < max)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(max);
    format!("{}...[truncated]", &s[..boundary])
}

/// Compact FFI messages to fit within Apple FM's small context window.
/// Merges tool call + tool result pairs into single user messages.
fn compact_messages(msgs: Vec<FfiMessage>) -> Vec<FfiMessage> {
    let mut result: Vec<FfiMessage> = Vec::new();
    let mut i = 0;
    while i < msgs.len() {
        // Keep system and first user message as-is
        if matches!(msgs[i].role.as_str(), "system" | "user") && result.len() < 2 {
            result.push(msgs[i].clone());
            i += 1;
            continue;
        }
        // Merge assistant tool call + tool result into one user message
        if i + 1 < msgs.len()
            && msgs[i].role == "assistant"
            && msgs[i + 1].role == "tool"
        {
            result.push(FfiMessage {
                role: "user".into(),
                content: format!(
                    "Tool result for {}: {}",
                    truncate_str(&msgs[i].content, 100),
                    truncate_str(&msgs[i + 1].content, 400)
                ),
            });
            i += 2;
            continue;
        }
        result.push(msgs[i].clone());
        i += 1;
    }
    result
}

#[derive(Serialize, Clone)]
struct FfiMessage {
    role: String,
    content: String,
}

impl From<&Message> for FfiMessage {
    fn from(msg: &Message) -> Self {
        match msg {
            Message::System(s) => FfiMessage {
                role: "system".into(),
                content: s.content.clone(),
            },
            Message::User(u) => FfiMessage {
                role: "user".into(),
                content: u.content.clone(),
            },
            Message::Assistant(a) => match &a.content {
                LanguageModelResponseContentType::Text(text) => FfiMessage {
                    role: "assistant".into(),
                    content: text.clone(),
                },
                LanguageModelResponseContentType::ToolCall(info) => FfiMessage {
                    role: "assistant".into(),
                    content: format!("Tool call: {}({})", info.tool.name, info.input),
                },
                other => FfiMessage {
                    role: "assistant".into(),
                    content: format!("{other:?}"),
                },
            },
            Message::Tool(result) => FfiMessage {
                role: "tool".into(),
                content: truncate_str(
                    &result
                        .output
                        .as_ref()
                        .map(|v| v.to_string())
                        .unwrap_or_default(),
                    500,
                ),
            },
            Message::Developer(d) => FfiMessage {
                role: "developer".into(),
                content: d.clone(),
            },
        }
    }
}

#[derive(Serialize, Clone)]
struct FfiToolDef {
    id: u64,
    name: String,
    description: String,
    parameters: serde_json::Value,
}

impl From<(&Tool, u64)> for FfiToolDef {
    fn from((tool, id): (&Tool, u64)) -> Self {
        let mut params = serde_json::to_value(&tool.input_schema)
            .unwrap_or(serde_json::json!({"type": "object"}));
        if let Some(obj) = params.as_object_mut() {
            obj.remove("$schema");
            obj.remove("title");
        }
        FfiToolDef {
            id,
            name: tool.name.clone(),
            description: tool.description.trim().to_string(),
            parameters: params,
        }
    }
}

#[derive(Deserialize)]
struct FfiToolCall {
    id: String,
    function: FfiToolCallFn,
}

#[derive(Deserialize)]
struct FfiToolCallFn {
    name: String,
    arguments: String,
}

#[derive(Deserialize)]
struct FfiResponse {
    #[serde(default)]
    text: String,
    #[serde(default, rename = "toolCalls")]
    tool_calls: Vec<FfiToolCall>,
}

impl From<FfiResponse> for LanguageModelResponse {
    fn from(ffi: FfiResponse) -> Self {
        let mut contents = Vec::new();
        if !ffi.text.is_empty() {
            contents.push(LanguageModelResponseContentType::Text(ffi.text));
        }
        for tc in &ffi.tool_calls {
            let mut info = ToolCallInfo::new(&tc.function.name);
            info.id(tc.id.to_string());
            info.input(
                serde_json::from_str(&tc.function.arguments).unwrap_or(serde_json::Value::Null),
            );
            contents.push(LanguageModelResponseContentType::ToolCall(info));
        }
        LanguageModelResponse {
            contents,
            usage: None,
        }
    }
}

fn extract_tools(tool_list: &Option<ToolList>) -> Vec<Tool> {
    tool_list
        .as_ref()
        .and_then(|tl| tl.tools.lock().ok().map(|g| (*g).clone()))
        .unwrap_or_default()
}

/// Async client for Apple Foundation Models via Swift FFI bridge.
#[derive(Clone)]
pub struct AppleClient {}

impl std::fmt::Debug for AppleClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppleClient").finish()
    }
}

impl AppleClient {
    pub fn new() -> anyhow::Result<Self> {
        unsafe {
            apple_ai_init();
            apple_ai_prewarm();
            apple_ai_register_tool_callback(Some(tool_call_noop));
        }

        match unsafe { apple_ai_check_availability() } {
            1 => Ok(Self {}),
            -1 => anyhow::bail!("Apple Intelligence: device not eligible"),
            -2 => anyhow::bail!("Apple Intelligence: not enabled in Settings"),
            -3 => anyhow::bail!("Apple Intelligence: model not ready"),
            code => {
                let reason = unsafe { ptr_to_string(apple_ai_get_availability_reason()) }
                    .unwrap_or_else(|| format!("unknown (code {code})"));
                anyhow::bail!("Apple Intelligence unavailable: {reason}");
            }
        }
    }
}

#[async_trait]
impl LanguageModel for AppleClient {
    fn name(&self) -> String {
        "apple-foundation-model".into()
    }

    async fn generate_text(
        &mut self,
        options: LanguageModelOptions,
    ) -> aisdk::error::Result<LanguageModelResponse> {
        let system = options.system.as_deref().unwrap_or("");
        let messages: Vec<Message> = options.messages.iter().map(|t| t.message.clone()).collect();
        let tools = extract_tools(&options.tools);

        let mut ffi_messages: Vec<FfiMessage> = Vec::new();
        if !system.is_empty() {
            ffi_messages.push(FfiMessage {
                role: "system".into(),
                content: system.into(),
            });
        }
        ffi_messages.extend(messages.iter().map(FfiMessage::from));

        // Compact messages to stay within Apple FM's small context window
        if ffi_messages.len() > 3 {
            ffi_messages = compact_messages(ffi_messages);
        }

        let ffi_tools: Vec<FfiToolDef> = tools
            .iter()
            .enumerate()
            .map(|(i, t)| FfiToolDef::from((t, (i as u64) + 1)))
            .collect();

        let messages_json = serde_json::to_string(&ffi_messages).unwrap_or_else(|_| "[]".into());
        let tools_json = serde_json::to_string(&ffi_tools).unwrap_or_else(|_| "[]".into());

        tracing::debug!(
            msg_bytes = messages_json.len(),
            tools_bytes = tools_json.len(),
            num_messages = ffi_messages.len(),
            "sending to FFI"
        );

        let result = tokio::task::spawn_blocking(move || unsafe {
            let c_messages = CString::new(messages_json).unwrap_or_default();
            let c_tools = CString::new(tools_json).unwrap_or_default();

            let ptr = apple_ai_generate_unified(
                c_messages.as_ptr(),
                c_tools.as_ptr(),
                std::ptr::null(),
                0.3,
                0,
                false,
                false,
                None,
            );

            ptr_to_string(ptr)
        })
        .await
        .map_err(|e| aisdk::error::Error::Other(e.to_string()))?
        .ok_or_else(|| aisdk::error::Error::Other("Apple AI returned null".into()))?;

        tracing::trace!(response = %result, "apple ai");

        if result.starts_with("Error:") {
            return Err(aisdk::error::Error::Other(result));
        }

        let ffi_resp: FfiResponse = serde_json::from_str(&result)
            .map_err(|e| aisdk::error::Error::Other(format!("parsing Apple AI response: {e}")))?;

        Ok(LanguageModelResponse::from(ffi_resp))
    }

    async fn stream_text(
        &mut self,
        options: LanguageModelOptions,
    ) -> aisdk::error::Result<ProviderStream> {
        let response = self.generate_text(options).await?;
        let assistant = AssistantMessage {
            content: response.contents.first().cloned().unwrap_or_default(),
            usage: response.usage.clone(),
        };
        let chunks: Vec<aisdk::error::Result<Vec<LanguageModelStreamChunk>>> =
            vec![Ok(vec![LanguageModelStreamChunk::Done(assistant)])];
        Ok(Box::pin(futures::stream::iter(chunks)))
    }
}
