//! The `/model` picker overlay: the startup catalog as a flat list —
//! the default entry plus one per configured tier. The `*` marks the
//! selection the next turn carries; Enter confirms, Esc closes.

use crate::door::CatalogEntry;
use tuirealm::ratatui::buffer::Buffer;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::style::{Color, Modifier, Style};
use tuirealm::ratatui::widgets::{
    Block, Borders, List, ListItem, ListState, StatefulWidget, Widget,
};

pub struct ModelSelectorOverlay<'a> {
    pub entries: &'a [CatalogEntry],
    /// The id the next turn carries (pending or confirmed), if any.
    pub current_id: Option<&'a str>,
    pub selected_idx: usize,
}

impl Widget for ModelSelectorOverlay<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let items: Vec<ListItem> = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                let is_navigating = i == self.selected_idx;
                let is_current = self.current_id == Some(entry.id.as_str());

                let prefix = if is_current { " * " } else { "   " };
                let text = format!("{prefix}{} — {}", entry.id, entry.model);

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

        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(" Model "))
            .highlight_style(
                Style::default()
                    .bg(Color::Cyan)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            );

        let mut state = ListState::default();
        state.select(Some(self.selected_idx));
        StatefulWidget::render(list, area, buf, &mut state);
    }
}
