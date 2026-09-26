//! `ChatComponent` — tuirealm component for the chat message display.
//!
//! Owns the message list, render cache, scroll state, and streaming response tracking.

use crate::client::CatalogEntry;
use crate::realm::{AskId, Msg, StreamEvent};
use crate::state::ChatMessage;
use crate::theme;
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
    /// The `/theme` picker over the built-in palettes.
    ThemeSelector(ThemeSelectorState),
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

/// The `/theme` picker's state — the palettes are statics, so only the
/// navigation and the active name are state.
#[derive(Debug, Clone, PartialEq)]
pub struct ThemeSelectorState {
    pub current_name: &'static str,
    pub selected_idx: usize,
}

impl ThemeSelectorState {
    /// Open on the active theme's row: Enter without navigating
    /// re-confirms what is already selected instead of silently
    /// jumping to the first row.
    pub fn for_current_theme() -> Self {
        Self {
            current_name: theme::current().name,
            selected_idx: theme::current_index(),
        }
    }
}

pub struct ChatComponent {
    pub messages: Vec<ChatMessage>,
    pub render_cache: MessageRenderCache,
    pub chat_state: ChatState,
    /// The in-flight text segment deltas append to — `None` whenever the
    /// segment is closed (a tool call arrived, the turn settled).
    pub response_idx: Option<usize>,
    /// A turn is in flight — set on submit, cleared on settle. Deliberately
    /// wider than `response_idx`, which legitimately goes `None` between
    /// segments mid-turn.
    pub streaming: bool,
    /// This turn streamed at least one delta — the terminal frame's full
    /// answer is then already on screen and must not be re-materialized.
    pub streamed_this_turn: bool,
    pub active_dialog: ActiveDialog,
    pub last_area: Rect,
    pub render_plan: Vec<chat::ChatRenderItem>,
    pub total_height: usize,
    pub last_width: usize,
    pub registry: Arc<Registry>,
    /// Yolo mode: permission asks are auto-allowed instead of
    /// prompting. Opt-in (`--yolo`, `/yolo`), never persisted; every
    /// auto-answer is audited into the transcript as a system message.
    pub yolo: bool,
}

impl ChatComponent {
    pub fn new(messages: Vec<ChatMessage>, registry: Arc<Registry>) -> Self {
        Self {
            messages,
            render_cache: MessageRenderCache::new(),
            chat_state: ChatState::new(),
            response_idx: None,
            streaming: false,
            streamed_this_turn: false,
            active_dialog: ActiveDialog::None,
            last_area: Rect::default(),
            render_plan: Vec::new(),
            total_height: 0,
            last_width: 0,
            registry,
            yolo: false,
        }
    }

    /// Start with yolo mode on (`--yolo`).
    pub fn with_yolo(mut self, yolo: bool) -> Self {
        self.yolo = yolo;
        self
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
        self.close_response_segment();
        self.add_message(ChatMessage::response());
        self.response_idx = Some(self.messages.len() - 1);
        self.streaming = true;
        self.streamed_this_turn = false;
        self.render_plan.clear();
    }

    /// A tool call (or turn end) arrived mid-stream: the text streamed so
    /// far is what preceded it — freeze that segment in place so the next
    /// delta opens a fresh one *after* whatever comes next. An untouched
    /// segment is dropped rather than left as dead weight.
    fn close_response_segment(&mut self) {
        let Some(idx) = self.response_idx else {
            return;
        };
        self.response_idx = None;
        if self.messages.get(idx).is_some_and(|m| m.content.is_empty()) {
            self.messages.remove(idx);
            self.render_cache.remove(idx);
        } else if let Some(msg) = self.messages.get_mut(idx) {
            msg.finalize_response();
            self.render_cache.invalidate(idx);
        }
    }

    /// Reset the per-turn streaming state once a turn settles.
    fn settle_turn(&mut self) {
        self.response_idx = None;
        self.streaming = false;
        self.streamed_this_turn = false;
        self.render_plan.clear();
    }

