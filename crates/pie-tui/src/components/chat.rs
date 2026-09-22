//! `ChatComponent` — tuirealm component for the chat message display.
//!
//! Owns the message list, render cache, scroll state, and streaming response tracking.

use crate::door::CatalogEntry;
use crate::realm::{AskId, Msg, StreamEvent};
use crate::state::ChatMessage;
use crate::widgets::chat::{self, ChatState, ChatView};
use crate::widgets::render_cache::MessageRenderCache;
use crate::widgets::tool_display::ToolCallResult;
use pie_core::registry::Registry;
use std::sync::Arc;
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::{Event, Key, KeyModifiers, MouseEvent, MouseEventKind};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::state::State;

const MAX_MESSAGES: usize = 1_000;

#[derive(Debug, PartialEq)]
pub enum ActiveDialog {
    None,
    Help {
        scroll_offset: u16,
    },
    PermissionPrompt(PermissionPromptState),
    /// The `/model` picker over the startup catalog.
    ModelSelector(ModelSelectorState),
}

#[derive(Debug, Clone, PartialEq)]
pub struct PermissionPromptState {
    pub id: AskId,
    pub skill: String,
    pub permissions: Vec<String>,
}

/// The `/model` picker's state: the catalog and the navigated entry.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelSelectorState {
    pub entries: Vec<CatalogEntry>,
    /// The id the next turn carries (pending or confirmed), if any.
    pub current_id: Option<String>,
    pub selected_idx: usize,
}

pub struct ChatComponent {
    pub messages: Vec<ChatMessage>,
    pub render_cache: MessageRenderCache,
    pub chat_state: ChatState,
    pub response_idx: Option<usize>,
    pub active_dialog: ActiveDialog,
    pub last_area: Rect,
    pub render_plan: Vec<chat::ChatRenderItem>,
    pub total_height: usize,
    pub last_width: usize,
    pub registry: Arc<Registry>,
}

impl ChatComponent {
    pub fn new(messages: Vec<ChatMessage>, registry: Arc<Registry>) -> Self {
        Self {
            messages,
            render_cache: MessageRenderCache::new(),
            chat_state: ChatState::new(),
            response_idx: None,
            active_dialog: ActiveDialog::None,
            last_area: Rect::default(),
            render_plan: Vec::new(),
            total_height: 0,
            last_width: 0,
            registry,
        }
    }

    pub fn set_help_dialog(&mut self) {
        self.active_dialog = ActiveDialog::Help { scroll_offset: 0 };
    }

    // ── Message management ───────────────────────────────────────────

    pub fn add_message(&mut self, msg: ChatMessage) {
        if self.messages.len() >= MAX_MESSAGES {
            self.messages.remove(0);
            self.render_cache.trim_front(1);
            self.response_idx = self.response_idx.and_then(|i| i.checked_sub(1));
        }

        if let Some(idx) = self.response_idx {
            self.messages.insert(idx, msg);
            self.render_cache.insert(idx);
            self.response_idx = Some(idx + 1);
        } else {
            self.messages.push(msg);
            self.render_cache.push();
        }

        self.chat_state.auto_scroll = true;
        self.render_plan.clear();
    }

    /// Append a tool call's result to its pending header message, keyed by
    /// the id set when [`ChatMessage::tool_call`] was added. A no-op result
    /// line (e.g. `load_skills`) leaves the header as its own line.
    fn append_tool_result(&mut self, id: &str, result_line: &str) {
        if result_line.is_empty() {
            return;
        }
        let Some((idx, msg)) = self
            .messages
            .iter_mut()
            .enumerate()
            .rev()
            .find(|(_, m)| m.tool_id.as_deref() == Some(id))
        else {
            return;
        };
        msg.content = format!("{} → {result_line}", msg.content);
        self.render_cache.invalidate(idx);
        self.render_plan.clear();
    }

    pub fn clear_messages(&mut self) {
        self.messages.clear();
        self.render_cache.clear();
        self.response_idx = None;
        self.chat_state.scroll_offset = 0;
        self.chat_state.auto_scroll = true;
        self.render_plan.clear();
    }

    // ── Streaming lifecycle ──────────────────────────────────────────

    pub fn start_response(&mut self) {
        self.add_message(ChatMessage::response());
        self.response_idx = Some(self.messages.len() - 1);
        self.render_plan.clear();
    }

