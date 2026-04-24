//! `ChatComponent` — tuirealm component for the chat message display.
//!
//! Owns the message list, render cache, scroll state, and streaming response tracking.

use crate::ui::tui::realm::{Msg, StreamEvent};
use crate::ui::tui::state::ChatMessage;
use crate::ui::tui::widgets::chat::{self, ChatState, ChatView};
use crate::ui::tui::widgets::render_cache::MessageRenderCache;
use crate::ui::tui::widgets::tool_display::ToolCallResult;
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::{Event, Key, KeyModifiers};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::state::State;

const MAX_MESSAGES: usize = 1_000;

#[derive(Debug, Clone, PartialEq)]
pub enum ActiveDialog {
    None,
    Help,
    ModelSelector {
        providers: Vec<String>,
        provider_idx: usize,
        models: Vec<String>,
        selected_idx: Option<usize>,
        is_loading: bool,
    },
}

pub struct ChatComponent {
    pub messages: Vec<ChatMessage>,
    pub render_cache: MessageRenderCache,
    pub chat_state: ChatState,
    pub response_idx: Option<usize>,
    pub active_dialog: ActiveDialog,
    pub current_model: String,
}

impl ChatComponent {
    pub fn new(messages: Vec<ChatMessage>, current_model: String) -> Self {
        Self {
            messages,
            render_cache: MessageRenderCache::new(),
            chat_state: ChatState::new(),
            response_idx: None,
            active_dialog: ActiveDialog::None,
            current_model,
        }
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
    }

    pub fn clear_messages(&mut self) {
        self.messages.clear();
        self.render_cache.clear();
        self.response_idx = None;
        self.chat_state.scroll_offset = 0;
        self.chat_state.auto_scroll = true;
    }

    // ── Streaming lifecycle ──────────────────────────────────────────

    pub fn start_response(&mut self) {
        self.add_message(ChatMessage::response());
        self.response_idx = Some(self.messages.len() - 1);
    }

    pub fn update_response(&mut self, content: String) {
        if let Some(idx) = self.response_idx
            && let Some(msg) = self.messages.get_mut(idx)
        {
            msg.set_content(content);
            self.chat_state.scroll_to_bottom();
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
    }

    pub fn stream_error(&mut self, err: &str) {
        self.finish_stream(format!("Error: {err}"));
    }

    pub fn is_streaming(&self) -> bool {
        self.response_idx.is_some()
    }

    // ── Scrolling ────────────────────────────────────────────────────

    pub fn scroll_up(&mut self, amount: u16) {
        self.chat_state.scroll_up(amount);
    }

    pub fn scroll_down(&mut self, amount: u16) {
        self.chat_state.scroll_down(amount);
    }
}

impl Component for ChatComponent {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        // Render chat in background
        let lines =
            chat::build_chat_lines(&self.messages, &mut self.render_cache, area.width as usize);
        frame.render_stateful_widget(ChatView { lines: &lines }, area, &mut self.chat_state);

        match &self.active_dialog {
            ActiveDialog::None => {}
            ActiveDialog::Help => {
                frame.render_widget(
                    super::super::widgets::dialog::Dialog::new(
                        "Help",
                        super::super::widgets::help::HelpOverlay,
                    )
                    .with_size(50, 60),
                    area,
                );
            }
            ActiveDialog::ModelSelector {
                providers,
                provider_idx,
                models,
                selected_idx,
                is_loading,
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
                        },
                    ),
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
            Event::User(user_ev) => self.handle_user_event(user_ev),
            Event::Keyboard(key) => self.handle_keyboard_event(key),
            _ => None,
        }
    }
}

impl ChatComponent {
    fn handle_user_event(&mut self, ev: &StreamEvent) -> Option<Msg> {
        match ev {
            StreamEvent::Delta(s) => {
                self.update_response(s.clone());
                None
            }
            StreamEvent::Done(s) => {
                self.finish_stream(s.clone());
                Some(Msg::StreamDone(s.clone()))
            }
            StreamEvent::Error(s) => {
                if let ActiveDialog::ModelSelector { .. } = self.active_dialog {
                    self.active_dialog = ActiveDialog::None;
                }
                self.stream_error(s);
                Some(Msg::StreamError(s.clone()))
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
                None
            }
            StreamEvent::ModelList(models) => {
                if let ActiveDialog::ModelSelector {
                    providers,
                    provider_idx,
                    ..
                } = &self.active_dialog
                {
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

                    self.active_dialog = ActiveDialog::ModelSelector {
                        providers,
                        provider_idx,
                        models,
                        selected_idx,
                        is_loading: false,
                    };
                }
                Some(Msg::Redraw)
            }
        }
    }

