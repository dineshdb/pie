use crate::afm::AppleClient;
use aisdk::core::capabilities::{
    AudioInputSupport, AudioOutputSupport, ImageInputSupport, ImageOutputSupport, ReasoningSupport,
    StructuredOutputSupport, TextInputSupport, TextOutputSupport, ToolCallSupport,
    VideoInputSupport, VideoOutputSupport,
};
use aisdk::core::language_model::{
    LanguageModel, LanguageModelOptions, LanguageModelResponse, LanguageModelResponseContentType,
    ProviderStream,
};
use aisdk::core::tools::ToolCallInfo;
use aisdk::core::DynamicModel;
use aisdk::providers::OpenAICompatible;
use anyhow::{Context, Result};
use async_trait::async_trait;

/// Resolved model provider — both variants implement `LanguageModel`.
#[derive(Debug, Clone)]
pub enum Model {
    Apple(AppleClient),
    OpenAI(OpenAICompatible<DynamicModel>),
}

// Delegate all capability marker traits
impl TextInputSupport for Model {}
impl TextOutputSupport for Model {}
impl ToolCallSupport for Model {}
impl StructuredOutputSupport for Model {}
impl ReasoningSupport for Model {}
impl ImageInputSupport for Model {}
impl ImageOutputSupport for Model {}
impl VideoInputSupport for Model {}
impl AudioInputSupport for Model {}
impl AudioOutputSupport for Model {}
impl VideoOutputSupport for Model {}

#[async_trait]
impl LanguageModel for Model {
    fn name(&self) -> String {
        match self {
            Model::Apple(c) => c.name(),
            Model::OpenAI(p) => <OpenAICompatible<DynamicModel> as LanguageModel>::name(p),
        }
    }

    async fn generate_text(
        &mut self,
        options: LanguageModelOptions,
    ) -> aisdk::error::Result<LanguageModelResponse> {
        let response = match self {
            Model::Apple(c) => c.generate_text(options).await?,
            Model::OpenAI(p) => p.generate_text(options).await?,
        };
        Ok(post_process_response(response))
    }

    async fn stream_text(
        &mut self,
        options: LanguageModelOptions,
    ) -> aisdk::error::Result<ProviderStream> {
        match self {
            Model::Apple(c) => c.stream_text(options).await,
            Model::OpenAI(p) => p.stream_text(options).await,
        }
    }
}

/// Build a model from CLI args + env vars.
///
/// Priority: CLI arg > `PIE_*` env > provider-specific env > default.
pub fn build_model(
    model: Option<&str>,
    base_url: Option<&str>,
    api_key: Option<&str>,
) -> Result<Model> {
    let model_name = model
        .map(|s| s.to_string())
        .or_else(|| std::env::var("PIE_MODEL").ok())
        .or_else(|| std::env::var("OPENAI_MODEL").ok());

    // If no model specified, try Apple
    if model_name.is_none() {
        match AppleClient::new() {
            Ok(client) => {
                tracing::debug!("using Apple Foundation Models");
                return Ok(Model::Apple(client));
            }
            Err(e) => {
                anyhow::bail!(
                    "No model specified and Apple Intelligence unavailable: {e}\n\
                     Set --model or PIE_MODEL to use an OpenAI-compatible provider."
                );
            }
        }
    }

    let model_name = model_name.unwrap();

    // Resolve base URL
    let base_url = base_url
        .map(|s| s.to_string())
        .or_else(|| std::env::var("PIE_BASE_URL").ok())
        .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
        .or_else(|| std::env::var("OPENAI_API_BASE").ok())
        .or_else(|| ollama_default(&model_name))
        .context("base URL is required (set --base-url, PIE_BASE_URL, or OPENAI_BASE_URL)")?;

    // Resolve API key
    let api_key = api_key
        .map(|s| s.to_string())
        .or_else(|| std::env::var("PIE_API_KEY").ok())
        .or_else(|| std::env::var("OPENAI_API_KEY").ok())
        .or_else(|| local_placeholder(&base_url))
        .context("API key is required (set --api-key, PIE_API_KEY, or OPENAI_API_KEY)")?;

    let provider = OpenAICompatible::<DynamicModel>::builder()
        .model_name(&model_name)
        .base_url(&base_url)
        .api_key(&api_key)
        .build()
        .context("failed to build OpenAI-compatible provider")?;

    tracing::debug!(model = %model_name, base_url = %base_url, "using OpenAI-compatible provider");

    Ok(Model::OpenAI(provider))
}

/// Well-known local model prefixes that default to Ollama.
fn ollama_default(model: &str) -> Option<String> {
    const LOCAL_PREFIXES: &[&str] = &[
        "llama",
        "mistral",
        "phi",
        "codellama",
        "qwen",
        "deepseek",
        "gemma",
    ];
    if LOCAL_PREFIXES.iter().any(|p| model.starts_with(p)) {
        Some("http://localhost:11434/v1".to_string())
    } else {
        None
    }
}

