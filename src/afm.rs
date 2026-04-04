//! Apple Foundation Model provider for the pie CLI agent.
//!
//! Provides `AppleClient` — a high-level async client wrapping the Swift FFI bridge.
//! Accepts `aisdk` types (`Message`, `Tool`) and converts to FFI JSON at the boundary.
//!
//! ## Why not implement `aisdk::LanguageModel`?
//!
//! The `aisdk` crate's `ProviderStream` and `LanguageModelOptions.tools` are `pub(crate)`,
//! so we provide `AppleClient::generate()` accepting `aisdk` types directly.

use aisdk::core::language_model::{LanguageModelResponse, LanguageModelResponseContentType};
use aisdk::core::messages::Message;
use aisdk::core::tools::{Tool, ToolCallInfo};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::ffi::CString;
use std::os::raw::{c_char, c_double, c_int};

// ---------------------------------------------------------------------------
// FFI declarations (was ffi.rs)
// ---------------------------------------------------------------------------

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

/// Convert a raw pointer from the bridge into a Rust String, freeing the original.
///
/// # Safety
/// Caller must ensure `ptr` was returned by an Apple AI bridge function.
unsafe fn ptr_to_string(ptr: *mut c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let s = std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned();
    apple_ai_free_string(ptr);
    Some(s)
}

// ---------------------------------------------------------------------------
// FFI-internal types (JSON serialization to the C bridge)
// ---------------------------------------------------------------------------

#[derive(serde::Serialize)]
struct FfiMessage {
    role: String,
    content: String,
}

#[derive(serde::Serialize, Clone)]
struct FfiToolDef {
    id: u64,
    name: String,
    description: String,
    parameters: serde_json::Value,
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

// ---------------------------------------------------------------------------
// Conversion: aisdk types → FFI JSON
// ---------------------------------------------------------------------------

fn message_to_ffi(msg: &Message) -> Option<FfiMessage> {
    match msg {
        Message::System(s) => Some(FfiMessage {
            role: "system".into(),
            content: s.content.clone(),
        }),
        Message::User(u) => Some(FfiMessage {
            role: "user".into(),
            content: u.content.clone(),
        }),
        Message::Assistant(a) => match &a.content {
            LanguageModelResponseContentType::Text(text) => Some(FfiMessage {
                role: "assistant".into(),
                content: text.clone(),
            }),
            LanguageModelResponseContentType::ToolCall(info) => Some(FfiMessage {
                role: "assistant".into(),
                content: format!("Tool call: {}({})", info.tool.name, info.input),
            }),
            _ => None,
        },
        Message::Tool(result) => Some(FfiMessage {
            role: "tool".into(),
            content: result
                .output
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_default(),
        }),
        Message::Developer(d) => Some(FfiMessage {
            role: "developer".into(),
            content: d.clone(),
        }),
    }
}

fn tool_to_ffi(tool: &Tool, id: u64) -> FfiToolDef {
    let mut parameters =
        serde_json::to_value(&tool.input_schema).unwrap_or(serde_json::json!({"type": "object"}));
    if let Some(obj) = parameters.as_object_mut() {
        obj.remove("$schema");
        obj.remove("title");
    }
    FfiToolDef {
        id,
        name: tool.name.clone(),
        description: tool.description.trim().to_string(),
        parameters,
    }
}

fn ffi_to_response(ffi: FfiResponse) -> LanguageModelResponse {
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

// ---------------------------------------------------------------------------
// No-op tool callback (Swift bridge requires one registered to proceed)
// ---------------------------------------------------------------------------

unsafe extern "C" fn tool_call_noop(_tool_id: u64, _args_json: *const c_char) {}

// ---------------------------------------------------------------------------
// AppleClient
// ---------------------------------------------------------------------------

/// High-level async client for Apple Foundation Models.
///
/// Wraps FFI calls in `spawn_blocking` so they don't block the async runtime.
#[derive(Clone)]
pub struct AppleClient {}

impl AppleClient {
    pub fn new() -> Result<Self> {
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

    pub async fn generate(
        &self,
        system_prompt: &str,
        messages: &[Message],
        tools: &[Tool],
    ) -> Result<LanguageModelResponse> {
        let mut ffi_messages = Vec::new();
        if !system_prompt.is_empty() {
            ffi_messages.push(FfiMessage {
                role: "system".into(),
                content: system_prompt.into(),
            });
        }
        for msg in messages {
            if let Some(m) = message_to_ffi(msg) {
                ffi_messages.push(m);
            }
        }

        let ffi_tools: Vec<FfiToolDef> = tools
            .iter()
            .enumerate()
            .map(|(i, t)| tool_to_ffi(t, (i as u64) + 1))
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
        .context("FFI task panicked")?;

        let json_str = result.context("Apple AI returned null")?;
        tracing::trace!(response = %json_str, "apple ai");

        if json_str.starts_with("Error:") {
            anyhow::bail!("Apple AI: {json_str}");
        }

        let ffi_resp: FfiResponse =
            serde_json::from_str(&json_str).context("parsing Apple AI response")?;

        Ok(ffi_to_response(ffi_resp))
    }
}
