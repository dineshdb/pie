use crate::session::Role;
use crate::ui::tui::state::ChatMessage;
use crate::ui::tui::widgets::render_cache::MessageRenderCache;
use tuirealm::ratatui::buffer::Buffer;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::style::{Color, Modifier, Style};
use tuirealm::ratatui::text::{Line, Span};
use tuirealm::ratatui::widgets::{Paragraph, StatefulWidget, Widget};

const PREFIX_WIDTH: usize = 2;
const RIGHT_PAD: usize = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub row: usize,
    pub col: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub start: Position,
    pub end: Position,
}

impl Selection {
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    pub fn contains(&self, row: usize, col: usize) -> bool {
        let (s, e) = if self.start.row < self.end.row
            || (self.start.row == self.end.row && self.start.col <= self.end.col)
        {
            (self.start, self.end)
        } else {
            (self.end, self.start)
        };

        if row < s.row || row > e.row {
            return false;
        }
        if row == s.row && row == e.row {
            return col >= s.col && col < e.col;
        }
        if row == s.row {
            return col >= s.col;
        }
        if row == e.row {
            return col < e.col;
        }
        true
    }

    pub fn get_selected_text(&self, lines: &[Line<'_>]) -> String {
        let (s, e) = if self.start.row < self.end.row
            || (self.start.row == self.end.row && self.start.col <= self.end.col)
        {
            (self.start, self.end)
        } else {
            (self.end, self.start)
        };

        let mut result = Vec::new();
        for row in s.row..=e.row {
            if let Some(line) = lines.get(row) {
                let line_text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                let chars: Vec<char> = line_text.chars().collect();
                let start_col = if row == s.row { s.col } else { 0 };
                let end_col = if row == e.row { e.col } else { chars.len() };

                if start_col < chars.len() {
                    let end_idx = end_col.min(chars.len());
                    if start_col < end_idx
                        && let Some(sub) = chars.get(start_col..end_idx)
                    {
                        result.push(sub.iter().collect::<String>());
                    }
                }
            }
        }
        result.join("\n")
    }
}

pub struct ChatState {
    pub scroll_offset: u16,
    pub auto_scroll: bool,
    pub selection: Option<Selection>,
}

impl ChatState {
    pub fn new() -> Self {
        Self {
            scroll_offset: 0,
            auto_scroll: true,
            selection: None,
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

    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    pub fn start_selection(&mut self, row: usize, col: usize) {
        self.selection = Some(Selection {
            start: Position { row, col },
            end: Position { row, col },
        });
    }

    pub fn update_selection(&mut self, row: usize, col: usize) {
        if let Some(ref mut sel) = self.selection {
            sel.end = Position { row, col };
        }
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
        let visible_count = inner_height as usize;
        let visible_end = (visible_start + visible_count).min(self.lines.len());

        // 1. Fast background render: only clone VISIBLE lines for Paragraph.
        // This is O(visible_height) instead of O(total_history).
        let visible_lines: Vec<Line> = self
            .lines
            .get(visible_start..visible_end)
            .unwrap_or(&[])
            .to_vec();

        Paragraph::new(visible_lines).render(area, buf);

        // 2. Focused second pass: only apply REVERSED to selection cells
        if let Some(ref sel) = state.selection {
            let s_row = sel.start.row.min(sel.end.row);
            let e_row = sel.start.row.max(sel.end.row);

            let overlap_start = s_row.max(visible_start);
            let overlap_end = e_row.min(visible_end.saturating_sub(1));

            if overlap_start <= overlap_end {
                for abs_row in overlap_start..=overlap_end {
                    let y = abs_row - visible_start;
                    if let Some(line) = self.lines.get(abs_row) {
                        let mut x = 0;
                        for span in &line.spans {
                            let mut span_x = 0;
                            for ch in span.content.chars() {
                                let col = x + span_x;
                                if sel.contains(abs_row, col)
                                    && let Some(cell) = buf.cell_mut((
                                        area.x + u16::try_from(col).unwrap_or(u16::MAX),
                                        area.y + u16::try_from(y).unwrap_or(u16::MAX),
                                    ))
                                {
                                    let style = cell.style().add_modifier(Modifier::REVERSED);
                                    cell.set_style(style);
                                }
                                span_x += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
                            }
                            x += span_x;
                        }
                    }
                }
            }
        }
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
        } else if msg.role == Role::System && msg.content.starts_with("Welcome to") {
            append_welcome_line(&mut lines, &msg.content, width);
        } else {
            // Add a gap before the assistant response (streaming or finalized)
            if msg.role == Role::Assistant && !lines.is_empty() {
                lines.push(Line::raw(""));
            }
            let is_latest = render_pos == last_rendered;
            let rendered = cache.get_or_render(msg.role, &msg.content, is_latest, msg_idx, width);
            lines.extend(rendered.iter().cloned());
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

/// Render the welcome message with colored "pie" and "?".
fn append_welcome_line(lines: &mut Vec<Line<'static>>, content: &str, _width: usize) {
    let yellow = Style::default().fg(Color::Yellow);
    let cyan = Style::default().fg(Color::Cyan);
    let green = Style::default().fg(Color::Green);

    // "Welcome to pie! Type ? for help."
    let mut spans = vec![
        Span::styled("  ", Style::default().fg(Color::DarkGray)), // Match prefix width
    ];
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
    lines.push(Line::from(spans));
}

/// Render a tool call as exactly two lines using color to distinguish them:
/// ```text
///   name(params)            ← magenta
///     output text...        ← dark gray
/// ```
fn append_tool_lines(lines: &mut Vec<Line<'static>>, content: &str, width: usize) {
    let (call, output) = content.split_once(" → ").unwrap_or((content, ""));

    let call_text = truncate_str(call, width);
    let prefix = Span::styled("  ", Style::default().fg(Color::DarkGray));

    lines.push(Line::from(vec![
        prefix.clone(),
        Span::styled(call_text, Style::default().fg(Color::Magenta)),
    ]));

    if !output.is_empty() {
        let output_text = truncate_str(output, width.saturating_sub(4));
        let dim = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM);
        lines.push(Line::from(vec![
            prefix,
            Span::styled("└ ", dim),
            Span::styled(output_text, dim),
        ]));
    }
}

pub(crate) fn truncate_str(s: &str, max_len: usize) -> String {
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
    use tuirealm::ratatui::Terminal;
    use tuirealm::ratatui::backend::TestBackend;

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
