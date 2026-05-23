use crate::session::Role;
use crate::ui::tui::state::ChatMessage;
use crate::ui::tui::widgets::render_cache::MessageRenderCache;
use tuirealm::ratatui::buffer::Buffer;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::style::Modifier;
use tuirealm::ratatui::widgets::{StatefulWidget, Widget};

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

    pub fn get_selected_text(
        &self,
        _messages: &[ChatMessage],
        cache: &MessageRenderCache,
        render_plan: &[ChatRenderItem],
    ) -> String {
        let (s, e) = if self.start.row < self.end.row
            || (self.start.row == self.end.row && self.start.col <= self.end.col)
        {
            (self.start, self.end)
        } else {
            (self.end, self.start)
        };

        let mut result = Vec::new();
        let mut current_line = 0;

        for item in render_plan {
            let item_height = item.height;
            if current_line + item_height <= s.row {
                current_line += item_height;
                continue;
            }
            if current_line > e.row {
                break;
            }

            match item.kind {
                ChatRenderKind::Message(msg_idx) => {
                    if let Some(lines) = cache.get_lines(msg_idx) {
                        for (i, line) in lines.iter().enumerate() {
                            let abs_row = current_line + i;
                            if abs_row >= s.row && abs_row <= e.row {
                                let line_text: String =
                                    line.spans.iter().map(|s| s.content.as_ref()).collect();
                                let chars: Vec<char> = line_text.chars().collect();
                                let start_col = if abs_row == s.row { s.col } else { 0 };
                                let end_col = if abs_row == e.row { e.col } else { chars.len() };

                                if start_col < chars.len() {
                                    let end_idx = end_col.min(chars.len());
                                    if start_col < end_idx
                                        && let Some(sub) = chars.get(start_col..end_idx)
                                    {
                                        result.push(sub.iter().collect::<String>());
                                    }
                                } else if abs_row >= s.row && abs_row <= e.row {
                                    // Empty line or selection beyond text
                                    result.push(String::new());
                                }
                            }
                        }
                    }
                }
                ChatRenderKind::EmptyLine => {
                    if current_line >= s.row && current_line <= e.row {
                        result.push(String::new());
                    }
                }
            }
            current_line += item_height;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatRenderKind {
    Message(usize),
    EmptyLine,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChatRenderItem {
    pub kind: ChatRenderKind,
    pub height: usize,
}

pub struct ChatView<'a> {
    pub cache: &'a mut MessageRenderCache,
    pub render_plan: &'a [ChatRenderItem],
    pub total_height: usize,
}

impl StatefulWidget for ChatView<'_> {
    type State = ChatState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let inner_height = area.height;
        #[allow(clippy::cast_possible_truncation)]
        let total_lines = self.total_height as u16;
        let max_scroll = total_lines.saturating_sub(inner_height);

        state.scroll_offset = if state.auto_scroll {
            max_scroll
        } else {
            state.scroll_offset.min(max_scroll)
        };

        let visible_start = state.scroll_offset as usize;
        let visible_end = visible_start + inner_height as usize;

        // 1. Render visible part from the render plan
        let mut current_line = 0;
        for item in self.render_plan {
            let item_height = item.height;

            if current_line + item_height <= visible_start {
                current_line += item_height;
                continue;
            }
            if current_line >= visible_end {
                break;
            }

            if let ChatRenderKind::Message(msg_idx) = item.kind
                && let Some(lines) = self.cache.get_lines(msg_idx)
            {
                for (i, line) in lines.iter().enumerate() {
                    let abs_line = current_line + i;
                    if abs_line < visible_start {
                        continue;
                    }
                    if abs_line >= visible_end {
                        break;
                    }

                    #[allow(clippy::cast_possible_truncation)]
                    let y = (abs_line - visible_start) as u16;
                    line.render(Rect::new(area.x, area.y + y, area.width, 1), buf);
                }
            }
            current_line += item_height;
        }

        // 2. Focused second pass: only apply REVERSED to selection cells
        let Some(sel) = state.selection else {
            return;
        };

        let (s, e) = if sel.start.row < sel.end.row
            || (sel.start.row == sel.end.row && sel.start.col <= sel.end.col)
        {
            (sel.start, sel.end)
        } else {
            (sel.end, sel.start)
        };

        let overlap_start = s.row.max(visible_start);
        let overlap_end = e.row.min(visible_end.saturating_sub(1));

        if overlap_start > overlap_end {
            return;
        }

        let mut current_line = 0;
        for item in self.render_plan {
            let item_height = item.height;

            if current_line + item_height <= overlap_start {
                current_line += item_height;
                continue;
            }
            if current_line > overlap_end {
                break;
            }

            if let ChatRenderKind::Message(msg_idx) = item.kind
                && let Some(lines) = self.cache.get_lines(msg_idx)
            {
                for (i, line) in lines.iter().enumerate() {
                    let abs_row = current_line + i;
                    if abs_row < overlap_start {
                        continue;
                    }
                    if abs_row > overlap_end {
                        break;
                    }

                    #[allow(clippy::cast_possible_truncation)]
                    let y = abs_row.saturating_sub(visible_start) as u16;
                    let mut x = 0;
                    for span in &line.spans {
                        let mut span_x = 0;
                        for ch in span.content.chars() {
                            let col = x + span_x;
                            if sel.contains(abs_row, col)
                                && let Some(cell) = buf.cell_mut((
                                    area.x + u16::try_from(col).unwrap_or(u16::MAX),
                                    area.y + y,
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
            current_line += item_height;
        }
    }
}

/// Build render plan from messages.
pub fn build_render_plan(
    messages: &[ChatMessage],
    cache: &mut MessageRenderCache,
    area_width: usize,
) -> (Vec<ChatRenderItem>, usize) {
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
    let mut items = Vec::new();
    let mut total_height = 0;

    for (render_pos, &msg_idx) in order.iter().enumerate() {
        let Some(msg) = messages.get(msg_idx) else {
            continue;
        };

        // Add a gap before the assistant response
        if msg.role == Role::Assistant && !items.is_empty() {
            items.push(ChatRenderItem {
                kind: ChatRenderKind::EmptyLine,
                height: 1,
            });
            total_height += 1;
        }

        let is_latest = render_pos == last_rendered;
        let rendered = cache.get_or_render(msg.role, &msg.content, is_latest, msg_idx, width);
        let height = rendered.len();
        items.push(ChatRenderItem {
            kind: ChatRenderKind::Message(msg_idx),
            height,
        });
        total_height += height;

        // Blank separator: only before user messages
        if let Some(&next_idx) = order.get(render_pos + 1)
            && let Some(next) = messages.get(next_idx)
            && next.role == Role::User
        {
            items.push(ChatRenderItem {
                kind: ChatRenderKind::EmptyLine,
                height: 1,
            });
            total_height += 1;
        }
    }

    (items, total_height)
}

// ── Helpers ─────────────────────────────────────────────────────

#[cfg(test)]
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
                let (plan, total_height) = build_render_plan(messages, &mut cache, width as usize);
                let view = ChatView {
                    cache: &mut cache,
                    render_plan: &plan,
                    total_height,
                };
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
