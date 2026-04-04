//! Raw FFI declarations for Apple Foundation Models.
//!
//! These declarations bridge Rust to the Swift runtime that wraps
//! Apple's on-device Foundation Models API. The actual Swift static
//! library is provided by the `apple_ai` crate.

use std::os::raw::{c_char, c_double, c_int};

/// Callback type for streaming chunks from Apple Foundation Models.
pub type AppleAIChunkCallback = unsafe extern "C" fn(chunk: *const c_char);

/// Callback type for tool calls from Apple Foundation Models.
#[allow(dead_code)]
pub type AppleAIToolCallback = unsafe extern "C" fn(tool_id: u64, args_json: *const c_char);

#[link(name = "apple_ai_bridge")]
extern "C" {
    /// Initialize the Apple AI bridge. Returns true on success.
    pub fn apple_ai_init() -> bool;

    /// Eagerly initialize SystemLanguageModel.default so the first inference is fast.
    pub fn apple_ai_prewarm() -> bool;

    /// Check availability of Apple Intelligence.
    /// Returns: 1 = available, -1 = not eligible, -2 = not enabled, -3 = model not ready.
    pub fn apple_ai_check_availability() -> i32;

    /// Get a human-readable reason for unavailability.
    pub fn apple_ai_get_availability_reason() -> *mut c_char;

    /// Free a string returned by the Apple AI bridge.
    pub fn apple_ai_free_string(ptr: *mut c_char);

    /// Register a callback for tool calls.
    #[allow(dead_code)]
    pub fn apple_ai_register_tool_callback(callback: Option<AppleAIToolCallback>);

    /// Submit a tool result back to the model.
    #[allow(dead_code)]
    pub fn apple_ai_tool_result_callback(tool_id: u64, result_json: *const c_char);

    /// Generate a response using Apple Foundation Models.
    ///
    /// Returns a JSON string that must be freed with `apple_ai_free_string`.
    #[allow(clippy::too_many_arguments)]
    pub fn apple_ai_generate_unified(
        messages_json: *const c_char,
        tools_json: *const c_char,
        schema_json: *const c_char,
        temperature: c_double,
        max_tokens: c_int,
        stream: bool,
        stop_after_tool_calls: bool,
        on_chunk: Option<AppleAIChunkCallback>,
    ) -> *mut c_char;
}

/// Helper: convert a raw pointer from the bridge into a Rust String, freeing the original.
///
/// # Safety
/// Caller must ensure `ptr` was returned by an Apple AI bridge function.
pub unsafe fn ptr_to_string(ptr: *mut c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let c_str = std::ffi::CStr::from_ptr(ptr);
    let s = c_str.to_string_lossy().into_owned();
    apple_ai_free_string(ptr);
    Some(s)
}
