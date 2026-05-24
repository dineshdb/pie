use crate::session::Role;
use tuirealm::ratatui::style::{Color, Style};
use tuirealm::ratatui::text::Line;

struct RenderedCache {
    width: usize,
    content: String,
    is_latest: bool,
    lines: Vec<Line<'static>>,
    last_render: std::time::Instant,
}

pub struct MessageRenderCache {
    entries: Vec<Option<RenderedCache>>,
    throttle_ms: u64,
}

impl MessageRenderCache {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            throttle_ms: 50, // Throttle re-renders to 20fps during streaming
        }
    }

    pub fn push(&mut self) {
        self.entries.push(None);
    }

    pub fn insert(&mut self, index: usize) {
        if index < self.entries.len() {
            self.entries.insert(index, None);
        } else {
            self.entries.push(None);
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn get_or_render(
        &mut self,
        role: Role,
        content: &str,
        is_latest: bool,
        index: usize,
        width: usize,
    ) -> &[Line<'static>] {
        let now = std::time::Instant::now();

        let existing = self.entries.get(index).and_then(|e| e.as_ref());

        let needs_rerender = match existing {
            None => true,
            Some(c) => {
                if c.width != width || c.is_latest != is_latest {
                    true
                } else if c.content != content {
                    // If content changed, throttle if it's the latest message
                    if is_latest {
                        c.last_render.elapsed().as_millis() >= u128::from(self.throttle_ms)
                    } else {
                        true
                    }
                } else {
                    false
                }
            }
        };

        if needs_rerender {
            let color = role_color(role);
            let prefix = message_prefix(role, is_latest);
            let cont_prefix = continuation_prefix();

            let raw_lines = if role == Role::Tool {
                render_tool_lines(content, width)
            } else if role == Role::System && content.starts_with("Welcome to") {
                render_welcome_lines(content)
            } else if role == Role::Assistant && !content.is_empty() {
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

            let lines = raw_lines
                .into_iter()
                .enumerate()
                .map(|(i, mut line)| {
                    let pfx = if i == 0 {
                        prefix.clone()
                    } else {
                        cont_prefix.clone()
                    };
                    line.spans.insert(0, pfx);
                    line
                })
                .collect();

            // Grow entries if needed
            while self.entries.len() <= index {
                self.entries.push(None);
            }
            if let Some(entry) = self.entries.get_mut(index) {
                *entry = Some(RenderedCache {
                    width,
                    content: content.to_string(),
                    is_latest,
                    lines,
                    last_render: now,
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

    pub fn invalidate(&mut self, index: usize) {
        if let Some(entry) = self.entries.get_mut(index) {
            *entry = None;
        }
    }

    pub fn get_lines(&self, index: usize) -> Option<&[Line<'static>]> {
        self.entries
            .get(index)
            .and_then(|e| e.as_ref())
            .map(|c| c.lines.as_slice())
    }
}

fn role_color(role: Role) -> Color {
    match role {
        Role::User => Color::White,
        Role::Assistant => Color::Gray,
        Role::System => Color::Yellow,
        Role::Tool => Color::DarkGray,
    }
}

fn message_prefix(role: Role, is_latest: bool) -> tuirealm::ratatui::text::Span<'static> {
    use tuirealm::ratatui::style::Modifier;
    use tuirealm::ratatui::text::Span;
    match role {
        Role::User if is_latest => Span::styled(
            "> ",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Role::User => Span::styled("> ", Style::default().fg(Color::DarkGray)),
        _ => Span::styled("  ", Style::default().fg(Color::DarkGray)),
    }
}

fn continuation_prefix() -> tuirealm::ratatui::text::Span<'static> {
    tuirealm::ratatui::text::Span::styled("  ", Style::default().fg(Color::DarkGray))
}

fn render_tool_lines(content: &str, width: usize) -> Vec<Line<'static>> {
    use tuirealm::ratatui::style::Modifier;
    use tuirealm::ratatui::text::Span;

    let mut lines = Vec::new();
    let (call, output) = content.split_once(" → ").unwrap_or((content, ""));

    let call_text = super::truncate_str(call, width);
    lines.push(Line::from(vec![Span::styled(
        call_text,
        Style::default().fg(Color::Magenta),
    )]));

    if !output.is_empty() {
        let output_text = super::truncate_str(output, width.saturating_sub(4));
        let dim = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM);
        lines.push(Line::from(vec![
            Span::styled("└ ", dim),
            Span::styled(output_text, dim),
        ]));
    }
    lines
}

fn render_welcome_lines(content: &str) -> Vec<Line<'static>> {
    use tuirealm::ratatui::text::Span;

    let yellow = Style::default().fg(Color::Yellow);
    let cyan = Style::default().fg(Color::Cyan);
    let green = Style::default().fg(Color::Green);

    let mut spans = Vec::new();
    let mut rest = content;
    while let Some(pos) = rest.find("pie") {
        if pos > 0 {
            spans.push(Span::styled(rest[..pos].to_string(), yellow));
        }
        spans.push(Span::styled("pie", cyan));
        rest = &rest[pos + 3..];
    }
    if let Some(pos) = rest.find('?') {
        if pos > 0 {
            spans.push(Span::styled(rest[..pos].to_string(), yellow));
        }
        spans.push(Span::styled("?", green));
        rest = &rest[pos + 1..];
    }
    if !rest.is_empty() {
        spans.push(Span::styled(rest.to_string(), yellow));
    }
    vec![Line::from(spans)]
}
