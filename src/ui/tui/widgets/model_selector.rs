use tuirealm::ratatui::layout::{Constraint, Direction, Layout, Rect};
use tuirealm::ratatui::style::{Color, Modifier, Style};
use tuirealm::ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Widget};

pub struct ModelSelectorOverlay<'a> {
    pub providers: &'a [String],
    pub provider_idx: usize,
    pub models: &'a [String],
    pub selected_idx: Option<usize>,
    pub current_model_idx: Option<usize>,
    pub is_loading: bool,
}

impl Widget for ModelSelectorOverlay<'_> {
    fn render(self, area: Rect, buf: &mut tuirealm::ratatui::buffer::Buffer) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(0)])
            .split(area);

        // Render providers
        let providers_text: String = self
            .providers
            .iter()
            .enumerate()
            .map(|(i, p)| {
                if i == self.provider_idx {
                    format!(" [{p}] ")
                } else {
                    format!("  {p}  ")
                }
            })
            .collect();

        let providers_para = Paragraph::new(providers_text)
            .block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .title(" Providers "),
            )
            .style(Style::default().fg(Color::Cyan));

        if let Some(p_area) = chunks.first() {
            providers_para.render(*p_area, buf);
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

        let list = List::new(items).block(Block::default().title(" Models "));

        let mut state = ListState::default();
        state.select(self.selected_idx);

        if let Some(m_area) = chunks.get(1) {
            tuirealm::ratatui::widgets::StatefulWidget::render(list, *m_area, buf, &mut state);
        }
    }
}
