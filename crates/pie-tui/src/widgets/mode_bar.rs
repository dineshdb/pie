//! The mode bar: the live selection state — the mode and model the next
//! turn runs under (pending selection, the conversation's confirmed
//! selection, or the startup defaults) — plus the yolo flag while it is
//! on: the standing disclosure that asks are being auto-approved.

use crate::theme::Theme;
use tuirealm::ratatui::buffer::Buffer;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::style::{Modifier, Style};
use tuirealm::ratatui::text::Span;
use tuirealm::ratatui::widgets::Widget;

pub struct ModeBar {
    pub mode: String,
    pub model: String,
    pub yolo: bool,
    pub theme: &'static Theme,
}

impl ModeBar {
    pub fn new(mode: String, model: String, yolo: bool, theme: &'static Theme) -> Self {
        Self {
            mode,
            model,
            yolo,
            theme,
        }
    }
}

impl Widget for ModeBar {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width < 4 {
            return;
        }

        let mode_style = Style::default()
            .fg(self.theme.accent)
            .add_modifier(Modifier::BOLD);
        let dim_style = Style::default().fg(self.theme.text_dim);

        let mode_label = format!(" {} ", self.mode);
        #[allow(clippy::cast_possible_truncation)]
        let mode_width = mode_label.chars().count() as u16;
        Span::styled(mode_label, mode_style).render(Rect::new(area.x, area.y, mode_width, 1), buf);

        let mut rest_x = area.x + mode_width;
        if self.yolo {
            let yolo_style = Style::default()
                .fg(self.theme.warning)
                .add_modifier(Modifier::BOLD);
            Span::styled(" yolo ", yolo_style).render(Rect::new(rest_x, area.y, 6, 1), buf);
            rest_x += 6;
        }

        let mdl_tag = Span::styled(format!(" {}", self.model), dim_style);
        mdl_tag.render(
            Rect::new(
                rest_x,
                area.y,
                area.width.saturating_sub(rest_x - area.x),
                1,
            ),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::DARK;
    use tuirealm::ratatui::Terminal;
    use tuirealm::ratatui::backend::TestBackend;

    fn render_bar(bar: ModeBar) -> String {
        let backend = TestBackend::new(40, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| f.render_widget(bar, f.area())).unwrap();
        (0..40)
            .map(|col| terminal.backend().buffer()[(col, 0)].symbol().to_string())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn yolo_tag_shows_only_while_yolo_is_on() {
        let on = render_bar(ModeBar {
            mode: "build".into(),
            model: "m1".into(),
            yolo: true,
            theme: &DARK,
        });
        assert!(
            on.contains(" yolo ") && on.starts_with(" build "),
            "yolo sits beside the mode: {on}"
        );

        let off = render_bar(ModeBar {
            mode: "build".into(),
            model: "m1".into(),
            yolo: false,
            theme: &DARK,
        });
        assert!(!off.contains("yolo"), "{off}");
    }
}
