use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use tuirealm::ratatui::style::{Color, Modifier, Style};
use tuirealm::ratatui::text::{Line, Span};

/// Render a markdown string into ratatui [`Line`]s with styled spans.
#[allow(clippy::too_many_lines)]
pub fn render_markdown(text: &str, width: usize, base_color: Color) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let parser = Parser::new_ext(text, Options::empty());

    let mut in_code_block = false;
    let mut code_block_lines: Vec<String> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut current_style = Style::default().fg(base_color);
    let mut style_stack: Vec<Style> = vec![current_style];

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { .. } => {
                    current_style = current_style.fg(Color::Cyan).add_modifier(Modifier::BOLD);
                    style_stack.push(current_style);
                }
                Tag::CodeBlock(_) => {
                    in_code_block = true;
                    code_block_lines.clear();
                }
                Tag::Strong => {
                    current_style = current_style.add_modifier(Modifier::BOLD);
                    style_stack.push(current_style);
                }
                Tag::Emphasis => {
                    current_style = current_style.add_modifier(Modifier::ITALIC);
                    style_stack.push(current_style);
                }
                Tag::Item => {
                    current_spans.push(Span::styled("  • ", Style::default().fg(Color::DarkGray)));
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Heading(_) => {
                    style_stack.pop();
                    current_style = *style_stack
                        .last()
                        .unwrap_or(&Style::default().fg(base_color));
                    if !current_spans.is_empty() {
                        let line = Line::from(std::mem::take(&mut current_spans));
                        lines.push(line);
                    }
                }
                TagEnd::CodeBlock => {
                    in_code_block = false;
                    for code_line in &code_block_lines {
                        for wl in super::wrap::wrap_line(code_line, width.saturating_sub(4)) {
                            lines.push(Line::from(vec![
                                Span::raw("  "),
                                Span::styled(
                                    wl,
                                    Style::default().fg(Color::Green).bg(Color::Black),
                                ),
                            ]));
                        }
                    }
                    code_block_lines.clear();
                }
                TagEnd::Strong | TagEnd::Emphasis => {
                    style_stack.pop();
                    current_style = *style_stack
                        .last()
                        .unwrap_or(&Style::default().fg(base_color));
                }
                TagEnd::Paragraph | TagEnd::Item if !current_spans.is_empty() => {
                    let line = Line::from(std::mem::take(&mut current_spans));
                    lines.push(line);
                }
                _ => {}
            },
            Event::Text(text) => {
                if in_code_block {
                    code_block_lines.extend(text.lines().map(ToString::to_string));
                } else {
                    push_text(&mut lines, &mut current_spans, &text, current_style);
                }
            }
            Event::Code(code) => {
                let code_style = current_style.fg(Color::Green).bg(Color::Black);
                push_text(&mut lines, &mut current_spans, &code, code_style);
            }
            Event::SoftBreak if !current_spans.is_empty() => {
                current_spans.push(Span::styled(" ", current_style));
            }
            Event::HardBreak if !current_spans.is_empty() => {
                lines.push(Line::from(std::mem::take(&mut current_spans)));
            }
            _ => {}
        }
    }

    if !current_spans.is_empty() {
        lines.push(Line::from(current_spans));
    }

    let mut result = Vec::with_capacity(lines.len());
    for line in lines {
        if line.width() <= width {
            result.push(line);
        } else {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            let style = line.spans.first().map(|s| s.style).unwrap_or_default();
            for w in super::wrap::wrap_line(&text, width) {
                result.push(Line::from(Span::styled(w, style)));
            }
        }
    }

    result
}

/// Push text into `current_spans`, splitting on newlines into separate Lines.
fn push_text(
    lines: &mut Vec<Line<'static>>,
    current_spans: &mut Vec<Span<'static>>,
    text: &str,
    style: Style,
) {
    for (i, sub_line) in text.split('\n').enumerate() {
        if i > 0 && !current_spans.is_empty() {
            lines.push(Line::from(std::mem::take(current_spans)));
        }
        current_spans.push(Span::styled(sub_line.to_string(), style));
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
        let spans: Vec<_> = first_line.spans.iter().collect();

        // We expect "This is ", "bold", " and ", "italic", " text."
        // Let's check if the bold one actually has the bold modifier.
        let bold_span = spans.iter().find(|s| s.content == "bold");
        assert!(bold_span.is_some_and(|s| s.style.add_modifier.contains(Modifier::BOLD)));
    }

    #[test]
    fn render_markdown_handles_soft_breaks() {
        let input = "Line one\nLine two";
        let lines = render_markdown(input, 80, Color::Gray);
        // Soft breaks should probably be spaces, but currently they are new lines.
        // If they are new lines, lines.len() will be 2.
        assert_eq!(
            lines.len(),
            1,
            "Soft breaks should not necessarily create new lines in the output if we want flowing text"
        );
    }
}
