use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::style::{Color, Modifier, Style};
use tuirealm::ratatui::widgets::{List, ListItem, ListState, Widget};

pub struct ModelSelectorOverlay<'a> {
    pub models: &'a [String],
    pub selected_idx: Option<usize>,
    pub current_model_idx: Option<usize>,
    pub is_loading: bool,
}

impl Widget for ModelSelectorOverlay<'_> {
    fn render(self, area: Rect, buf: &mut tuirealm::ratatui::buffer::Buffer) {
        if self.is_loading {
            let loading_text = "Loading models...";
            let text_len = u16::try_from(loading_text.len()).unwrap_or(u16::MAX);
            let x = area.x + (area.width.saturating_sub(text_len) / 2);
            let y = area.y + (area.height / 2);
            buf.set_string(x, y, loading_text, Style::default().fg(Color::Gray));
            return;
        }

        let items: Vec<ListItem> = self
            .models
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let is_navigating = Some(i) == self.selected_idx;
                let is_current = Some(i) == self.current_model_idx;

                let prefix = if is_current { " * " } else { "   " };
                let text = format!("{prefix}{m}");

                let mut style = Style::default().fg(Color::White);
                if is_navigating {
                    style = style
                        .bg(Color::Cyan)
                        .fg(Color::Black)
                        .add_modifier(Modifier::BOLD);
                } else if is_current {
                    style = style.fg(Color::Cyan).add_modifier(Modifier::BOLD);
                }

                ListItem::new(text).style(style)
            })
            .collect();

        let list = List::new(items);

        let mut state = ListState::default();
        state.select(self.selected_idx);

        tuirealm::ratatui::widgets::StatefulWidget::render(list, area, buf, &mut state);
    }
}