/// Localhost servers don't need a real key — use a placeholder.
fn local_placeholder(base_url: &str) -> Option<String> {
    if base_url.contains("localhost") || base_url.contains("127.0.0.1") {
        Some("ollama".to_string())
    } else {
        None
    }
}

/// Post-process a language model response to extract inline tool calls.
///
/// Some local model servers (e.g., MLX serving gemma-4) don't parse the model's
/// native tool call tokens into structured OpenAI-format `tool_calls`. Instead,
/// the raw model output like `<|tool_call>call:name{args}<tool_call|><eos>` is
/// returned as plain text in the `content` field.
///
/// This function detects such inline tool calls in text content and converts
/// them into proper `LanguageModelResponseContentType::ToolCall` entries.
fn post_process_response(mut response: LanguageModelResponse) -> LanguageModelResponse {
    let has_structured_tool_calls = response
        .contents
        .iter()
        .any(|c| matches!(c, LanguageModelResponseContentType::ToolCall(_)));

    let mut new_contents = Vec::new();
    for content in response.contents {
        match content {
            LanguageModelResponseContentType::Text(ref text) => {
                if !has_structured_tool_calls {
                    if let Some(calls) = extract_inline_tool_calls(text) {
                        new_contents.extend(calls);
                        continue;
                    }
                }
                new_contents.push(content);
            }
            LanguageModelResponseContentType::ToolCall(ref info) => {
                let name = info.tool.name.as_str();
                if matches!(name, "subagent" | "shell_tool") {
                    new_contents.push(content);
                } else {
                    // Remap unknown tool names (skill names) to subagent
                    let query = info.input.get("query").and_then(|v| v.as_str()).unwrap_or("");
                    let mut remapped = ToolCallInfo::new("subagent");
                    remapped.input(serde_json::json!({
                        "skill_name": name,
                        "query": query,
                    }));
                    new_contents.push(LanguageModelResponseContentType::ToolCall(remapped));
                }
            }
            other => new_contents.push(other),
        }
    }

    response.contents = new_contents;
    response
}

/// Extract tool calls from inline text like:
/// `<|tool_call>call:name{key:"value"}<tool_call|><eos>`
fn extract_inline_tool_calls(text: &str) -> Option<Vec<LanguageModelResponseContentType>> {
    // Fast check: if text doesn't contain tool_call markers, skip.
    if !text.contains("<|tool_call>") && !text.contains("call:") {
        return None;
    }

    let mut results = Vec::new();
    let mut search_text = text;

    while let Some(start) = search_text.find("<|tool_call>") {
        let after_marker = &search_text[start + "<|tool_call>".len()..];

        // Expect "call:" prefix
        let rest = after_marker.strip_prefix("call:")?;

        // Find the end marker — try both <|tool_call|> and <tool_call|>
        let end = rest
            .find("<tool_call|>")
            .or_else(|| rest.find("<|tool_call|>"))?;

        let call_body = &rest[..end];

        // Parse "name{args}" format
        if let Some(open_brace) = call_body.find('{') {
            let name = &call_body[..open_brace];
            let args_str = &call_body[open_brace..];

            // Find matching closing brace
            let close_brace = find_matching_brace(args_str)?;

            let args_json_str = &args_str[..=close_brace];

            // Normalize the args: gemma may output <|"|> for escaped quotes
            let normalized = normalize_tool_args(args_json_str);

            let input = serde_json::from_str(&normalized)
                .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));

            // Models like gemma call skill names directly (e.g. "web-search")
            // instead of using the registered "subagent" tool. Remap them.
            let (tool_name, input) = if matches!(name, "subagent" | "shell_tool") {
                (name.to_string(), input)
            } else {
                // Remap to subagent: name → skill_name, keep existing query
                let query = input.get("query").and_then(|v| v.as_str()).unwrap_or("");
                let mapped = serde_json::json!({
                    "skill_name": name,
                    "query": query,
                });
                ("subagent".to_string(), mapped)
            };

            let mut info = ToolCallInfo::new(&tool_name);
            info.input(input);

            results.push(LanguageModelResponseContentType::ToolCall(info));
        }

        // Advance past this tool call
        let consumed = start + "<|tool_call>".len() + "call:".len() + end + "<tool_call|>".len();
        search_text = if consumed < text.len() {
            &text[consumed.min(text.len())..]
        } else {
            break;
        };
    }

    if results.is_empty() {
        None
    } else {
        Some(results)
    }
}

/// Find the matching closing brace for the first `{` in the string.
/// Returns the byte offset of the matching `}`.
fn find_matching_brace(s: &str) -> Option<usize> {
    let mut depth = 0;
    let mut in_string = false;
    let mut escape_next = false;
    let mut byte_offset = 0;

    for ch in s.chars() {
        let char_len = ch.len_utf8();
        if escape_next {
            escape_next = false;
            byte_offset += char_len;
            continue;
        }
        match ch {
            '\\' if in_string => escape_next = true,
            '"' => in_string = !in_string,
            '{' if !in_string => depth += 1,
            '}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(byte_offset);
                }
            }
            _ => {}
        }
        byte_offset += char_len;
    }
    None
}

