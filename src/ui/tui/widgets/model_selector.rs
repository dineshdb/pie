use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::style::{Color, Modifier, Style};
use tuirealm::ratatui::text::Line;
use tuirealm::ratatui::widgets::{Block, Borders, Tabs, Widget};

pub struct ModelSelectorOverlay<'a> {
    pub providers: &'a [String],
    pub provider_idx: usize,
    pub is_loading: bool,
    pub error: Option<&'a str>,
}

impl Widget for ModelSelectorOverlay<'_> {
    fn render(self, area: Rect, buf: &mut tuirealm::ratatui::buffer::Buffer) {
        tracing::debug!(
            loading = self.is_loading,
            error = ?self.error,
            "rendering ModelSelectorOverlay"
        );

        // Render providers using Tabs widget
        let titles: Vec<Line> = self
            .providers
            .iter()
            .map(|p| Line::from(p.as_str()))
            .collect();

        let tabs = Tabs::new(titles)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Providers (Left/Right) "),
            )
            .select(self.provider_idx)
            .style(Style::default().fg(Color::Cyan))
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            );

        tabs.render(area, buf);

        if let Some(err) = self.error {
            // Render error as a small popup or overlay if needed, but for now just skip models.
            tracing::error!(error = %err, "not rendering models due to error");
        }
    }
}
