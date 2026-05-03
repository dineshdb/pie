//! `ChatComponent` — tuirealm component for the chat message display.
//!
//! Owns the message list, render cache, scroll state, and streaming response tracking.

use crate::db::DbPool;
use crate::registry::Registry;
use crate::ui::tui::realm::{Msg, StreamEvent};
use crate::ui::tui::state::ChatMessage;
use crate::ui::tui::widgets::chat::{self, ChatState, ChatView};
use crate::ui::tui::widgets::render_cache::MessageRenderCache;
use crate::ui::tui::widgets::tool_display::ToolCallResult;
use std::sync::Arc;
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::{Event, Key, KeyModifiers, MouseEvent, MouseEventKind};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::state::State;

const MAX_MESSAGES: usize = 1_000;

#[derive(Debug, Clone, PartialEq)]
pub enum ActiveDialog {
    None,
    Help {
        scroll_offset: u16,
    },
    ModelSelector {
        providers: Vec<String>,
        provider_idx: usize,
        models: Vec<String>,
        selected_idx: Option<usize>,
        is_loading: bool,
        error: Option<String>,
    },
}
pub struct ChatComponent {
    pub messages: Vec<ChatMessage>,
    pub render_cache: MessageRenderCache,
    pub chat_state: ChatState,
    pub response_idx: Option<usize>,
    pub active_dialog: ActiveDialog,
    pub current_model: String,
    pub last_area: Rect,
    pub cached_lines: Vec<tuirealm::ratatui::text::Line<'static>>,
    pub last_width: usize,
    pub registry: Arc<Registry>,
    pub pool: Arc<DbPool>,
    pub session_id: String,
    pub show_plan: bool,
}

impl ChatComponent {
    pub fn new(
        messages: Vec<ChatMessage>,
        current_model: String,
        registry: Arc<Registry>,
        pool: Arc<DbPool>,
        session_id: String,
    ) -> Self {
        Self {
            messages,
            render_cache: MessageRenderCache::new(),
            chat_state: ChatState::new(),
            response_idx: None,
            active_dialog: ActiveDialog::None,
            current_model,
            last_area: Rect::default(),
            cached_lines: Vec::new(),
            last_width: 0,
            registry,
            pool,
            session_id,
            show_plan: true,
        }
    }

    pub fn toggle_plan(&mut self) {
        self.show_plan = !self.show_plan;
        self.cached_lines.clear();
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
        self.messages.push(msg);
        self.render_cache.push();
        self.chat_state.auto_scroll = true;
        self.cached_lines.clear();
    }
    pub fn clear_messages(&mut self) {
        self.messages.clear();
        self.render_cache.clear();
        self.response_idx = None;
        self.chat_state.scroll_offset = 0;
        self.chat_state.auto_scroll = true;
        self.cached_lines.clear();
    }

    // ── Streaming lifecycle ──────────────────────────────────────────

    pub fn start_response(&mut self) {
        self.add_message(ChatMessage::response());
        self.response_idx = Some(self.messages.len() - 1);
        self.cached_lines.clear();
    }

    pub fn update_response(&mut self, content: String) {
        if let Some(idx) = self.response_idx
            && let Some(msg) = self.messages.get_mut(idx)
        {
            msg.set_content(content);
            self.chat_state.scroll_to_bottom();
            self.cached_lines.clear();
        }
    }
    pub fn finish_stream(&mut self, output: String) {
        if let Some(idx) = self.response_idx {
            if let Some(msg) = self.messages.get_mut(idx) {
                msg.set_content(output);
                msg.finalize_response();
            }
            let msg = self.messages.remove(idx);
            self.messages.push(msg);
            self.render_cache.shift_remove(idx);
        }
        self.response_idx = None;
        self.cached_lines.clear();
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
            .map(|sel| sel.get_selected_text(&self.cached_lines))
    }
}

impl Component for ChatComponent {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        // 1. Rebuild cache only if width changed or content changed (cache cleared elsewhere)
        if self.cached_lines.is_empty() || self.last_width != area.width as usize {
            self.cached_lines =
                chat::build_chat_lines(&self.messages, &mut self.render_cache, area.width as usize);
            self.last_width = area.width as usize;
        }
        self.last_area = area;

