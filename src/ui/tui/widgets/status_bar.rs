use crate::ui::tui::widgets::spinner::Spinner;
use tuirealm::ratatui::buffer::Buffer;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::style::{Color, Modifier, Style};
use tuirealm::ratatui::text::Span;
use tuirealm::ratatui::widgets::Widget;

pub struct StatusBar {
    pub active_steps: Vec<String>,
    pub is_streaming: bool,
    pub spinner_frame: usize,
}

impl StatusBar {
    pub fn new(active_steps: Vec<String>, is_streaming: bool, spinner_frame: usize) -> Self {
        Self {
            active_steps,
            is_streaming,
            spinner_frame,
        }
    }
}

impl Widget for StatusBar {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let style = if self.is_streaming {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else if !self.active_steps.is_empty() {
            Style::default().fg(Color::Green)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        // 1. Render Spinner
        if self.is_streaming {
            let spinner = Spinner::new(self.spinner_frame).style(style);
            spinner.render(Rect::new(area.x, area.y, 1, 1), buf);
        }

        // 2. Render Plan Title(s)
        let title = if self.active_steps.is_empty() {
            "PIE".to_string()
        } else {
            self.active_steps.join(" › ")
        };
        let title_span = Span::styled(format!(" {title} "), style);

        title_span.render(
            Rect::new(
                area.x + u16::from(self.is_streaming),
                area.y,
                area.width.saturating_sub(1),
                1,
            ),
            buf,
        );
    }
}
