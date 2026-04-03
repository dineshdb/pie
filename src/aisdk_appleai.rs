//! Apple Foundation Model provider for the pie CLI agent.
//!
//! This module provides a high-level async client (`AppleClient`) that wraps
//! the raw FFI declarations in `ffi.rs`. It accepts `aisdk` types (`Message`,
//! `Tool`) and converts them to FFI JSON at the boundary.
//!
//! ## Why not implement `aisdk::LanguageModel`?
//!
//! The `aisdk` crate's `ProviderStream` type alias and `LanguageModelOptions.tools`
//! field are `pub(crate)`, making it impossible to implement the `LanguageModel`
//! trait from outside the crate. Instead, we provide our own `AppleClient` with
//! `generate()` that accepts `aisdk` types directly.

use crate::ffi;
use aisdk::core::language_model::{LanguageModelResponse, LanguageModelResponseContentType};
use aisdk::core::messages::Message;
use aisdk::core::tools::{Tool, ToolCallInfo};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::ffi::CString;
use std::os::raw::c_char;

// ---------------------------------------------------------------------------
// FFI-internal types (only used for JSON serialization to the C bridge)
// ---------------------------------------------------------------------------

/// Message format for Apple Foundation Model FFI.
#[derive(serde::Serialize)]
struct FfiMessage {
    role: String,
    content: String,
}

/// Tool definition for Apple Foundation Model FFI.
#[derive(serde::Serialize, Clone)]
pub struct FfiToolDef {
    pub id: u64,
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Tool call returned by Apple Foundation Models FFI.
#[derive(Deserialize, Debug)]
struct FfiToolCall {
    id: String,
    function: FfiToolCallFn,
}

#[derive(Deserialize, Debug)]
struct FfiToolCallFn {
    name: String,
    arguments: String,
}

/// Response from Apple Foundation Models FFI.
#[derive(Deserialize, Debug)]
struct FfiResponse {
    #[serde(default)]
    text: String,
    #[serde(default, rename = "toolCalls")]
    tool_calls: Vec<FfiToolCall>,
}

// ---------------------------------------------------------------------------
// Conversion helpers: aisdk types → FFI JSON
// ---------------------------------------------------------------------------

/// Convert an `aisdk::Message` to an FFI message.
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
        Message::Tool(result) => {
            let output = result
                .output
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_default();
            Some(FfiMessage {
                role: "tool".into(),
                content: output,
            })
        }
        Message::Developer(d) => Some(FfiMessage {
            role: "developer".into(),
            content: d.clone(),
        }),
    }
}

/// Convert an `aisdk::Tool` to an FFI tool definition.
fn tool_to_ffi(tool: &Tool, id: u64) -> FfiToolDef {
    let mut parameters =
        serde_json::to_value(&tool.input_schema).unwrap_or(serde_json::json!({"type": "object"}));
    // Strip schemars metadata that Apple Foundation Models doesn't expect
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

/// Convert an FFI response to an `aisdk::LanguageModelResponse`.
fn ffi_to_response(ffi: FfiResponse) -> LanguageModelResponse {
    let mut contents = Vec::new();

    if !ffi.text.is_empty() {
        contents.push(LanguageModelResponseContentType::Text(ffi.text));
    }

    for tc in &ffi.tool_calls {
        let mut info = ToolCallInfo::new(&tc.function.name);
        info.id(tc.id.to_string());
        info.input(serde_json::from_str(&tc.function.arguments).unwrap_or(serde_json::Value::Null));
        contents.push(LanguageModelResponseContentType::ToolCall(info));
    }

    LanguageModelResponse {
        contents,
        usage: None,
    }
}

// ---------------------------------------------------------------------------
// Tool callback (no-op — lets Swift bridge record tool calls)
// ---------------------------------------------------------------------------

/// No-op callback for Swift bridge tool calls. The Swift `JSProxyTool` requires
/// a registered callback to proceed past its guard; it records tool calls in
/// `ToolCallCollector` and returns them in the JSON response. We execute tools
/// on the Rust side after receiving the response.
unsafe extern "C" fn tool_call_noop_callback(_tool_id: u64, _args_json: *const c_char) {}

// ---------------------------------------------------------------------------
// AppleClient
// ---------------------------------------------------------------------------

/// High-level async client for Apple Foundation Models.
///
/// Wraps the FFI calls in `spawn_blocking` so they don't block the async
/// runtime. Accepts `aisdk` types and converts at the boundary.
pub struct AppleClient {
    _initialized: bool,
}

impl AppleClient {
    /// Create a new client. Initializes the bridge, then checks availability.
    pub fn new() -> Result<Self> {
        // Init must come first (matches apple_ai crate behavior)
        unsafe { ffi::apple_ai_init() };

        // Register a no-op tool callback so the Swift bridge's JSProxyTool
        // records tool calls in ToolCallCollector instead of returning
        // "Tool system not available".
        unsafe {
            ffi::apple_ai_register_tool_callback(Some(tool_call_noop_callback));
        }

        match unsafe { ffi::apple_ai_check_availability() } {
            1 => Ok(Self { _initialized: true }),
            -1 => anyhow::bail!("Apple Intelligence: device not eligible"),
            -2 => anyhow::bail!("Apple Intelligence: not enabled in Settings"),
            -3 => anyhow::bail!("Apple Intelligence: model not ready"),
            _ => {
                let reason = unsafe { ffi::ptr_to_string(ffi::apple_ai_get_availability_reason()) }
                    .unwrap_or_else(|| "unknown reason".to_string());
                anyhow::bail!("Apple Intelligence unavailable: {reason}");
            }
        }
    }

    /// Generate a response from Apple Foundation Models using aisdk types.
    ///
    /// - `system_prompt`: System instructions (prepended to messages)
    /// - `messages`: Conversation history as `aisdk::Message`
    /// - `tools`: Tool definitions as `aisdk::Tool`
    pub async fn generate(
        &self,
        system_prompt: &str,
        messages: &[Message],
        tools: &[Tool],
    ) -> Result<LanguageModelResponse> {
        // Build FFI messages
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

        // Build FFI tool definitions
        let ffi_tools: Vec<FfiToolDef> = tools
            .iter()
            .enumerate()
            .map(|(i, t)| tool_to_ffi(t, (i as u64) + 1))
            .collect();

        let messages_json = serde_json::to_string(&ffi_messages).unwrap_or_else(|_| "[]".into());
        let tools_json = serde_json::to_string(&ffi_tools).unwrap_or_else(|_| "[]".into());

        let result = tokio::task::spawn_blocking(move || {
            unsafe {
                let c_messages = CString::new(messages_json).unwrap_or_default();
                let c_tools = CString::new(tools_json).unwrap_or_default();

                let ptr = ffi::apple_ai_generate_unified(
                    c_messages.as_ptr(),
                    c_tools.as_ptr(),
                    std::ptr::null(), // no schema
                    0.0,              // temperature (0 = deterministic, matches original)
                    0,                // max_tokens (0 = model default)
                    false,            // no streaming
                    false,            // don't stop after tool calls
                    None,             // no chunk callback
                );

                ffi::ptr_to_string(ptr)
            }
        })
        .await
        .context("FFI task panicked")?;

        let json_str = result.context("Apple AI returned null")?;
        let ffi_resp: FfiResponse =
            serde_json::from_str(&json_str).context("parsing Apple AI response")?;

        Ok(ffi_to_response(ffi_resp))
    }
}
