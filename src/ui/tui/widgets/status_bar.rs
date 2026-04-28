use crate::ui::tui::widgets::spinner::Spinner;
use tuirealm::ratatui::buffer::Buffer;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::style::{Color, Modifier, Style};
use tuirealm::ratatui::text::Span;
use tuirealm::ratatui::widgets::Widget;

pub struct StatusBar {
    pub active_tasks: Vec<String>,
    pub is_streaming: bool,
    pub spinner_frame: usize,
}

impl StatusBar {
    pub fn new(active_tasks: Vec<String>, is_streaming: bool, spinner_frame: usize) -> Self {
        Self {
            active_tasks,
            is_streaming,
            spinner_frame,
        }
    }
}

impl Widget for StatusBar {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let style = if self.is_streaming {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else if !self.active_tasks.is_empty() {
            Style::default().fg(Color::Green)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        // 1. Render Spinner at the far left if streaming
        if self.is_streaming {
            Spinner::new(self.spinner_frame)
                .style(style)
                .render(Rect::new(area.x, area.y, 1, 1), buf);
        }

        // 2. Render Task Title(s)
        let title = if self.active_tasks.is_empty() {
            "PIE".to_string()
        } else {
            self.active_tasks.join(" › ")
        };
        let title_span = Span::styled(format!(" {title} "), style);

        // Offset title if spinner is shown
        let title_x = if self.is_streaming {
            area.x + 2
        } else {
            area.x
        };
        buf.set_span(
            title_x,
            area.y,
            &title_span,
            area.width.saturating_sub(title_x - area.x),
        );
    }
}
