//! The mode bar: the live selection state — the mode and model the next
//! turn runs under (pending selection, the conversation's confirmed
//! selection, or the startup defaults).

use tuirealm::ratatui::buffer::Buffer;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::style::{Color, Modifier, Style};
use tuirealm::ratatui::text::Span;
use tuirealm::ratatui::widgets::Widget;

pub struct ModeBar {
    pub mode: String,
    pub model: String,
}

impl ModeBar {
    pub fn new(mode: String, model: String) -> Self {
        Self { mode, model }
    }
}

impl Widget for ModeBar {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width < 4 {
            return;
        }

        let mode_style = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let dim_style = Style::default().fg(Color::DarkGray);
        let mode_tag = Span::styled(format!(" {} ", self.mode), mode_style);
        let mdl_tag = Span::styled(format!(" {}", self.model), dim_style);

        mode_tag.render(Rect::new(area.x, area.y, 6, 1), buf);
        mdl_tag.render(
            Rect::new(area.x + 6, area.y, area.width.saturating_sub(6), 1),
            buf,
        );

        // Dim background
        for col in area.x..area.x + area.width {
            if let Some(cell) = buf.cell_mut((col, area.y))
                && cell.symbol() == " "
            {
                cell.set_style(dim_style);
            }
        }
    }
}
