use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use tuirealm::ratatui::style::{Color, Modifier, Style};
use tuirealm::ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// Render a markdown string into ratatui [`Line`]s with styled spans.
/// This implementation wraps text while parsing and preserves styles.
pub fn render_markdown(text: &str, width: usize, base_color: Color) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }

    let mut renderer = MarkdownRenderer::new(width, base_color);
    let parser = Parser::new_ext(text, Options::all());

    for event in parser {
        renderer.handle_event(event);
    }

    renderer.finish()
}

struct MarkdownRenderer {
    width: usize,
    lines: Vec<Line<'static>>,
    current_line: Vec<Span<'static>>,
    current_width: usize,
    style_stack: Vec<Style>,
    in_code_block: bool,
    list_level: usize,
}

impl MarkdownRenderer {
    fn new(width: usize, base_color: Color) -> Self {
        let base_style = Style::default().fg(base_color);
        Self {
            width,
            lines: Vec::new(),
            current_line: Vec::new(),
            current_width: 0,
            style_stack: vec![base_style],
            in_code_block: false,
            list_level: 0,
        }
    }

    fn current_style(&self) -> Style {
        *self.style_stack.last().unwrap_or(&Style::default())
    }

    fn push_line(&mut self) {
        if !self.current_line.is_empty() {
            self.lines
                .push(Line::from(std::mem::take(&mut self.current_line)));
            self.current_width = 0;
        } else if !self.lines.is_empty() {
            // Keep empty lines if they are between content
            self.lines.push(Line::raw(""));
        }
    }

