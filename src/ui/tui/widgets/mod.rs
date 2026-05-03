pub mod chat;
pub mod completion;
pub mod dialog;
pub mod help;
pub mod history;
pub mod input;
pub mod markdown;
pub mod model_selector;
pub mod plan_list;
pub mod render_cache;
pub mod spinner;
pub mod status_bar;
pub mod tool_display;
pub mod wrap;

/// Truncate a string to `max_len` bytes, appending a Unicode ellipsis if truncated.
pub(crate) fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }
    let end = s.ceil_char_boundary(max_len);
    format!("{}…", &s[..end])
}
