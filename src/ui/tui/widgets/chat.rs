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

/// Build rendered chat lines from messages.
/// Response messages are rendered last so tool calls appear above the response.
pub fn build_chat_lines(
    messages: &[ChatMessage],
    cache: &mut MessageRenderCache,
    area_width: usize,
) -> Vec<Line<'static>> {
    let width = area_width.saturating_sub(PREFIX_WIDTH + RIGHT_PAD);

    // Build render order: regular messages in order, then response message last
    let mut order: Vec<usize> = Vec::with_capacity(messages.len());
    let mut response_idx: Option<usize> = None;

    for (i, msg) in messages.iter().enumerate() {
        if msg.is_response() {
            response_idx = Some(i);
        } else {
            order.push(i);
        }
    }
    if let Some(ri) = response_idx {
        order.push(ri);
    }

    let last_rendered = order.len().saturating_sub(1);
    let mut lines: Vec<Line<'static>> = Vec::new();

    for (render_pos, &msg_idx) in order.iter().enumerate() {
        let Some(msg) = messages.get(msg_idx) else {
            continue;
        };

        if msg.role == Role::Tool {
            append_tool_lines(&mut lines, &msg.content, width);
        } else {
            // Add a gap before the assistant response (streaming or finalized)
            if msg.role == Role::Assistant && !lines.is_empty() {
                lines.push(Line::raw(""));
            }
            let is_latest = render_pos == last_rendered;
            let prefix = message_prefix(msg.role, is_latest);
            let rendered =
                cache.get_or_render(msg.role, &msg.content, msg.is_response(), msg_idx, width);

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
        }

        // Blank separator: only before user messages
        if let Some(&next_idx) = order.get(render_pos + 1)
            && let Some(next) = messages.get(next_idx)
            && next.role == Role::User
        {
            lines.push(Line::raw(""));
        }
    }

    lines
}

/// Render a tool call as exactly two lines using color to distinguish them:
/// ```text
///   name(params)            ← magenta
///     output text...        ← dark gray
/// ```
fn append_tool_lines(lines: &mut Vec<Line<'static>>, content: &str, width: usize) {
    let (call, output) = content.split_once(" → ").unwrap_or((content, ""));

    let call_text = truncate_str(call, width);
    lines.push(Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(call_text, Style::default().fg(Color::Magenta)),
    ]));

    if !output.is_empty() {
        let output_text = truncate_str(output, width.saturating_sub(4));
        lines.push(Line::from(vec![
            Span::styled("  └ ", Style::default().fg(Color::DarkGray)),
            Span::styled(output_text, Style::default().fg(Color::DarkGray)),
        ]));
    }
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

fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }
    let end = s.ceil_char_boundary(max_len);
    format!("{}…", &s[..end])
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render_chat(messages: &[ChatMessage], width: u16, height: u16) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut cache = MessageRenderCache::new();
        let mut state = ChatState::new();
        terminal
            .draw(|f| {
                let lines = build_chat_lines(messages, &mut cache, width as usize);
                let view = ChatView { lines: &lines };
                f.render_stateful_widget(view, f.area(), &mut state);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }
    #[test]
    fn session_restore_shows_welcome_then_history_in_order() {
        let messages = vec![
            ChatMessage::system("Welcome to pie!"),
            ChatMessage::user("hello"),
            ChatMessage::assistant("hi there"),
        ];
        let buf = render_chat(&messages, 40, 10);

        let row0 = buffer_row(&buf, 0);
        assert!(
            row0.contains("Welcome"),
            "welcome message should be first row, got: {row0}"
        );

        let content = buffer_to_string(&buf);
        let welcome_pos = content.find("Welcome").unwrap_or(0);
        let hello_pos = content.find("hello").unwrap_or(0);
        assert!(
            hello_pos > welcome_pos,
            "user message should appear after welcome"
        );
    }

    #[test]
    fn chat_shows_user_message_with_arrow_prefix() {
        let messages = vec![ChatMessage::user("test query")];
        let buf = render_chat(&messages, 40, 5);
        let row0 = buffer_row(&buf, 0);
        assert!(
            row0.contains('>'),
            "user message should have > prefix, got: {row0}"
        );
    }

    #[test]
    fn auto_scroll_shows_latest_messages() {
        let messages: Vec<ChatMessage> = (0..20)
            .map(|i| ChatMessage::assistant(&format!("line {i}")))
            .collect();
        let buf = render_chat(&messages, 30, 5);
        let content = buffer_to_string(&buf);
        assert!(
            content.contains("line 19"),
            "auto_scroll should show latest messages, got: {content}"
        );
        assert!(
            !content.contains("line 0"),
            "auto_scroll should not show earliest messages"
        );
    }

    #[test]
    fn scroll_up_disables_auto_scroll() {
        let mut state = ChatState::new();
        assert!(state.auto_scroll);
        state.scroll_up(1);
        assert!(!state.auto_scroll, "scroll_up should disable auto_scroll");
    }

    // ── Helpers ─────────────────────────────────────────────────────

    fn buffer_row(buf: &Buffer, row: u16) -> String {
        let width = buf.area.width;
        (0..width)
            .map(|col| buf[(col, row)].symbol().to_string())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    fn buffer_to_string(buf: &Buffer) -> String {
        (0..buf.area.height)
            .map(|row| buffer_row(buf, row))
            .collect::<Vec<_>>()
            .join("\n")
    }
}
