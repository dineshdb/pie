use tuirealm::ratatui::buffer::Buffer;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::style::{Color, Style};
use tuirealm::ratatui::text::{Line, Span};
use tuirealm::ratatui::widgets::{Paragraph, Widget};

pub struct TaskView<'a> {
    pub task_list: &'a crate::tools::tasks::TaskList,
    pub block: Option<tuirealm::ratatui::widgets::Block<'a>>,
}

impl<'a> TaskView<'a> {
    pub fn new(task_list: &'a crate::tools::tasks::TaskList) -> TaskView<'a> {
        TaskView {
            task_list,
            block: None,
        }
    }

    pub fn block(mut self, block: tuirealm::ratatui::widgets::Block<'a>) -> TaskView<'a> {
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
        append_task_list_lines(&mut lines, self.task_list, task_area.width as usize);
        Paragraph::new(lines).render(task_area, buf);
    }
}

fn append_task_list_lines(
    lines: &mut Vec<Line<'static>>,
    task_list: &crate::tools::tasks::TaskList,
    width: usize,
) {
    use crate::tools::tasks::TaskStatus;
    let prefix = Span::styled("  ", Style::default().fg(Color::DarkGray));

    for task in &task_list.tasks {
        let (icon, color) = match task.status {
            TaskStatus::Pending => ("○", Color::DarkGray),
            TaskStatus::InProgress => ("▶", Color::Yellow),
            TaskStatus::Completed => ("✓", Color::Green),
            TaskStatus::Failed => ("✖", Color::Red),
            TaskStatus::Skipped => ("↷", Color::DarkGray),
        };

        let task_text = super::truncate_str(&task.title, width.saturating_sub(4));
        lines.push(Line::from(vec![
            prefix.clone(),
            Span::styled(format!("{icon} "), Style::default().fg(color)),
            Span::styled(task_text, Style::default().fg(color)),
        ]));
    }
}
