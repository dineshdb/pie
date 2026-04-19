use crate::session::Role;
use crate::ui::tui::state::ChatMessage;
use crate::ui::tui::widgets::render_cache::MessageRenderCache;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, StatefulWidget, Widget};

const PREFIX_WIDTH: usize = 2;
const RIGHT_PAD: usize = 1;

pub struct ChatState {
    pub scroll_offset: u16,
    pub auto_scroll: bool,
}

impl ChatState {
    pub fn new() -> Self {
        Self {
            scroll_offset: 0,
            auto_scroll: true,
        }
    }

    pub fn scroll_up(&mut self, amount: u16) {
        self.auto_scroll = false;
        self.scroll_offset = self.scroll_offset.saturating_sub(amount);
    }

    pub fn scroll_down(&mut self, amount: u16) {
        self.auto_scroll = false;
        self.scroll_offset = self.scroll_offset.saturating_add(amount);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.auto_scroll = true;
    }
}

pub struct ChatView<'a> {
    pub lines: &'a [Line<'static>],
}

impl StatefulWidget for ChatView<'_> {
    type State = ChatState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let inner_height = area.height;
        #[allow(clippy::cast_possible_truncation)]
        let total_lines = self.lines.len() as u16;
        let max_scroll = total_lines.saturating_sub(inner_height);

        if state.auto_scroll {
            state.scroll_offset = max_scroll;
        } else {
            state.scroll_offset = state.scroll_offset.min(max_scroll);
        }

        let visible_start = state.scroll_offset as usize;
        let visible_lines: Vec<Line> = self
            .lines
            .iter()
            .skip(visible_start)
            .take(inner_height as usize)
            .cloned()
            .collect();

        Paragraph::new(visible_lines).render(area, buf);
    }
}

/// Build rendered chat lines from messages, using the render cache for efficiency.
pub fn build_chat_lines(
    messages: &[ChatMessage],
    cache: &mut MessageRenderCache,
    area_width: usize,
) -> Vec<Line<'static>> {
    let width = area_width.saturating_sub(PREFIX_WIDTH + RIGHT_PAD);
    let last_idx = messages.len().saturating_sub(1);
    let mut lines: Vec<Line<'static>> = Vec::new();

    for (msg_idx, msg) in messages.iter().enumerate() {
        let is_latest = msg_idx == last_idx;
        let prefix = message_prefix(msg.role, is_latest);
        let rendered =
            cache.get_or_render(msg.role, &msg.content, msg.is_streaming, msg_idx, width);

        for (i, line) in rendered.iter().enumerate() {
            let pfx = if i == 0 {
                prefix.clone()
            } else {
                continuation_prefix()
            };
            let mut spans = vec![pfx];
            spans.extend(line.spans.iter().cloned());
            lines.push(Line::from(spans));
        }

        // Blank separator: only before user messages (keeps space between response → next question,
        // removes space between question → response).
        if let Some(next) = messages.get(msg_idx + 1)
            && next.role == Role::User
        {
            lines.push(Line::raw(""));
        }
    }

    lines
}

fn message_prefix(role: Role, is_latest: bool) -> Span<'static> {
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

fn continuation_prefix() -> Span<'static> {
    Span::styled("  ", Style::default().fg(Color::DarkGray))
}
