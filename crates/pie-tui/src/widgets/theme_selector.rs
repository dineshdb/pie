//! The `/theme` picker overlay: one row per palette, each name and its
//! sample rendered in that theme's own colors — the preview is the
//! point. The `*` marks the active theme; Enter confirms, Esc closes.

use crate::theme::{THEMES, Theme};
use tuirealm::ratatui::buffer::Buffer;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::style::{Modifier, Style};
use tuirealm::ratatui::widgets::{
    Block, Borders, List, ListItem, ListState, StatefulWidget, Widget,
};

pub struct ThemeSelectorOverlay {
    /// The theme `/theme` would show as active.
    pub current_name: &'static str,
    pub selected_idx: usize,
    /// The theme whose colors draw the chrome (highlight row) — the
    /// UI the user is currently looking at, not the row being previewed.
    pub theme: &'static Theme,
}

impl Widget for ThemeSelectorOverlay {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let items: Vec<ListItem> = THEMES
            .iter()
            .enumerate()
            .map(|(i, candidate)| {
                let is_current = candidate.name == self.current_name;
                let prefix = if is_current { " * " } else { "   " };
                let text = format!("{prefix}{}", candidate.name);

                let style = if i == self.selected_idx {
                    Style::default()
                        .bg(self.theme.selection_bg)
                        .fg(self.theme.selection_fg)
                        .add_modifier(Modifier::BOLD)
                } else {
                    // Preview the row in its own palette's text color.
                    Style::default().fg(candidate.text)
                };
                ListItem::new(text).style(style)
            })
            .collect();

        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(" Theme "))
            .highlight_style(
                Style::default()
                    .bg(self.theme.selection_bg)
                    .fg(self.theme.selection_fg)
                    .add_modifier(Modifier::BOLD),
            );

        let mut state = ListState::default();
        state.select(Some(self.selected_idx));
        StatefulWidget::render(list, area, buf, &mut state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::LIGHT;
    use tuirealm::ratatui::Terminal;
    use tuirealm::ratatui::backend::TestBackend;

    fn render_overlay(overlay: ThemeSelectorOverlay) -> Buffer {
        let backend = TestBackend::new(30, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| f.render_widget(overlay, f.area()))
            .unwrap();
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
    fn every_theme_gets_a_row_and_current_gets_a_star() {
        let buf = render_overlay(ThemeSelectorOverlay {
            current_name: LIGHT.name,
            selected_idx: 0,
            theme: &LIGHT,
        });

        let content: Vec<String> = (0..buf.area.height).map(|r| row(&buf, r)).collect();
        for theme in THEMES {
            assert!(
                content.iter().any(|r| r.contains(theme.name)),
                "theme '{}' should have a row: {content:?}",
                theme.name
            );
        }
        let star_row = content
            .iter()
            .find(|r| r.contains('*'))
            .expect("the active theme is marked with *");
        assert!(
            star_row.contains(LIGHT.name),
            "* must mark the current theme"
        );
    }
}