    pub fn update_response(&mut self, delta: &str) {
        if let Some(idx) = self.response_idx
            && let Some(msg) = self.messages.get_mut(idx)
        {
            msg.content.push_str(delta);
            self.chat_state.scroll_to_bottom();
            self.render_plan.clear();
        }
    }
    pub fn finish_stream(&mut self, output: String) {
        if let Some(idx) = self.response_idx
            && let Some(msg) = self.messages.get_mut(idx)
        {
            msg.set_content(output);
            msg.finalize_response();
            self.render_cache.invalidate(idx);
        }
        self.response_idx = None;
        self.render_plan.clear();
    }

    pub fn stream_error(&mut self, err: &str) {
        self.finish_stream(format!("Error: {err}"));
    }

    pub fn is_streaming(&self) -> bool {
        self.response_idx.is_some()
    }

    fn get_help_total_lines(registry: &Registry) -> u16 {
        let mut total = 13; // Commands (7) + Keys (6)
        if !registry.agents.is_empty() {
            #[allow(clippy::cast_possible_truncation)]
            {
                total += 3 + registry.agents.len() as u16;
            }
        }
        if !registry.skills.is_empty() {
            #[allow(clippy::cast_possible_truncation)]
            {
                total += 3 + registry.skills.len() as u16;
            }
        }
        total
    }

    // ── Scrolling ────────────────────────────────────────────────────

    pub fn scroll_up(&mut self, amount: u16) {
        self.chat_state.scroll_up(amount);
    }

    pub fn scroll_down(&mut self, amount: u16) {
        self.chat_state.scroll_down(amount);
    }

    pub fn get_selected_text(&self) -> Option<String> {
        self.chat_state
            .selection
            .map(|sel| sel.get_selected_text(&self.messages, &self.render_cache, &self.render_plan))
    }
}

impl Component for ChatComponent {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        // 1. Rebuild plan only if width changed or content changed (plan cleared elsewhere)
        if self.render_plan.is_empty() || self.last_width != area.width as usize {
            let (plan, height) = chat::build_render_plan(
                &self.messages,
                &mut self.render_cache,
                area.width as usize,
            );
            self.render_plan = plan;
            self.total_height = height;
            self.last_width = area.width as usize;
        }
        self.last_area = area;

        // 2. Render visible part
        frame.render_stateful_widget(
            ChatView {
                cache: &mut self.render_cache,
                render_plan: &self.render_plan,
                total_height: self.total_height,
            },
            area,
            &mut self.chat_state,
        );

        match &self.active_dialog {
            ActiveDialog::None => {}
            ActiveDialog::Help { scroll_offset } => {
                frame.render_widget(
                    super::super::widgets::dialog::Dialog::new(
                        "Help",
                        super::super::widgets::help::HelpOverlay {
                            agents: &self.registry.agents,
                            skills: &self.registry.skills,
                            scroll_offset: *scroll_offset,
                        },
                    )
                    .with_size(70, 70),
                    area,
                );
            }
            ActiveDialog::PermissionPrompt(state) => {
                let perm_lines: Vec<String> = state
                    .permissions
                    .iter()
                    .map(|p| format!("  - {p}"))
                    .collect();
                let body = format!(
                    "'{}' wants permission:\n{}\n\n[Enter] Allow   [Esc/n] Deny",
                    state.skill,
                    perm_lines.join("\n")
                );
                let para = tuirealm::ratatui::widgets::Paragraph::new(body).style(
                    tuirealm::ratatui::style::Style::default()
                        .fg(tuirealm::ratatui::style::Color::Yellow),
                );
                frame.render_widget(
                    super::super::widgets::dialog::Dialog::new(" Permission Required ", para)
                        .with_size(70, 35),
                    area,
                );
            }
            ActiveDialog::ModelSelector(state) => {
                frame.render_widget(
                    super::super::widgets::dialog::Dialog::new(
                        " Model (Enter select · Esc close) ",
                        super::super::widgets::model_selector::ModelSelectorOverlay {
                            entries: &state.entries,
                            current_id: state.current_id.as_deref(),
                            selected_idx: state.selected_idx,
                        },
                    )
                    .with_size(60, 40),
                    area,
                );
            }
        }
    }

    fn state(&self) -> State {
        State::None
    }

    fn query(&self, _attr: Attribute) -> Option<QueryResult<'_>> {
        None
    }

    fn attr(&mut self, _attr: Attribute, _value: AttrValue) {}

    fn perform(&mut self, _cmd: Cmd) -> CmdResult {
        CmdResult::NoChange
    }
}

