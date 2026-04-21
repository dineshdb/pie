use tuirealm::ratatui::style::{Color, Modifier, Style};
use tuirealm::ratatui::text::{Line, Span};

/// Render a markdown string into ratatui [`Line`]s with styled spans.
#[allow(clippy::too_many_lines)]
pub fn render_markdown(text: &str, width: usize, base_color: Color) -> Vec<Line<'static>> {
    use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
    tracing::debug!(%text, "rendering");

    let mut lines: Vec<Line<'static>> = Vec::new();
    let parser = Parser::new_ext(text, Options::empty());

    let mut in_code_block = false;
    let mut code_block_lines: Vec<String> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut heading_level = 0u8;

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    heading_level = level as u8;
                }
                Tag::CodeBlock(_) => {
                    in_code_block = true;
                    code_block_lines.clear();
                }
                Tag::Strong => {
                    current_spans.push(Span::styled(
                        "",
                        Style::default().fg(base_color).add_modifier(Modifier::BOLD),
                    ));
                }
                Tag::Emphasis => {
                    current_spans.push(Span::styled(
                        "",
                        Style::default()
                            .fg(base_color)
                            .add_modifier(Modifier::ITALIC),
                    ));
                }
                Tag::Item => {
                    current_spans.push(Span::styled("  • ", Style::default().fg(Color::DarkGray)));
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Heading(_) => {
                    let line = Line::from(std::mem::take(&mut current_spans));
                    lines.push(line);
                    heading_level = 0;
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
                    let style = if heading_level > 0 {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(base_color)
                    };
                    push_text(&mut lines, &mut current_spans, &text, style);
                }
            }
            Event::Code(code) => {
                let code_style = Style::default().fg(Color::Green).bg(Color::Black);
                push_text(&mut lines, &mut current_spans, &code, code_style);
            }
            Event::SoftBreak | Event::HardBreak if !current_spans.is_empty() => {
                lines.push(Line::from(std::mem::take(&mut current_spans)));
            }
            _ => {}
        }
    }

    if !current_spans.is_empty() {
        lines.push(Line::from(current_spans));
    }

    // Post-process: guarantee no line exceeds width
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
    fn render_markdown_wraps_long_paragraph() {
        let input = "This is a very long paragraph that should definitely be wrapped at the specified width because it exceeds the terminal column count by a significant margin.";
        let lines = render_markdown(input, 40, Color::Gray);
        assert!(
            lines.len() > 1,
            "Should produce multiple wrapped lines, got {} lines",
            lines.len()
        );
        for line in &lines {
            let w = line.width();
            assert!(w <= 40, "Line exceeds width 40: width={w}");
        }
    }
}
