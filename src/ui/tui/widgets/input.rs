use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

const PROMPT: &str = "> ";

pub struct InputView<'a> {
    pub text_lines: Vec<String>,
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
                if show_placeholder && j == 0 {
                    spans.push(Span::styled(
                        self.placeholder,
                        Style::default().fg(Color::DarkGray),
                    ));
                } else {
                    spans.push(Span::raw(segment));
                    if show_hint && show_prompt && j == 0 {
                        spans.push(Span::styled(
                            self.hint,
                            Style::default()
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::ITALIC),
                        ));
                    }
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
