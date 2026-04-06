use aisdk::core::language_model::{LanguageModelResponse, LanguageModelResponseContentType};
use aisdk::core::tools::ToolCallInfo;

/// Post-process a language model response to extract inline tool calls.
///
/// Some local model servers (e.g., MLX serving gemma-4) don't parse the model's
/// native tool call tokens into structured OpenAI-format `tool_calls`. Instead,
/// the raw model output like `<|tool_call>call:name{args}<tool_call|><eos>` is
/// returned as plain text in the `content` field.
///
/// This function detects such inline tool calls in text content and converts
/// them into proper `LanguageModelResponseContentType::ToolCall` entries.
pub fn post_process_response(mut response: LanguageModelResponse) -> LanguageModelResponse {
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
                    let query = info
                        .input
                        .get("query")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
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
    if !text.contains("<|tool_call>") && !text.contains("call:") {
        return None;
    }

    let mut results = Vec::new();
    let mut search_text = text;

    while let Some(start) = search_text.find("<|tool_call>") {
        let after_marker = &search_text[start + "<|tool_call>".len()..];
        let rest = after_marker.strip_prefix("call:")?;
        let end = rest
            .find("<tool_call|>")
            .or_else(|| rest.find("<|tool_call|>"))?;

        let call_body = &rest[..end];

        if let Some(open_brace) = call_body.find('{') {
            let name = &call_body[..open_brace];
            let args_str = &call_body[open_brace..];
            let close_brace = find_matching_brace(args_str)?;
            let args_json_str = &args_str[..=close_brace];
            let normalized = normalize_tool_args(args_json_str);

            let input = serde_json::from_str(&normalized)
                .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));

            let (tool_name, input) = if matches!(name, "subagent" | "shell_tool") {
                (name.to_string(), input)
            } else {
                let query = input.get("query").and_then(|v| v.as_str()).unwrap_or("");
                (
                    "subagent".to_string(),
                    serde_json::json!({
                        "skill_name": name,
                        "query": query,
                    }),
                )
            };

            let mut info = ToolCallInfo::new(&tool_name);
            info.input(input);
            results.push(LanguageModelResponseContentType::ToolCall(info));
        }

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

fn normalize_tool_args(s: &str) -> String {
    let s = s
        .replace("<|\"|>", "\"")
        .replace("<eos>", "")
        .replace("<|eos|>", "");

    if serde_json::from_str::<serde_json::Value>(&s).is_ok() {
        return s;
    }

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
            while let Some(&next) = chars.peek() {
                if next.is_alphanumeric() || next == '_' {
                    buffer.push(chars.next().unwrap());
                } else {
                    break;
                }
            }
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