    pub fn update_response(&mut self, delta: &str) {
        if self.response_idx.is_none() {
            // Text resuming after a tool call — a fresh segment, so it
            // renders after the tool line it followed.
            self.add_message(ChatMessage::response());
            self.response_idx = Some(self.messages.len() - 1);
        }
        if let Some(idx) = self.response_idx
            && let Some(msg) = self.messages.get_mut(idx)
        {
            msg.content.push_str(delta);
            self.streamed_this_turn = true;
            self.chat_state.scroll_to_bottom();
            self.render_plan.clear();
        }
    }

    pub fn finish_stream(&mut self, output: String) {
        match self.response_idx {
            Some(idx) => {
                if let Some(msg) = self.messages.get_mut(idx) {
                    // The terminal frame carries the whole answer; deltas
                    // already painted it (possibly across segments), so it
                    // only materializes when nothing streamed.
                    if !self.streamed_this_turn {
                        msg.set_content(output);
                    }
                    msg.finalize_response();
                    self.render_cache.invalidate(idx);
                }
            }
            None if !self.streamed_this_turn && !output.is_empty() => {
                // The answer arrived only on the final frame, after tool
                // calls closed every segment — it lands as its own message.
                let mut msg = ChatMessage::response();
                msg.set_content(output);
                msg.finalize_response();
                self.add_message(msg);
            }
            None => {}
        }
        self.settle_turn();
    }

    pub fn stream_error(&mut self, err: &str) {
        let line = format!("Error: {err}");
        if !self.streamed_this_turn {
            self.finish_stream(line);
            return;
        }
        // Deltas already painted this turn's text — the failure appends
        // after it instead of clobbering streamed content.
        self.close_response_segment();
        self.add_message(ChatMessage::assistant(&line));
        self.settle_turn();
    }

    pub fn is_streaming(&self) -> bool {
        self.streaming
    }

