use crate::registry::{CompletionItem, CompletionKind};
use tuirealm::ratatui::buffer::Buffer;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::style::{Color, Modifier, Style};
use tuirealm::ratatui::text::{Line, Span};
use tuirealm::ratatui::widgets::{Paragraph, Widget};

const PROMPT: &str = "> ";

pub struct InputView<'a> {
    pub text_lines: &'a [String],
    pub cursor_row: usize,
    pub placeholder: &'a str,
    pub hint: &'a str,
    pub is_empty: bool,
    pub is_streaming: bool,
    pub completions: &'a [CompletionItem],
}

impl Widget for InputView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let has_hint = !self.hint.is_empty();
        #[allow(clippy::cast_possible_truncation)]
        let line_width = area.width.saturating_sub(3) as usize;

        let prompt_style = Style::default()
            .fg(if self.is_streaming {
                Color::Cyan
            } else {
                Color::Green
            })
            .add_modifier(Modifier::BOLD);

        let mut rendered: Vec<Line> = Vec::new();
        for (i, line) in self.text_lines.iter().enumerate() {
            let show_prompt = i == 0;
            let show_hint = i == self.cursor_row && has_hint && !self.is_empty;
            let show_placeholder = self.is_empty && i == 0;

            let wrapped = if line.is_empty() {
                vec![String::new()]
            } else {
                super::wrap::wrap_line(line, line_width)
            };

            for (j, segment) in wrapped.into_iter().enumerate() {
                let mut spans = Vec::new();
                if show_prompt && j == 0 {
                    spans.push(Span::styled(PROMPT, prompt_style));
                }

                spans.extend(highlight_line(&segment, self.completions));
                if show_placeholder && show_prompt && j == 0 {
                    spans.push(Span::styled(
                        self.placeholder,
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                if show_hint && show_prompt && j == 0 {
                    spans.push(Span::styled(
                        self.hint,
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    ));
                }
                rendered.push(Line::from(spans));
            }
        }

        Paragraph::new(rendered).render(area, buf);
    }
}

fn kind_color(kind: CompletionKind) -> Color {
    match kind {
        CompletionKind::Builtin => Color::Yellow,
        CompletionKind::Skill => Color::Cyan,
        CompletionKind::Agent => Color::Green,
    }
}

/// Highlight `/name` tokens that match known completions.
fn highlight_line(text: &str, completions: &[CompletionItem]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut last = 0;

    for (i, _) in text.char_indices().filter(|&(_, c)| c == '/') {
        if i > 0
            && !text
                .as_bytes()
                .get(i - 1)
                .is_some_and(u8::is_ascii_whitespace)
        {
            continue;
        }

        let word_end = text[i..]
            .find(|c: char| c.is_whitespace())
            .map_or(text.len(), |pos| i + pos);

        let token = &text[i..word_end];
        if let Some(item) = completions.iter().find(|c| c.label == token) {
            if i > last {
                spans.push(Span::raw(text[last..i].to_string()));
            }
            spans.push(Span::styled(
                token.to_string(),
                Style::default()
                    .fg(kind_color(item.kind))
                    .add_modifier(Modifier::BOLD),
            ));
            last = word_end;
        }
    }

    if last < text.len() {
        spans.push(Span::raw(text[last..].to_string()));
    }
    if spans.is_empty() {
        spans.push(Span::raw(text.to_string()));
    }
    spans
}

pub fn cursor_position(area: Rect, cursor_row: usize, cursor_col: usize) -> (u16, u16) {
    let col_offset = if cursor_row == 0 { 2 } else { 0 };
    #[allow(clippy::cast_possible_truncation)]
    (
        area.x + cursor_col as u16 + col_offset,
        area.y + cursor_row as u16,
    )
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use tuirealm::ratatui::Terminal;
    use tuirealm::ratatui::backend::TestBackend;

    fn render_input(view: InputView<'_>, width: u16, height: u16) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| f.render_widget(view, f.area())).unwrap();
        terminal.backend().buffer().clone()
    }

    fn row(buf: &Buffer, row: u16) -> String {
        (0..buf.area.width)
            .map(|col| buf[(col, row)].symbol().to_string())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn empty_input_shows_placeholder() {
        let view = InputView {
            text_lines: &[String::new()],
            cursor_row: 0,
            placeholder: "Type something",
            hint: "",
            is_empty: true,
            is_streaming: false,
            completions: &[],
        };
        let buf = render_input(view, 30, 3);
        let content = row(&buf, 0);
        assert!(
            content.contains("Type something"),
            "empty input should show placeholder, got: {content}"
        );
    }

    #[test]
    fn input_with_text_shows_prompt_and_content() {
        let view = InputView {
            text_lines: &["hello world".to_string()],
            cursor_row: 0,
            placeholder: "",
            hint: "",
            is_empty: false,
            is_streaming: false,
            completions: &[],
        };
        let buf = render_input(view, 30, 3);
        let content = row(&buf, 0);
        assert!(content.contains("hello"), "input should show typed content");
    }

    #[test]
    fn streaming_input_has_correct_styling() {
        let view = InputView {
            text_lines: &[String::new()],
            cursor_row: 0,
            placeholder: "",
            hint: "",
            is_empty: true,
            is_streaming: true,
            completions: &[],
        };
        let buf = render_input(view, 30, 3);
        let prompt_cell = &buf[(0, 0)];
        assert_eq!(
            prompt_cell.fg,
            Color::Cyan,
            "prompt should be cyan when streaming"
        );
    }

    #[test]
    fn cursor_position_accounts_for_prompt_offset() {
        let area = Rect::new(0, 0, 40, 5);
        let (x, y) = cursor_position(area, 0, 0);
        assert_eq!(x, 2, "first row cursor should offset for prompt");
        assert_eq!(y, 0, "cursor should be at row 0 (no border)");

        let (x, _) = cursor_position(area, 1, 5);
        assert_eq!(x, 5, "subsequent rows should have no prompt offset");
    }

    #[test]
    fn highlight_skill_name_at_start() {
        let completions = vec![CompletionItem {
            label: "/review".to_string(),
            description: String::new(),
            kind: CompletionKind::Skill,
        }];
        let spans = highlight_line("/review fix the bug", &completions);
        assert_eq!(spans.len(), 2, "should split into skill + rest");
        assert_eq!(spans[0].content, "/review");
        assert_eq!(spans[1].content, " fix the bug");
    }

    #[test]
    fn highlight_unknown_slash_not_styled() {
        let spans = highlight_line("/unknown thing", &[]);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "/unknown thing");
    }

    #[test]
    fn highlight_mid_line_skill() {
        let completions = vec![CompletionItem {
            label: "/debug".to_string(),
            description: String::new(),
            kind: CompletionKind::Agent,
        }];
        let spans = highlight_line("use /debug now", &completions);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].content, "use ");
        assert_eq!(spans[1].content, "/debug");
        assert_eq!(spans[2].content, " now");
    }

    #[test]
    fn slash_not_at_word_boundary_not_matched() {
        let completions = vec![CompletionItem {
            label: "/debug".to_string(),
            description: String::new(),
            kind: CompletionKind::Agent,
        }];
        let spans = highlight_line("foo/debug", &completions);
        assert_eq!(spans.len(), 1, "embedded /debug should not match");
    }
}
