use tuirealm::ratatui::buffer::Buffer;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::style::{Color, Modifier, Style};
use tuirealm::ratatui::text::{Line, Span};
use tuirealm::ratatui::widgets::{Block, Borders, Paragraph, Widget};

const PROMPT: &str = "> ";

pub struct InputView<'a> {
    pub text_lines: &'a [String],
    pub cursor_row: usize,
    pub placeholder: &'a str,
    pub hint: &'a str,
    pub is_empty: bool,
    pub is_streaming: bool,
}

impl Widget for InputView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let border_style = if self.is_streaming {
            Style::default().fg(Color::Rgb(255, 140, 0))
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let has_hint = !self.hint.is_empty();
        #[allow(clippy::cast_possible_truncation)]
        let line_width = area.width.saturating_sub(5) as usize;

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

                spans.push(Span::raw(segment));
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

        Paragraph::new(rendered)
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(border_style),
            )
            .render(area, buf);
    }
}

pub fn cursor_position(area: Rect, cursor_row: usize, cursor_col: usize) -> (u16, u16) {
    let col_offset = if cursor_row == 0 { 2 } else { 0 };
    #[allow(clippy::cast_possible_truncation)]
    (
        area.x + cursor_col as u16 + col_offset,
        area.y + 1 + cursor_row as u16,
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
        };
        let buf = render_input(view, 30, 3);
        // Row 1 is the content row (row 0 is the top border)
        let content = row(&buf, 1);
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
        };
        let buf = render_input(view, 30, 3);
        let content = row(&buf, 1);
        assert!(content.contains('>'), "non-empty input should show prompt");
        assert!(content.contains("hello"), "input should show typed content");
    }

    #[test]
    fn streaming_input_has_colored_border() {
        let view = InputView {
            text_lines: &[String::new()],
            cursor_row: 0,
            placeholder: "",
            hint: "",
            is_empty: true,
            is_streaming: true,
        };
        let buf = render_input(view, 30, 3);
        // Top border should be orange (streaming color)
        let border_cell = &buf[(0, 0)];
        assert_eq!(
            border_cell.fg,
            Color::Rgb(255, 140, 0),
            "streaming border should be orange"
        );
    }

    #[test]
    fn cursor_position_accounts_for_prompt_offset() {
        let area = Rect::new(0, 0, 40, 5);
        // Row 0 with col 0 should offset by 2 (prompt "> ")
        let (x, y) = cursor_position(area, 0, 0);
        assert_eq!(x, 2, "first row cursor should offset for prompt");
        assert_eq!(y, 1, "cursor should be below top border");

        // Row 1+ should have no offset
        let (x, _) = cursor_position(area, 1, 5);
        assert_eq!(x, 5, "subsequent rows should have no prompt offset");
    }
}