        // 2. Render visible part
        frame.render_stateful_widget(
            ChatView {
                lines: &self.cached_lines,
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
            ActiveDialog::ModelSelector {
                providers,
                provider_idx,
                models,
                selected_idx,
                is_loading,
                error,
            } => {
                let provider_name = providers.get(*provider_idx).cloned().unwrap_or_default();
                let title = if provider_name.is_empty() {
                    "Select Model".to_string()
                } else {
                    format!("Select Provider / Model ({provider_name})")
                };

                let current_model_idx = if *is_loading {
                    None
                } else {
                    let current = self.current_model.trim().to_lowercase();
                    models.iter().position(|m| {
                        let m_lower = m.trim().to_lowercase();
                        m_lower == current
                            || m_lower.ends_with(&format!("/{current}"))
                            || current.ends_with(&format!("/{m_lower}"))
                    })
                };

                frame.render_widget(
                    super::super::widgets::dialog::Dialog::new(
                        &title,
                        super::super::widgets::model_selector::ModelSelectorOverlay {
                            providers,
                            provider_idx: *provider_idx,
                            models,
                            selected_idx: *selected_idx,
                            current_model_idx,
                            is_loading: *is_loading,
                            error: error.as_deref(),
                        },
                    )
                    .with_size(80, 80),
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
                self.update_response(s.clone());
                Msg::Redraw
            }
            StreamEvent::Done(s) => {
                self.finish_stream(s.clone());
                Msg::StreamDone(s.clone())
            }
            StreamEvent::Error(s) => {
                if let ActiveDialog::ModelSelector {
                    is_loading, error, ..
                } = &mut self.active_dialog
                {
                    *is_loading = false;
                    *error = Some(s.clone());
                } else {
                    self.stream_error(s);
                }
                Msg::Redraw
            }
            StreamEvent::ToolCall {
                name,
                display,
                output,
            } => {
                let tool = ToolCallResult::new(name, output);
                let result_line = tool.to_string();
                let content = if result_line.is_empty() {
                    display.clone()
                } else {
                    format!("{display} → {result_line}")
                };
                self.add_message(ChatMessage::tool(&content));
                Msg::Redraw
            }
            StreamEvent::PlanUpdate => {
                self.cached_lines.clear();
                Msg::Redraw
            }
            StreamEvent::ModelList(models) => {
                tracing::info!(count = models.len(), "received ModelList in ChatComponent");
                if let ActiveDialog::ModelSelector {
                    providers,
                    provider_idx,
                    ..
                } = &self.active_dialog
                {
                    tracing::info!(provider = %providers.get(*provider_idx).cloned().unwrap_or_default(), "updating ModelSelector state");
                    let models = models.clone();
                    let providers = providers.clone();
                    let provider_idx = *provider_idx;

                    // Match current_model flexibly
                    let current = self.current_model.trim().to_lowercase();
                    let selected_idx = models
                        .iter()
                        .position(|m| {
                            let m_lower = m.trim().to_lowercase();
                            m_lower == current
                                || m_lower.ends_with(&format!("/{current}"))
                                || current.ends_with(&format!("/{m_lower}"))
                        })
                        .or(if models.is_empty() { None } else { Some(0) });

                    tracing::info!(selected = ?selected_idx, "selected index determined");

                    self.active_dialog = ActiveDialog::ModelSelector {
                        providers,
                        provider_idx,
                        models,
                        selected_idx,
                        is_loading: false,
                        error: None,
                    };
                } else {
                    tracing::warn!(dialog = ?self.active_dialog, "received ModelList but ModelSelector is not active");
                }
                Msg::Redraw
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
            ActiveDialog::ModelSelector {
                providers,
                provider_idx,
                models,
                selected_idx,
                is_loading,
                error,
            } => Self::handle_model_selector_keyboard_event(
                key,
                providers,
                provider_idx,
                models,
                selected_idx,
                *is_loading,
                error,
            ),
        };

        if let Some(m) = msg {
            if matches!(m, Msg::Redraw)
                && (matches!(self.active_dialog, ActiveDialog::Help { .. })
                    || matches!(self.active_dialog, ActiveDialog::ModelSelector { .. }))
            {
                // If it was a close command, handle it here
                if let Key::Esc | Key::Char('?') = key.code {
                    self.active_dialog = ActiveDialog::None;
                }
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
            (KeyModifiers::CONTROL, Key::Char('t')) => {
                self.toggle_plan();
                Msg::Redraw
            }
            _ => Msg::KeyboardToInput(*key),
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

    #[allow(clippy::too_many_arguments)]
    fn handle_model_selector_keyboard_event(
        key: &tuirealm::event::KeyEvent,
        providers: &[String],
        provider_idx: &mut usize,
        models: &[String],
        selected_idx: &mut Option<usize>,
        is_loading: bool,
        error: &mut Option<String>,
    ) -> Option<Msg> {
        match (key.modifiers, &key.code) {
            (KeyModifiers::NONE, Key::Up) => {
                if let Some(idx) = selected_idx
                    && *idx > 0
                {
                    *selected_idx = Some(*idx - 1);
                }
                Some(Msg::Redraw)
            }
            (KeyModifiers::NONE, Key::Down) => {
                if let Some(idx) = selected_idx
                    && *idx + 1 < models.len()
                {
                    *selected_idx = Some(*idx + 1);
                }
                Some(Msg::Redraw)
            }
            (KeyModifiers::NONE, Key::Left) => {
                if *provider_idx > 0 {
                    *provider_idx -= 1;
                    *selected_idx = None;
                    *error = None;
                    let provider_name = providers.get(*provider_idx).cloned().unwrap_or_default();
                    return Some(Msg::FetchModels(provider_name));
                }
                Some(Msg::Redraw)
            }
            (KeyModifiers::NONE, Key::Right) => {
                if *provider_idx + 1 < providers.len() {
                    *provider_idx += 1;
                    *selected_idx = None;
                    *error = None;
                    let provider_name = providers.get(*provider_idx).cloned().unwrap_or_default();
                    return Some(Msg::FetchModels(provider_name));
                }
                Some(Msg::Redraw)
            }
            (KeyModifiers::NONE, Key::Enter) => {
                if let Some(idx) = selected_idx
                    && let Some(model) = models.get(*idx)
                {
                    let model = model.clone();
                    let provider_name = providers.get(*provider_idx).cloned().unwrap_or_default();
                    return Some(Msg::SwitchProviderAndModel(provider_name, model));
                }
                None
            }
            (KeyModifiers::NONE, Key::Esc) => {
                if !is_loading {
                    return Some(Msg::Redraw);
                }
                None
            }
            _ => None,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::session::Role;

    fn test_registry() -> Arc<Registry> {
        Arc::new(Registry {
            agents: Vec::new(),
            skills: Vec::new(),
            completions: Vec::new(),
        })
    }

    fn test_pool() -> Arc<DbPool> {
        Arc::new(crate::db::create_test_pool().unwrap())
    }

    #[test]
    fn new_chat_has_welcome_message_first() {
        let messages = vec![ChatMessage::system("Welcome to pie! Type ? for help.")];
        let chat = ChatComponent::new(
            messages,
            "test-model".to_string(),
            test_registry(),
            test_pool(),
            "test-session".to_string(),
        );
        assert_eq!(chat.messages.len(), 1);
        assert_eq!(chat.messages[0].role, Role::System);
        assert!(chat.messages[0].content.contains("Welcome"));
        assert!(chat.show_plan);
    }

    #[test]
    fn add_message_auto_scrolls() {
        let mut chat = ChatComponent::new(
            vec![ChatMessage::system("Welcome")],
            "test-model".to_string(),
            test_registry(),
            test_pool(),
            "test-session".to_string(),
        );
        chat.chat_state.auto_scroll = false;
        chat.add_message(ChatMessage::user("test"));
        assert!(
            chat.chat_state.auto_scroll,
            "add_message should enable auto_scroll"
        );
    }

    #[test]
    fn toggle_plan_state() {
        let mut chat = ChatComponent::new(
            vec![],
            "test-model".to_string(),
            test_registry(),
            test_pool(),
            "test-session".to_string(),
        );
        assert!(chat.show_plan);
        chat.toggle_plan();
        assert!(!chat.show_plan);
        chat.toggle_plan();
        assert!(chat.show_plan);
    }

    #[test]
    fn start_and_finish_stream() {
        let mut chat = ChatComponent::new(
            vec![],
            "test-model".to_string(),
            test_registry(),
            test_pool(),
            "test-session".to_string(),
        );
        chat.start_response();
        assert!(chat.is_streaming());
        assert_eq!(chat.response_idx, Some(0));
        assert!(chat.messages[0].is_response());

        chat.update_response("Hello".to_string());
        assert_eq!(chat.messages[0].content, "Hello");

        chat.finish_stream("Hello world".to_string());
        assert!(!chat.is_streaming());
        assert_eq!(chat.response_idx, None);
        assert!(!chat.messages[0].is_response());
        assert_eq!(chat.messages[0].content, "Hello world");
    }
}
