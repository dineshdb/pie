//! ChatComponent — tuirealm component for the chat message display.
//!
//! Owns the message list, render cache, scroll state, and streaming response tracking.

use crate::ui::tui::realm::{Msg, StreamEvent};
use crate::ui::tui::state::ChatMessage;
use crate::ui::tui::widgets::chat::{self, ChatState, ChatView, truncate_str};
use crate::ui::tui::widgets::render_cache::MessageRenderCache;
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::{Event, Key, KeyModifiers};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::Frame;
use tuirealm::state::State;

const MAX_MESSAGES: usize = 1_000;

pub struct ChatComponent {
    pub messages: Vec<ChatMessage>,
    pub render_cache: MessageRenderCache,
    pub chat_state: ChatState,
    pub response_idx: Option<usize>,
    pub show_help: bool,
}

impl ChatComponent {
    pub fn new(messages: Vec<ChatMessage>) -> Self {
        Self {
            messages,
            render_cache: MessageRenderCache::new(),
            chat_state: ChatState::new(),
            response_idx: None,
            show_help: false,
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
        if self.show_help {
            frame.render_widget(super::super::widgets::help::HelpOverlay, area);
        } else {
            let lines = chat::build_chat_lines(
                &self.messages,
                &mut self.render_cache,
                area.width as usize,
            );
            frame.render_stateful_widget(
                ChatView { lines: &lines },
                area,
                &mut self.chat_state,
            );
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
            // Stream events: handle directly
            Event::User(StreamEvent::Delta(s)) => {
                self.update_response(s.clone());
                None
            }
            Event::User(StreamEvent::Done(s)) => {
                self.finish_stream(s.clone());
                Some(Msg::StreamDone(s.clone()))
            }
            Event::User(StreamEvent::Error(s)) => {
                self.stream_error(s);
                Some(Msg::StreamError(s.clone()))
            }
            Event::User(StreamEvent::ToolCall { display, output }) => {
                let truncated = truncate_str(output, 120);
                let content = if truncated.is_empty() {
                    display.clone()
                } else {
                    format!("{display} → {truncated}")
                };
                self.add_message(ChatMessage::tool(&content));
                None
            }
            // Keyboard: intercept scroll keys, delegate everything else to Input
            Event::Keyboard(key) => match (key.modifiers, &key.code) {
                (KeyModifiers::NONE, Key::PageUp) => {
                    self.scroll_up(20);
                    Some(Msg::ScrollUp(20))
                }
                (KeyModifiers::NONE, Key::PageDown) => {
                    self.scroll_down(20);
                    Some(Msg::ScrollDown(20))
                }
                _ => Some(Msg::KeyboardToInput(*key)),
            },
            // Tick and everything else: ignore
            _ => None,
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
        let chat = ChatComponent::new(messages);
        assert_eq!(chat.messages.len(), 1);
        assert_eq!(chat.messages[0].role, Role::System);
        assert!(chat.messages[0].content.contains("Welcome"));
    }

    #[test]
    fn add_message_auto_scrolls() {
        let mut chat = ChatComponent::new(vec![ChatMessage::system("Welcome")]);
        chat.chat_state.auto_scroll = false;
        chat.add_message(ChatMessage::user("test"));
        assert!(chat.chat_state.auto_scroll, "add_message should enable auto_scroll");
    }

    #[test]
    fn start_and_finish_stream() {
        let mut chat = ChatComponent::new(vec![]);
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