/// Normalize gemma-style tool call arguments.
/// Handles `<|"|>` (gemma's escaped quote token), unquoted keys, and other formatting quirks.
fn normalize_tool_args(s: &str) -> String {
    // First replace gemma's escaped quote tokens
    let s = s
        .replace("<|\"|>", "\"")
        .replace("<eos>", "")
        .replace("<|eos|>", "");

    // Try parsing as-is first (might already be valid JSON)
    if serde_json::from_str::<serde_json::Value>(&s).is_ok() {
        return s;
    }

    // Fix unquoted keys: match word characters followed by colon (not inside a string)
    // Pattern: `key:"value"` -> `"key":"value"` and `key: "value"` -> `"key": "value"`
    let mut result = String::with_capacity(s.len() + 20);
    let mut in_string = false;
    let mut chars = s.chars().peekable();
    let mut buffer = String::new();

    while let Some(ch) = chars.next() {
        if ch == '"' {
            in_string = !in_string;
            result.push(ch);
            continue;
        }
        if in_string {
            result.push(ch);
            continue;
        }

        if ch.is_alphanumeric() || ch == '_' {
            buffer.push(ch);
            // Keep collecting identifier chars
            while let Some(&next) = chars.peek() {
                if next.is_alphanumeric() || next == '_' {
                    buffer.push(chars.next().unwrap());
                } else {
                    break;
                }
            }
            // Check if followed by a colon — if so, it's a key
            if chars.peek() == Some(&':') {
                result.push('"');
                result.push_str(&buffer);
                result.push('"');
                buffer.clear();
            } else {
                result.push_str(&buffer);
                buffer.clear();
            }
        } else {
            result.push(ch);
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_inline_tool_call_basic() {
        let text = r#"<|tool_call>call:subagent{skill_name:<|"|>web-search<|"|>,query:<|"|>current prime minister of Nepal<|"|>}<tool_call|><eos>"#;
        let result = extract_inline_tool_calls(text);
        assert!(result.is_some());
        let calls = result.unwrap();
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn test_extract_remaps_skill_name_to_subagent() {
        // gemma calls "web-search" directly instead of "subagent"
        let text = r#"<|tool_call>call:web-search{query:<|"|>current prime minister of Nepal<|"|>}<tool_call|><eos>"#;
        let calls = extract_inline_tool_calls(text).unwrap();

        if let LanguageModelResponseContentType::ToolCall(info) = &calls[0] {
            assert_eq!(info.tool.name, "subagent");
            assert_eq!(info.input["skill_name"], "web-search");
            assert_eq!(info.input["query"], "current prime minister of Nepal");
        } else {
            panic!("expected ToolCall");
        }
    }

    #[test]
    fn test_extract_inline_no_tool_calls() {
        let text = "Just a regular response with no tool calls.";
        let result = extract_inline_tool_calls(text);
        assert!(result.is_none());
    }

    #[test]
    fn test_find_matching_brace_simple() {
        assert_eq!(find_matching_brace("{}}"), Some(1));
        assert_eq!(find_matching_brace(r#"{"key": "value"}"#), Some(15));
    }

    #[test]
    fn test_find_matching_brace_nested() {
        assert_eq!(find_matching_brace(r#"{"a": {"b": 1}}"#), Some(14));
    }

    #[test]
    fn test_normalize_tool_args() {
        let input = r#"{skill_name:<|"|>web-search<|"|>,query:<|"|>test<|"|>}"#;
        let normalized = normalize_tool_args(input);
        assert_eq!(normalized, r#"{"skill_name":"web-search","query":"test"}"#);
    }

    #[test]
    fn test_post_process_leaves_normal_response_untouched() {
        let response = LanguageModelResponse {
            contents: vec![LanguageModelResponseContentType::Text(
                "Hello, world!".to_string(),
            )],
            usage: None,
        };
        let processed = post_process_response(response);
        assert_eq!(processed.contents.len(), 1);
        assert!(matches!(
            &processed.contents[0],
            LanguageModelResponseContentType::Text(t) if t == "Hello, world!"
        ));
    }

    #[test]
    fn test_post_process_remaps_structured_skill_tool_call() {
        // Model calls "repo" directly as a structured tool call instead of "subagent"
        let mut info = ToolCallInfo::new("repo");
        info.input(serde_json::json!({"query": "context"}));
        let response = LanguageModelResponse {
            contents: vec![LanguageModelResponseContentType::ToolCall(info)],
            usage: None,
        };
        let processed = post_process_response(response);
        assert_eq!(processed.contents.len(), 1);
        if let LanguageModelResponseContentType::ToolCall(remapped) = &processed.contents[0] {
            assert_eq!(remapped.tool.name, "subagent");
            assert_eq!(remapped.input["skill_name"], "repo");
            assert_eq!(remapped.input["query"], "context");
        } else {
            panic!("expected ToolCall");
        }
    }
}