    fn get_help_total_lines(registry: &Registry) -> u16 {
        let mut total = 15; // Commands (9) + Keys (6)
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
                theme::current(),
            );
            self.render_plan = plan;
            self.total_height = height;
            self.last_width = area.width as usize;
        }
        self.last_area = area;
        let theme = theme::current();

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
                            theme,
                        },
                        theme,
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
                let para = tuirealm::ratatui::widgets::Paragraph::new(body)
                    .style(tuirealm::ratatui::style::Style::default().fg(theme.warning));
                frame.render_widget(
                    super::super::widgets::dialog::Dialog::new(
                        " Permission Required ",
                        para,
                        theme,
                    )
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
                            theme,
                        },
                        theme,
                    )
                    .with_size(60, 40),
                    area,
                );
            }
            ActiveDialog::ThemeSelector(state) => {
                frame.render_widget(
                    super::super::widgets::dialog::Dialog::new(
                        " Theme (Enter select · Esc close) ",
                        super::super::widgets::theme_selector::ThemeSelectorOverlay {
                            current_name: state.current_name,
                            selected_idx: state.selected_idx,
                            theme,
                        },
                        theme,
                    )
                    .with_size(40, 40),
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
                    // The call announces itself here: anything streamed so
                    // far happened *before* it.
                    self.close_response_segment();
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
                // Yolo answers allow on the spot — the dialog never
                // opens and nothing is printed: the mode bar's yolo tag
                // is the standing disclosure of what is happening.
                if self.yolo {
                    return Msg::AnswerPermission(id.clone(), true);
                }
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
            StreamEvent::Warm | StreamEvent::Selection | StreamEvent::Usage => Msg::Redraw,
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
            ActiveDialog::ThemeSelector(state) => {
                Some(ChatComponent::theme_selector_key(key, state))
            }
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
            // Same for the theme picker — the confirmed name rides
            // `Msg::SelectTheme` out.
            if let Key::Enter | Key::Esc = key.code
                && matches!(self.active_dialog, ActiveDialog::ThemeSelector(_))
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

    /// The `/theme` picker's keys, mirroring the model picker — Enter
    /// confirms the navigated palette (the realm loop applies it).
    fn theme_selector_key(key: &tuirealm::event::KeyEvent, state: &mut ThemeSelectorState) -> Msg {
        let last = theme::THEMES.len().saturating_sub(1);
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
                let Some(selected) = theme::THEMES.get(state.selected_idx) else {
                    return Msg::Redraw;
                };
                Msg::SelectTheme(selected.name)
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
            // The realm loop routes the answer through the A2A client.
            return Msg::AnswerPermission(id, allow);
        }
        Msg::Redraw
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pie_core::session::Role;

    fn key(code: Key) -> tuirealm::event::KeyEvent {
        tuirealm::event::KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn theme_picker_navigates_and_confirms() {
        let mut state = ThemeSelectorState {
            current_name: theme::DARK.name,
            selected_idx: 0,
        };

        assert_eq!(
            ChatComponent::theme_selector_key(&key(Key::Down), &mut state),
            Msg::Redraw
        );
        assert_eq!(state.selected_idx, 1);
        assert_eq!(
            ChatComponent::theme_selector_key(&key(Key::Up), &mut state),
            Msg::Redraw
        );
        assert_eq!(state.selected_idx, 0);

        // Enter confirms the navigated palette as a SelectTheme message.
        assert_eq!(
            ChatComponent::theme_selector_key(&key(Key::Down), &mut state),
            Msg::Redraw
        );
        assert_eq!(
            ChatComponent::theme_selector_key(&key(Key::Enter), &mut state),
            Msg::SelectTheme(theme::LIGHT.name)
        );
    }

    #[test]
    fn theme_picker_navigation_clamps_at_the_ends() {
        let mut state = ThemeSelectorState {
            current_name: theme::DARK.name,
            selected_idx: 0,
        };
        ChatComponent::theme_selector_key(&key(Key::Up), &mut state);
        assert_eq!(state.selected_idx, 0, "Up at the top must not wrap");

        state.selected_idx = theme::THEMES.len() - 1;
        ChatComponent::theme_selector_key(&key(Key::Down), &mut state);
        assert_eq!(
            state.selected_idx,
            theme::THEMES.len() - 1,
            "Down at the bottom must not wrap"
        );
    }

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

    fn permission_ask() -> StreamEvent {
        StreamEvent::PermissionAsk {
            id: AskId("t-1".to_string()),
            skill: "Bash".to_string(),
            permissions: vec!["network".to_string()],
        }
    }

    /// Yolo answers allow on the spot: the reply rides out as
    /// `Msg::AnswerPermission(…, true)`, no dialog opens, and the
    /// transcript records the auto-approval.
    #[tokio::test]
    async fn yolo_auto_answers_permission_asks() {
        let mut chat = ChatComponent::new(vec![], test_registry()).with_yolo(true);
        let msg = chat.handle_user_event(&permission_ask());

        assert_eq!(msg, Msg::AnswerPermission(AskId("t-1".to_string()), true));
        assert_eq!(
            chat.active_dialog,
            ActiveDialog::None,
            "yolo must not open the permission dialog"
        );
        assert!(
            chat.messages.is_empty(),
            "auto-approvals must not add transcript lines, got {:?}",
            chat.messages
        );
    }

    /// The control: without yolo the same ask parks in the dialog.
    #[tokio::test]
    async fn without_yolo_permission_asks_prompt() {
        let mut chat = ChatComponent::new(vec![], test_registry());
        let msg = chat.handle_user_event(&permission_ask());

        assert_eq!(msg, Msg::Redraw);
        match chat.active_dialog {
            ActiveDialog::PermissionPrompt(state) => {
                assert_eq!(state.id, AskId("t-1".to_string()));
                assert_eq!(state.skill, "Bash");
            }
            other => panic!("expected the permission dialog, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn with_yolo_builder_sets_the_flag() {
        let chat = ChatComponent::new(vec![], test_registry()).with_yolo(true);
        assert!(chat.yolo);
        assert!(!ChatComponent::new(vec![], test_registry()).yolo);
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

        chat.finish_stream("Hello".to_string());
        assert!(!chat.is_streaming());
        assert_eq!(chat.response_idx, None);
        assert!(!chat.messages[0].is_response());
        assert_eq!(chat.messages[0].content, "Hello");
    }

    /// The terminal frame's full answer only materializes when no delta
    /// ever streamed — streamed text is already on screen and must not be
    /// overwritten (possibly across several segments).
    #[tokio::test]
    async fn unstreamed_answer_materializes_on_finish() {
        let mut chat = ChatComponent::new(vec![], test_registry());
        chat.start_response();
        chat.finish_stream("the whole answer".to_string());
        assert_eq!(chat.messages[0].content, "the whole answer");
        assert!(!chat.messages[0].is_response());
    }

    /// Text, a tool call, and more text must read in arrival order: the
    /// tool call splits the response into segments instead of hoisting
    /// itself above one growing lump of text.
    #[tokio::test]
    async fn text_and_tool_calls_interleave_in_arrival_order() {
        let mut chat = ChatComponent::new(vec![ChatMessage::user("run tool")], test_registry());
        chat.start_response(); // idx 1
        chat.update_response("first, a tool: ");

        chat.handle_user_event(&StreamEvent::ToolCall {
            id: "call-1".to_string(),
            name: "Bash".to_string(),
            display: "Bash{command = ls}".to_string(),
            output: String::new(),
            failed: false,
        });

        chat.update_response("done. ");
        chat.finish_stream("done. that is all".to_string());

        let roles: Vec<_> = chat.messages.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            vec![Role::User, Role::Assistant, Role::Tool, Role::Assistant],
            "transcript must interleave in arrival order"
        );
        assert_eq!(chat.messages[1].content, "first, a tool: ");
        assert_eq!(chat.messages[2].content, "Bash{command = ls}");
        assert_eq!(chat.messages[3].content, "done. ");
        assert!(!chat.messages[3].is_response(), "turn settled");
        assert!(!chat.is_streaming());
    }

    /// A turn that opens with a tool call must not leave an empty response
    /// segment behind — the text had not started yet.
    #[tokio::test]
    async fn tool_call_before_any_text_leaves_no_empty_segment() {
        let mut chat = ChatComponent::new(vec![ChatMessage::user("go")], test_registry());
        chat.start_response(); // idx 1, still empty

        chat.handle_user_event(&StreamEvent::ToolCall {
            id: "call-1".to_string(),
            name: "Glob".to_string(),
            display: "Glob{pattern = **/*.rs}".to_string(),
            output: String::new(),
            failed: false,
        });

        assert_eq!(chat.messages.len(), 2, "empty segment dropped");
        assert_eq!(chat.messages[1].role, Role::Tool);

        // Text after the tool call opens a fresh segment after it.
        chat.update_response("found them");
        assert_eq!(chat.messages.len(), 3);
        assert_eq!(chat.messages[2].role, Role::Assistant);
        assert_eq!(chat.messages[2].content, "found them");
    }

    /// A failed turn after streamed text appends the failure instead of
    /// clobbering what already rendered.
    #[tokio::test]
    async fn stream_error_after_streamed_text_appends() {
        let mut chat = ChatComponent::new(vec![], test_registry());
        chat.start_response();
        chat.update_response("partial answer");
        chat.stream_error("boom");

        assert_eq!(chat.messages.len(), 2);
        assert_eq!(chat.messages[0].content, "partial answer");
        assert_eq!(chat.messages[1].content, "Error: boom");
        assert!(!chat.is_streaming());
    }

    /// The answer arriving only on the final frame, after tool calls
    /// closed every segment, still lands — as its own message.
    #[tokio::test]
    async fn final_answer_after_tool_calls_lands_its_own_message() {
        let mut chat = ChatComponent::new(vec![ChatMessage::user("go")], test_registry());
        chat.start_response();
        chat.handle_user_event(&StreamEvent::ToolCall {
            id: "call-1".to_string(),
            name: "Read".to_string(),
            display: "Read{path = a.rs}".to_string(),
            output: String::new(),
            failed: false,
        });

        chat.finish_stream("the whole answer".to_string());
        assert_eq!(chat.messages.len(), 3);
        assert_eq!(chat.messages[2].content, "the whole answer");
        assert_eq!(chat.messages[2].role, Role::Assistant);
        assert!(!chat.is_streaming());
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
