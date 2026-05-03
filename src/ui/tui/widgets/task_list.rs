use crate::db::DbPool;
use crate::tools::tasks::TaskRepo;
use crate::tools::tasks::TaskStatus;
use std::sync::Arc;
use tuirealm::ratatui::buffer::Buffer;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::style::{Color, Style};
use tuirealm::ratatui::text::{Line, Span};
use tuirealm::ratatui::widgets;
use tuirealm::ratatui::widgets::{Paragraph, Widget};

pub struct TaskView<'a> {
    pub db: Arc<DbPool>,
    pub session: &'a str,
    pub block: Option<widgets::Block<'a>>,
}

impl<'a> TaskView<'a> {
    pub fn new(db: Arc<DbPool>, session: &'a str) -> TaskView<'a> {
        TaskView {
            db,
            block: None,
            session,
        }
    }

    pub fn block(mut self, block: widgets::Block<'a>) -> TaskView<'a> {
        self.block = Some(block);
        self
    }
}

impl Widget for TaskView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let task_area = match self.block {
            Some(ref b) => {
                let inner = b.inner(area);
                b.clone().render(area, buf);
                inner
            }
            None => area,
        };

        let mut lines = Vec::new();
        let prefix = Span::styled("  ", Style::default().fg(Color::DarkGray));

        for task in &self.db.load_tasks(self.session).unwrap_or_default() {
            let (icon, color) = match task.status {
                TaskStatus::Pending => ("○", Color::DarkGray),
                TaskStatus::InProgress => ("▶", Color::Yellow),
                TaskStatus::Completed => ("✓", Color::Green),
                TaskStatus::Failed => ("✖", Color::Red),
                TaskStatus::Skipped => ("↷", Color::DarkGray),
            };

            let task_text =
                super::truncate_str(&task.name, (task_area.width as usize).saturating_sub(4));
            lines.push(Line::from(vec![
                prefix.clone(),
                Span::styled(format!("{icon} "), Style::default().fg(color)),
                Span::styled(task_text, Style::default().fg(color)),
            ]));
        }

        Paragraph::new(lines).render(task_area, buf);
    }
}
