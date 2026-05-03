use crate::db::DbPool;
use crate::tools::plan::PlanRepo;
use crate::tools::plan::StepStatus;
use std::sync::Arc;
use tuirealm::ratatui::buffer::Buffer;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::style::{Color, Style};
use tuirealm::ratatui::text::{Line, Span};
use tuirealm::ratatui::widgets;
use tuirealm::ratatui::widgets::{Paragraph, Widget};

pub struct PlanView<'a> {
    pub db: Arc<DbPool>,
    pub session: &'a str,
    pub block: Option<widgets::Block<'a>>,
}

impl<'a> PlanView<'a> {
    pub fn new(db: Arc<DbPool>, session: &'a str) -> PlanView<'a> {
        PlanView {
            db,
            block: None,
            session,
        }
    }

    pub fn block(mut self, block: widgets::Block<'a>) -> PlanView<'a> {
        self.block = Some(block);
        self
    }
}

impl Widget for PlanView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let plan_area = match self.block {
            Some(ref b) => {
                let inner = b.inner(area);
                b.clone().render(area, buf);
                inner
            }
            None => area,
        };

        let mut lines = Vec::new();
        let prefix = Span::styled("  ", Style::default().fg(Color::DarkGray));

        for step in &self.db.load_steps(self.session).unwrap_or_default() {
            let (icon, color) = match step.status {
                StepStatus::Pending => ("○", Color::DarkGray),
                StepStatus::InProgress => ("▶", Color::Yellow),
                StepStatus::Completed => ("✓", Color::Green),
                StepStatus::Failed => ("✖", Color::Red),
                StepStatus::Skipped => ("↷", Color::DarkGray),
            };

            let step_text =
                super::truncate_str(&step.name, (plan_area.width as usize).saturating_sub(4));
            lines.push(Line::from(vec![
                prefix.clone(),
                Span::styled(format!("{icon} "), Style::default().fg(color)),
                Span::styled(step_text, Style::default().fg(color)),
            ]));
        }

        Paragraph::new(lines).render(plan_area, buf);
    }
}
