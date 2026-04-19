use unicode_width::UnicodeWidthStr;

/// Word-wrap a single line to `width` columns, respecting unicode widths.
pub fn wrap_line(line: &str, width: usize) -> Vec<String> {
    if width == 0 || line.is_empty() {
        return vec![line.to_string()];
    }

    let line_width = UnicodeWidthStr::width(line);
    if line_width <= width {
        return vec![line.to_string()];
    }

    let mut result = Vec::new();
    let mut current = String::new();
    let mut current_len: usize = 0;

    for word in line.split_whitespace() {
        let word_len = word.width();
        if current_len == 0 {
            current.push_str(word);
            current_len = word_len;
        } else if current_len + 1 + word_len <= width {
            current.push(' ');
            current.push_str(word);
            current_len += 1 + word_len;
        } else {
            result.push(current.clone());
            current.clear();
            current.push_str(word);
            current_len = word_len;
        }
    }

    if !current.is_empty() {
        result.push(current);
    }

    if result.is_empty() {
        result.push(String::new());
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_line_splits_long_text() {
        let input = "This is a very long line of text that should be wrapped";
        let result = wrap_line(input, 20);
        for line in &result {
            assert!(
                line.len() <= 20,
                "Line exceeds width: [{line}] (len={})",
                line.len()
            );
        }
        assert!(result.len() > 1, "Should produce multiple lines");
    }

    #[test]
    fn wrap_line_short_text_unchanged() {
        let input = "short";
        let result = wrap_line(input, 20);
        assert_eq!(result, vec!["short"]);
    }

    #[test]
    fn wrap_line_empty() {
        let result = wrap_line("", 20);
        assert_eq!(result, vec![""]);
    }
}