    fn handle_event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start_tag(&tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(text) => self.push_text(&text),
            Event::Code(code) => {
                let code_style = self.current_style().fg(Color::Green).bg(Color::Black);
                self.push_text_styled(&code, code_style);
            }
            Event::SoftBreak if self.current_width > 0 => {
                self.push_text(" ");
            }
            Event::HardBreak => {
                self.push_line();
            }
            Event::Rule => {
                self.push_line();
                self.push_text_styled(
                    &"─".repeat(self.width),
                    self.current_style().fg(Color::DarkGray),
                );
                self.push_line();
            }
            _ => {}
        }
    }

    fn start_tag(&mut self, tag: &Tag) {
        match tag {
            Tag::Heading { .. } => {
                self.push_line();
                let style = self
                    .current_style()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD);
                self.style_stack.push(style);
            }
            Tag::CodeBlock(_) => {
                self.push_line();
                self.in_code_block = true;
                let style = Style::default().fg(Color::Green).bg(Color::Black);
                self.style_stack.push(style);
            }
            Tag::Strong => {
                let style = self.current_style().add_modifier(Modifier::BOLD);
                self.style_stack.push(style);
            }
            Tag::Emphasis => {
                let style = self.current_style().add_modifier(Modifier::ITALIC);
                self.style_stack.push(style);
            }
            Tag::Link { .. } => {
                let style = self
                    .current_style()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::UNDERLINED);
                self.style_stack.push(style);
            }
            Tag::List(_) => {
                self.list_level += 1;
            }
            Tag::Item => {
                self.push_line();
                let indent = "  ".repeat(self.list_level.saturating_sub(1));
                self.push_text(&format!("{indent}• "));
            }
            Tag::Paragraph if !self.lines.is_empty() || !self.current_line.is_empty() => {
                self.push_line();
            }
            Tag::BlockQuote(_) => {
                self.push_line();
                let style = self
                    .current_style()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC);
                self.style_stack.push(style);
                self.push_text("│ ");
            }
            _ => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Heading(_)
            | TagEnd::Strong
            | TagEnd::Emphasis
            | TagEnd::Link
            | TagEnd::BlockQuote(_) => {
                self.style_stack.pop();
            }
            TagEnd::CodeBlock => {
                self.in_code_block = false;
                self.style_stack.pop();
                self.push_line();
            }
            TagEnd::List(_) => {
                self.list_level = self.list_level.saturating_sub(1);
            }
            TagEnd::Paragraph | TagEnd::Item => {
                self.push_line();
            }
            _ => {}
        }
    }

    fn push_text(&mut self, text: &str) {
        let style = self.current_style();
        self.push_text_styled(text, style);
    }

    fn push_text_styled(&mut self, text: &str, style: Style) {
        if self.in_code_block {
            for line in text.lines() {
                // Code blocks are special, we might want to wrap them but differently
                // For now, simple wrap
                self.wrap_and_push(line, style, true);
                self.push_line();
            }
            return;
        }

        // Split by lines first to handle explicit newlines in text
        let mut first = true;
        for line in text.split('\n') {
            if !first {
                self.push_line();
            }
            first = false;

            if line.is_empty() {
                continue;
            }

            self.wrap_and_push(line, style, false);
        }
    }

    fn wrap_and_push(&mut self, text: &str, style: Style, is_code: bool) {
        let words = if is_code {
            // In code blocks, we don't split by whitespace, we just wrap by width
            vec![text]
        } else {
            // Split by whitespace but keep track of it
            text.split_inclusive(char::is_whitespace)
                .collect::<Vec<_>>()
        };

        for word in words {
            let word_width = word.width();

            if self.current_width + word_width > self.width && self.current_width > 0 {
                // Word doesn't fit, start new line
                // If it's just whitespace at the start of a new line, skip it
                if word.chars().all(char::is_whitespace) {
                    continue;
                }
                self.push_line();
            }

            if word_width > self.width {
                // Word is longer than the whole width, must break it
                let mut remaining = word;
                while !remaining.is_empty() {
                    let mut break_idx = remaining.len();
                    let current_word_width = UnicodeWidthStr::width(remaining);

                    if current_word_width > self.width - self.current_width {
                        // Find where to break
                        for (i, _) in remaining.char_indices().rev() {
                            let sub = &remaining[..i];
                            if UnicodeWidthStr::width(sub) <= self.width - self.current_width {
                                break_idx = i;
                                break;
                            }
                        }
                        if break_idx == 0 {
                            // Can't even fit one char?
                            self.push_line();
                            // Try again with full width
                            continue;
                        }
                    }

                    let part = &remaining[..break_idx];
                    self.current_line
                        .push(Span::styled(part.to_string(), style));
                    self.current_width += UnicodeWidthStr::width(part);
                    remaining = &remaining[break_idx..];

                    if !remaining.is_empty() {
                        self.push_line();
                    }
                }
            } else {
                self.current_line
                    .push(Span::styled(word.to_string(), style));
                self.current_width += word_width;
            }
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        if !self.current_line.is_empty() {
            self.lines.push(Line::from(self.current_line));
        }
        self.lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_markdown_handles_nested_styles() {
        let input = "This is **bold** and *italic* text.";
        let lines = render_markdown(input, 80, Color::Gray);
        let first_line = lines.first().expect("Should have at least one line");

        let bold_span = first_line.spans.iter().find(|s| s.content.contains("bold"));
        assert!(bold_span.is_some_and(|s| s.style.add_modifier.contains(Modifier::BOLD)));
    }

    #[test]
    fn render_markdown_wraps_correctly() {
        let input = "This is a long sentence that should be wrapped into multiple lines.";
        let lines = render_markdown(input, 20, Color::Gray);
        assert!(lines.len() > 1);
        for line in &lines {
            assert!(line.width() <= 20);
        }
    }

    #[test]
    fn render_markdown_preserves_style_across_wrap() {
        let input = "This is a **very long bold sentence that must wrap** somewhere.";
        let lines = render_markdown(input, 20, Color::Gray);

        let mut bold_found_on_multiple_lines = 0;
        for line in &lines {
            if line
                .spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::BOLD))
            {
                bold_found_on_multiple_lines += 1;
            }
        }
        assert!(bold_found_on_multiple_lines >= 2);
    }
}
