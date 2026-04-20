use crate::session::Role;
use ratatui::style::{Color, Style};
use ratatui::text::Line;

struct RenderedCache {
    width: usize,
    content: String,
    lines: Vec<Line<'static>>,
}

pub struct MessageRenderCache {
    entries: Vec<Option<RenderedCache>>,
}

impl MessageRenderCache {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub fn push(&mut self) {
        self.entries.push(None);
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn get_or_render(
        &mut self,
        role: Role,
        content: &str,
        _is_streaming: bool,
        index: usize,
        width: usize,
    ) -> &[Line<'static>] {
        let needs_rerender = self
            .entries
            .get(index)
            .and_then(|e| e.as_ref())
            .is_none_or(|c| c.width != width || c.content != content);

        if needs_rerender {
            let color = role_color(role);
            let lines = if matches!(role, Role::Assistant) && !content.is_empty() {
                super::markdown::render_markdown(content, width, color)
            } else {
                content
                    .lines()
                    .flat_map(|line| {
                        if line.is_empty() {
                            vec![Line::raw("")]
                        } else {
                            super::wrap::wrap_line(line, width)
                                .into_iter()
                                .map(|l| Line::styled(l, Style::default().fg(color)))
                                .collect()
                        }
                    })
                    .collect()
            };

            // Grow entries if needed
            while self.entries.len() <= index {
                self.entries.push(None);
            }
            if let Some(entry) = self.entries.get_mut(index) {
                *entry = Some(RenderedCache {
                    width,
                    content: content.to_string(),
                    lines,
                });
            }
        }

        // SAFETY: we just ensured the entry exists and is Some above.
        self.entries
            .get(index)
            .and_then(|e| e.as_ref())
            .map_or_else(|| unreachable!("entry guaranteed to exist"), |c| &c.lines)
    }

    pub fn trim_front(&mut self, count: usize) {
        self.entries.drain(0..count);
    }

    /// Remove the entry at `index`, shift subsequent entries down by one,
    /// and append `None` at the end (for the moved message).
    pub fn shift_remove(&mut self, index: usize) {
        if index < self.entries.len() {
            self.entries.remove(index);
            self.entries.push(None);
        }
    }
}

fn role_color(role: Role) -> Color {
    match role {
        Role::User => Color::White,
        Role::Assistant => Color::Gray,
        Role::System => Color::Yellow,
        Role::Tool => Color::DarkGray, // fallback — tool messages bypass the cache
    }
}