impl AppComponent<Msg, StreamEvent> for ChatComponent {
    fn on(&mut self, ev: &Event<StreamEvent>) -> Option<Msg> {
        match ev {
            Event::User(user_ev) => Some(self.handle_user_event(user_ev)),
            Event::Keyboard(key) => Some(self.handle_keyboard_event(key)),
            Event::Mouse(ev) => self.handle_mouse_event(*ev),
            _ => None,
        }
    }
}

impl ChatComponent {
    fn handle_mouse_event(&mut self, ev: MouseEvent) -> Option<Msg> {
        if self.active_dialog != ActiveDialog::None {
            return None;
        }

        match ev.kind {
            MouseEventKind::ScrollUp => Some(Msg::ScrollChat(-1)),
            MouseEventKind::ScrollDown => Some(Msg::ScrollChat(1)),
            MouseEventKind::Down(_) => {
                if self
                    .last_area
                    .contains(tuirealm::ratatui::layout::Position::new(ev.column, ev.row))
                {
                    let rel_row = ev.row.saturating_sub(self.last_area.y) as usize;
                    let rel_col = ev.column.saturating_sub(self.last_area.x) as usize;
                    let abs_row = self.chat_state.scroll_offset as usize + rel_row;
                    self.chat_state.start_selection(abs_row, rel_col);
                    Some(Msg::Redraw)
                } else {
                    self.chat_state.clear_selection();
                    Some(Msg::Redraw)
                }
            }
            MouseEventKind::Drag(_) => {
                if self.chat_state.selection.is_some() {
                    let rel_row = ev.row.saturating_sub(self.last_area.y) as usize;
                    let rel_col = ev.column.saturating_sub(self.last_area.x) as usize;
                    let abs_row = self.chat_state.scroll_offset as usize + rel_row;
                    self.chat_state.update_selection(abs_row, rel_col);
                    Some(Msg::Redraw)
                } else {
                    None
                }
            }
            MouseEventKind::Up(_) => {
                if let Some(sel) = self.chat_state.selection
                    && !sel.is_empty()
                {
                    Some(Msg::CopySelection)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn handle_user_event(&mut self, ev: &StreamEvent) -> Msg {
        match ev {
            StreamEvent::Delta(s) => {
                self.update_response(s);
                Msg::Redraw
            }
            StreamEvent::Done(s) => Msg::StreamDone(s.clone()),
            StreamEvent::Error(s) => Msg::StreamError(s.clone()),
            StreamEvent::ToolCall {
                id,
                name,
                display,
                output,
                failed,
            } => {
                if output.is_empty() {
                    self.add_message(ChatMessage::tool_call(id, display));
                    return Msg::Redraw;
                }

                let tool = ToolCallResult::new(name, output, *failed);
                let result_line = tool.to_string();
                self.append_tool_result(id, &result_line);
                Msg::Redraw
            }
            StreamEvent::PermissionAsk {
                id,
                skill,
                permissions,
            } => {
                self.active_dialog = ActiveDialog::PermissionPrompt(PermissionPromptState {
                    id: id.clone(),
                    skill: skill.clone(),
                    permissions: permissions.clone(),
                });
                Msg::Redraw
            }
            StreamEvent::SessionSwitched(session_id) => {
                // The realm loop resets the input component onto it.
                Msg::SessionSwitched(session_id.clone())
            }
        }
    }

    fn handle_keyboard_event(&mut self, key: &tuirealm::event::KeyEvent) -> Msg {
        let msg = match &mut self.active_dialog {
            ActiveDialog::None => None,
            ActiveDialog::Help { scroll_offset } => {
                let total_lines = Self::get_help_total_lines(&self.registry);
                let dialog_height = (self.last_area.height * 70 / 100).saturating_sub(2);
                let max_scroll = total_lines.saturating_sub(dialog_height);
                Self::handle_help_keyboard_event(key, scroll_offset, max_scroll)
            }
            ActiveDialog::PermissionPrompt(_) => {
                Some(self.handle_permission_prompt_keyboard_event(key))
            }
            ActiveDialog::ModelSelector(state) => Some(Self::model_selector_key(key, state)),
        };

        if let Some(m) = msg {
            if matches!(m, Msg::Redraw) && matches!(self.active_dialog, ActiveDialog::Help { .. }) {
                // If it was a close command, handle it here
                if let Key::Esc | Key::Char('?') = key.code {
                    self.active_dialog = ActiveDialog::None;
                }
            }
            if let Key::Enter | Key::Char('n') = key.code
                && matches!(self.active_dialog, ActiveDialog::PermissionPrompt(_))
            {
                self.active_dialog = ActiveDialog::None;
            }
            // The model picker closes on confirm and cancel alike — the
            // confirmed id rides `Msg::SelectModel` out.
            if let Key::Enter | Key::Esc = key.code
                && matches!(self.active_dialog, ActiveDialog::ModelSelector(_))
            {
                self.active_dialog = ActiveDialog::None;
            }
            return m;
        }

        match (key.modifiers, &key.code) {
            (KeyModifiers::NONE, Key::PageUp) => {
                self.scroll_up(20);
                Msg::Redraw
            }
            (KeyModifiers::NONE, Key::PageDown) => {
                self.scroll_down(20);
                Msg::Redraw
            }
            _ => Msg::KeyboardToInput(*key),
        }
    }

    /// The `/model` picker's keys: Up/Down navigate, Enter confirms the
    /// navigated entry (the realm loop turns it into a pending
    /// selection), Esc closes. Closing the dialog is the caller's —
    /// the state borrows it for the match.
    fn model_selector_key(key: &tuirealm::event::KeyEvent, state: &mut ModelSelectorState) -> Msg {
        let last = state.entries.len().saturating_sub(1);
        match (&key.code, key.modifiers) {
            (Key::Up, KeyModifiers::NONE) => {
                state.selected_idx = state.selected_idx.saturating_sub(1).min(last);
                Msg::Redraw
            }
            (Key::Down, KeyModifiers::NONE) => {
                state.selected_idx = (state.selected_idx + 1).min(last);
                Msg::Redraw
            }
            (Key::Enter, _) => {
                let Some(entry) = state.entries.get(state.selected_idx) else {
                    return Msg::Redraw;
                };
                Msg::SelectModel(entry.id.clone())
            }
            _ => Msg::Redraw,
        }
    }

    fn handle_help_keyboard_event(
        key: &tuirealm::event::KeyEvent,
        scroll_offset: &mut u16,
        max_scroll: u16,
    ) -> Option<Msg> {
        match (&key.code, key.modifiers) {
            (Key::Esc | Key::Char('?'), _) => Some(Msg::Redraw),
            (Key::Up, KeyModifiers::NONE) => {
                if *scroll_offset > 0 {
                    *scroll_offset = scroll_offset.saturating_sub(1);
                    return Some(Msg::Redraw);
                }
                None
            }
            (Key::Down, KeyModifiers::NONE) => {
                if *scroll_offset < max_scroll {
                    *scroll_offset = (*scroll_offset + 1).min(max_scroll);
                    return Some(Msg::Redraw);
                }
                None
            }
            (Key::PageUp, KeyModifiers::NONE) => {
                if *scroll_offset > 0 {
                    *scroll_offset = scroll_offset.saturating_sub(10);
                    return Some(Msg::Redraw);
                }
                None
            }
            (Key::PageDown, KeyModifiers::NONE) => {
                if *scroll_offset < max_scroll {
                    *scroll_offset = (*scroll_offset + 10).min(max_scroll);
                    return Some(Msg::Redraw);
                }
                None
            }
            _ => None,
        }
    }

    fn handle_permission_prompt_keyboard_event(&mut self, key: &tuirealm::event::KeyEvent) -> Msg {
        let answer = match (key.modifiers, &key.code) {
            (KeyModifiers::NONE, Key::Enter) => Some(true),
            (KeyModifiers::NONE, Key::Esc | Key::Char('n')) => Some(false),
            _ => None,
        };
        if let (ActiveDialog::PermissionPrompt(state), Some(allow)) = (&self.active_dialog, answer)
        {
            let id = state.id.clone();
            self.active_dialog = ActiveDialog::None;
            // The realm loop routes the answer through the door client.
            return Msg::AnswerPermission(id, allow);
        }
        Msg::Redraw
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pie_core::session::Role;

    fn test_registry() -> Arc<Registry> {
        Arc::new(Registry {
            agents: Vec::new(),
            skills: Vec::new(),
            completions: Vec::new(),
        })
    }

    #[tokio::test]
    async fn new_chat_has_welcome_message_first() {
        let messages = vec![ChatMessage::system("Welcome to pie! Type ? for help.")];
        let chat = ChatComponent::new(messages, test_registry());
        assert_eq!(chat.messages.len(), 1);
        assert_eq!(chat.messages[0].role, Role::System);
        assert!(chat.messages[0].content.contains("Welcome"));
    }

    #[tokio::test]
    async fn add_message_auto_scrolls() {
        let mut chat = ChatComponent::new(vec![ChatMessage::system("Welcome")], test_registry());
        chat.chat_state.auto_scroll = false;
        chat.add_message(ChatMessage::user("test"));
        assert!(
            chat.chat_state.auto_scroll,
            "add_message should enable auto_scroll"
        );
    }

    #[tokio::test]
    async fn start_and_finish_stream() {
        let mut chat = ChatComponent::new(vec![], test_registry());
        chat.start_response();
        assert!(chat.is_streaming());
        assert_eq!(chat.response_idx, Some(0));
        assert!(chat.messages[0].is_response());

        chat.update_response("Hello");
        assert_eq!(chat.messages[0].content, "Hello");

        chat.finish_stream("Hello world".to_string());
        assert!(!chat.is_streaming());
        assert_eq!(chat.response_idx, None);
        assert!(!chat.messages[0].is_response());
        assert_eq!(chat.messages[0].content, "Hello world");
    }

    #[tokio::test]
    async fn tool_calls_appear_before_active_response() {
        let mut chat = ChatComponent::new(vec![ChatMessage::user("run tool")], test_registry());
        chat.start_response(); // idx 1
        chat.update_response("I will run a tool");
        chat.add_message(ChatMessage::tool("tool result"));

        assert_eq!(chat.messages.len(), 3);
        assert_eq!(chat.messages[1].role, Role::Tool);
        assert_eq!(chat.messages[2].role, Role::Assistant);
        assert!(chat.messages[2].is_response());
        assert_eq!(chat.response_idx, Some(2));
    }

    /// A tool call's pre- and post-execution halves must land as one
    /// message, not two — two meant a blank line between the call header
    /// and its result (the header rendered alone, then an empty call line
    /// plus the result on its own line).
    #[tokio::test]
    async fn tool_call_pre_and_post_execution_merge_into_one_message() {
        let mut chat = ChatComponent::new(vec![], test_registry());

        chat.handle_user_event(&StreamEvent::ToolCall {
            id: "call-1".to_string(),
            name: "Read".to_string(),
            display: "Read{path = a.rs}".to_string(),
            output: String::new(),
            failed: false,
        });
        assert_eq!(chat.messages.len(), 1);
        assert_eq!(chat.messages[0].content, "Read{path = a.rs}");

        chat.handle_user_event(&StreamEvent::ToolCall {
            id: "call-1".to_string(),
            name: "Read".to_string(),
            display: String::new(),
            output: "hello".to_string(),
            failed: false,
        });
        assert_eq!(
            chat.messages.len(),
            1,
            "completion must merge into the pending message, not add a new one"
        );
        assert_eq!(chat.messages[0].content, "Read{path = a.rs} → hello");
    }

    /// Two tool calls in flight at once (a parallel batch) must merge by
    /// id, not by "most recently added" — otherwise call B's result would
    /// land on call A's header.
    #[tokio::test]
    async fn concurrent_tool_calls_merge_by_id_not_by_order() {
        let mut chat = ChatComponent::new(vec![], test_registry());

        chat.handle_user_event(&StreamEvent::ToolCall {
            id: "a".to_string(),
            name: "Bash".to_string(),
            display: "Bash{command = one}".to_string(),
            output: String::new(),
            failed: false,
        });
        chat.handle_user_event(&StreamEvent::ToolCall {
            id: "b".to_string(),
            name: "Bash".to_string(),
            display: "Bash{command = two}".to_string(),
            output: String::new(),
            failed: false,
        });
        // b finishes first
        chat.handle_user_event(&StreamEvent::ToolCall {
            id: "b".to_string(),
            name: "Bash".to_string(),
            display: String::new(),
            output: r#"{"code":0,"stdout":"two-out","stderr":""}"#.to_string(),
            failed: false,
        });
        chat.handle_user_event(&StreamEvent::ToolCall {
            id: "a".to_string(),
            name: "Bash".to_string(),
            display: String::new(),
            output: r#"{"code":0,"stdout":"one-out","stderr":""}"#.to_string(),
            failed: false,
        });

        assert_eq!(chat.messages.len(), 2);
        assert!(
            chat.messages[0].content.contains("one")
                && chat.messages[0].content.contains("one-out")
        );
        assert!(
            chat.messages[1].content.contains("two")
                && chat.messages[1].content.contains("two-out")
        );
    }
}
