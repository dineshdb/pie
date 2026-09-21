use tuirealm::ratatui::layout::{Constraint, Direction, Layout, Rect};
use tuirealm::ratatui::style::{Color, Style};
use tuirealm::ratatui::widgets::{Block, Borders, Clear, Widget};

pub struct Dialog<'a, W: Widget> {
    pub title: &'a str,
    pub inner: W,
    pub width_percent: u16,
    pub height_percent: u16,
}

impl<'a, W: Widget> Dialog<'a, W> {
    pub fn new(title: &'a str, inner: W) -> Self {
        Self {
            title,
            inner,
            width_percent: 60,
            height_percent: 40,
        }
    }

    pub fn with_size(mut self, width: u16, height: u16) -> Self {
        self.width_percent = width;
        self.height_percent = height;
        self
    }
}

impl<W: Widget> Widget for Dialog<'_, W> {
    fn render(self, area: Rect, buf: &mut tuirealm::ratatui::buffer::Buffer) {
        let popup_area = centered_rect(self.width_percent, self.height_percent, area);
        Clear.render(popup_area, buf);

        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", self.title))
            .border_style(Style::default().fg(Color::Cyan));

        self.inner.render(block.inner(popup_area), buf);
        block.render(popup_area, buf);
    }
}

pub fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints(
            [
                Constraint::Percentage((100 - percent_y) / 2),
                Constraint::Percentage(percent_y),
                Constraint::Percentage((100 - percent_y) / 2),
            ]
            .as_ref(),
        )
        .split(r);

    let popup_row = popup_layout.get(1).copied().unwrap_or(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints(
            [
                Constraint::Percentage((100 - percent_x) / 2),
                Constraint::Percentage(percent_x),
                Constraint::Percentage((100 - percent_x) / 2),
            ]
            .as_ref(),
        )
        .split(popup_row)
        .get(1)
        .copied()
        .unwrap_or(popup_row)
}