    fn handle_keyboard_event(&mut self, key: &tuirealm::event::KeyEvent) -> Option<Msg> {
        match &mut self.active_dialog {
            ActiveDialog::None => {}
            ActiveDialog::Help => {
                if matches!(key.code, Key::Esc | Key::Char('?')) {
                    self.active_dialog = ActiveDialog::None;
                    return Some(Msg::Redraw);
                }
                return None;
            }
            ActiveDialog::ModelSelector {
                providers,
                provider_idx,
                models,
                selected_idx,
                is_loading,
            } => match (key.modifiers, &key.code) {
                (KeyModifiers::NONE, Key::Up) => {
                    if let Some(idx) = selected_idx
                        && *idx > 0
                    {
                        *selected_idx = Some(*idx - 1);
                    }
                    return Some(Msg::Redraw);
                }
                (KeyModifiers::NONE, Key::Down) => {
                    if let Some(idx) = selected_idx
                        && *idx + 1 < models.len()
                    {
                        *selected_idx = Some(*idx + 1);
                    }
                    return Some(Msg::Redraw);
                }
                (KeyModifiers::NONE, Key::Left) => {
                    if *provider_idx > 0 {
                        *provider_idx -= 1;
                        *models = Vec::new();
                        *selected_idx = None;
                        *is_loading = true;
                        let provider_name =
                            providers.get(*provider_idx).cloned().unwrap_or_default();
                        return Some(Msg::FetchModels(provider_name));
                    }
                    return Some(Msg::Redraw);
                }
                (KeyModifiers::NONE, Key::Right) => {
                    if *provider_idx + 1 < providers.len() {
                        *provider_idx += 1;
                        *models = Vec::new();
                        *selected_idx = None;
                        *is_loading = true;
                        let provider_name =
                            providers.get(*provider_idx).cloned().unwrap_or_default();
                        return Some(Msg::FetchModels(provider_name));
                    }
                    return Some(Msg::Redraw);
                }
                (KeyModifiers::NONE, Key::Enter) => {
                    if let Some(idx) = selected_idx
                        && let Some(model) = models.get(*idx)
                    {
                        let model = model.clone();
                        let provider_name =
                            providers.get(*provider_idx).cloned().unwrap_or_default();
                        self.current_model.clone_from(&model);
                        self.active_dialog = ActiveDialog::None;
                        return Some(Msg::SwitchProviderAndModel(provider_name, model));
                    }
                    return Some(Msg::Redraw);
                }
                (KeyModifiers::NONE, Key::Esc) => {
                    self.active_dialog = ActiveDialog::None;
                    return Some(Msg::Redraw);
                }
                _ => return None,
            },
        }

        match (key.modifiers, &key.code) {
            (KeyModifiers::NONE, Key::PageUp) => {
                self.scroll_up(20);
                Some(Msg::ScrollUp(20))
            }
            (KeyModifiers::NONE, Key::PageDown) => {
                self.scroll_down(20);
                Some(Msg::ScrollDown(20))
            }
            _ => Some(Msg::KeyboardToInput(*key)),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::session::Role;

    #[test]
    fn new_chat_has_welcome_message_first() {
        let messages = vec![ChatMessage::system("Welcome to pie! Type ? for help.")];
        let chat = ChatComponent::new(messages, "test-model".to_string());
        assert_eq!(chat.messages.len(), 1);
        assert_eq!(chat.messages[0].role, Role::System);
        assert!(chat.messages[0].content.contains("Welcome"));
    }

    #[test]
    fn add_message_auto_scrolls() {
        let mut chat = ChatComponent::new(
            vec![ChatMessage::system("Welcome")],
            "test-model".to_string(),
        );
        chat.chat_state.auto_scroll = false;
        chat.add_message(ChatMessage::user("test"));
        assert!(
            chat.chat_state.auto_scroll,
            "add_message should enable auto_scroll"
        );
    }

    #[test]
    fn start_and_finish_stream() {
        let mut chat = ChatComponent::new(vec![], "test-model".to_string());
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
