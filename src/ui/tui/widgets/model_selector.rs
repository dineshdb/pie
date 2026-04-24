use tuirealm::ratatui::layout::{Constraint, Direction, Layout, Rect};
use tuirealm::ratatui::style::{Color, Modifier, Style};
use tuirealm::ratatui::text::Line;
use tuirealm::ratatui::widgets::{Block, Borders, List, ListItem, ListState, Tabs, Widget};

pub struct ModelSelectorOverlay<'a> {
    pub providers: &'a [String],
    pub provider_idx: usize,
    pub models: &'a [String],
    pub selected_idx: Option<usize>,
    pub current_model_idx: Option<usize>,
    pub is_loading: bool,
    pub error: Option<&'a str>,
}

impl Widget for ModelSelectorOverlay<'_> {
    fn render(self, area: Rect, buf: &mut tuirealm::ratatui::buffer::Buffer) {
        tracing::info!(
            models = self.models.len(),
            loading = self.is_loading,
            error = ?self.error,
            area = ?area,
            "rendering ModelSelectorOverlay"
        );
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(0)])
            .split(area);

        // Render providers using Tabs widget
        let titles: Vec<Line> = self
            .providers
            .iter()
            .map(|p| Line::from(p.as_str()))
            .collect();

        let tabs = Tabs::new(titles)
            .block(Block::default().borders(Borders::ALL))
            .select(self.provider_idx)
            .style(Style::default().fg(Color::Cyan))
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            );

        if let Some(p_area) = chunks.first() {
            tabs.render(*p_area, buf);
        }

        if let Some(err) = self.error {
            if let Some(m_area) = chunks.get(1) {
                let error_text = format!("Error: {err}");
                let text_len = u16::try_from(error_text.len()).unwrap_or(u16::MAX);
                let x = m_area.x + (m_area.width.saturating_sub(text_len) / 2);
                let y = m_area.y + (m_area.height / 2);
                buf.set_string(x, y, error_text, Style::default().fg(Color::Red));
            }
            return;
        }

        if self.is_loading {
            if let Some(m_area) = chunks.get(1) {
                let loading_text = "Loading models...";
                let text_len = u16::try_from(loading_text.len()).unwrap_or(u16::MAX);
                let x = m_area.x + (m_area.width.saturating_sub(text_len) / 2);
                let y = m_area.y + (m_area.height / 2);
                buf.set_string(x, y, loading_text, Style::default().fg(Color::Gray));
            }
            return;
        }

        if self.models.is_empty() {
            if let Some(m_area) = chunks.get(1) {
                let no_models_text = "No models found.";
                let text_len = u16::try_from(no_models_text.len()).unwrap_or(u16::MAX);
                let x = m_area.x + (m_area.width.saturating_sub(text_len) / 2);
                let y = m_area.y + (m_area.height / 2);
                buf.set_string(x, y, no_models_text, Style::default().fg(Color::Gray));
            }
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

        let list = List::new(items).block(Block::default().borders(Borders::ALL));

        let mut state = ListState::default();
        state.select(self.selected_idx);

        if let Some(m_area) = chunks.get(1) {
            tuirealm::ratatui::widgets::StatefulWidget::render(list, *m_area, buf, &mut state);
        }
    }
}
