use crate::theme::Theme;
use crate::widgets::spinner::Spinner;
use tuirealm::ratatui::buffer::Buffer;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::style::{Modifier, Style};
use tuirealm::ratatui::text::Span;
use tuirealm::ratatui::widgets::Widget;

pub struct StatusBar {
    pub active_steps: Vec<String>,
    pub is_streaming: bool,
    pub spinner_frame: usize,
    /// The conversation's session usage summary (tokens + spend) —
    /// right-aligned, dim; `None` before the first billed turn.
    pub usage: Option<String>,
    pub theme: &'static Theme,
}

impl StatusBar {
    pub fn new(
        active_steps: Vec<String>,
        is_streaming: bool,
        spinner_frame: usize,
        usage: Option<String>,
        theme: &'static Theme,
    ) -> Self {
        Self {
            active_steps,
            is_streaming,
            spinner_frame,
            usage,
            theme,
        }
    }
}

impl Widget for StatusBar {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let style = if self.is_streaming {
            Style::default()
                .fg(self.theme.accent)
                .add_modifier(Modifier::BOLD)
        } else if !self.active_steps.is_empty() {
            Style::default().fg(self.theme.success)
        } else {
            Style::default().fg(self.theme.text_dim)
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

        // 3. Session usage, right-aligned over the title — it wins the
        // overlap, the title is filler.
        if let Some(usage) = self.usage
            && area.width > 0
        {
            let usage = format!(" {usage} ");
            let width =
                u16::try_from(usage.chars().count().min(area.width as usize)).unwrap_or(area.width);
            let x = area.x + area.width - width;
            Span::styled(usage, Style::default().fg(self.theme.text_dim))
                .render(Rect::new(x, area.y, width, 1), buf);
        }
    }
}
