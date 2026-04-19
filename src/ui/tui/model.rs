use crate::config::pie_home;
use crate::providers::Model;
use crate::session::{Role, Session};
use crate::ui::tui::command::{self, Command, CommandAction};
use crate::ui::tui::event::{AppEvent, HandleResult};
use crate::ui::tui::state::{ChatMessage, StreamState};
use crate::ui::tui::widgets::chat::{self, ChatState, ChatView};
use crate::ui::tui::widgets::completion::{CompletionPopup, CompletionState, Direction};
use crate::ui::tui::widgets::history::InputHistory;
use crate::ui::tui::widgets::input::{InputView, cursor_position};
use crate::ui::tui::widgets::render_cache::MessageRenderCache;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui_textarea::TextArea;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tachyonfx::{CellFilter, EffectManager, fx};
use tokio::sync::mpsc;

const MAX_MESSAGES: usize = 1_000;
const PLACEHOLDER_MESSAGES: &[&str] = &[
    "Type a query or /help for commands",
    "Use /list-skills to see available skills",
    "Ctrl+Enter for new line, Tab to complete",
    "/clear to reset, Esc to cancel streaming",
];

pub struct AppModel {
    pub messages: Vec<ChatMessage>,
    pub render_cache: MessageRenderCache,
    pub chat_state: ChatState,
    pub textarea: TextArea<'static>,
    pub history: InputHistory,
    pub completion: CompletionState,
    pub current_hint: String,
    pub placeholder_index: usize,
    pub placeholder_changed: Instant,
    pub show_help: bool,
    pub should_quit: bool,
    pub stream_state: StreamState,
    pub stream_abort: Option<mpsc::UnboundedSender<()>>,
    pub effects: EffectManager<&'static str>,
    pub last_frame: Instant,
    stream_effect_active: bool,
    /// Index of the current streaming response message (if any).
    response_idx: Option<usize>,
    pub model: Model,
    pub session_id: uuid::Uuid,
    pub session_pool: Arc<crate::db::DbPool>,
    pub sandbox_settings: PathBuf,
}

impl AppModel {
    pub fn new(model: Model, session: &Session, sandbox_settings: PathBuf) -> Self {
        let session_id = session.id;
        let session_pool = session.pool().clone();

        let history_dir = pie_home().join("history");
        let _ = std::fs::create_dir_all(&history_dir);
        let history_path = history_dir.join(format!("{session_id}.txt"));
        let history = InputHistory::new(history_path);

        let textarea = {
            let mut ta = TextArea::default();
            apply_textarea_style(&mut ta);
            ta
        };

        let mut messages = vec![ChatMessage::system("Welcome to pie! Type ? for help.")];
        for entry in session.history_entries() {
            let msg = match entry.role {
                Role::User => ChatMessage::user(&entry.content),
                Role::Assistant => ChatMessage::assistant(&entry.content),
                Role::System => ChatMessage::system(&entry.content),
                Role::Tool => ChatMessage::tool(&entry.content),
            };
            messages.push(msg);
        }

        Self {
            render_cache: MessageRenderCache::new(),
            chat_state: ChatState::new(),
            messages,
            textarea,
            history,
            completion: CompletionState::new(command::build_all_completions()),
            current_hint: String::new(),
            placeholder_index: 0,
            placeholder_changed: Instant::now(),
            show_help: false,
            should_quit: false,
            stream_state: StreamState::Idle,
            stream_abort: None,
            effects: EffectManager::default(),
            last_frame: Instant::now(),
            stream_effect_active: false,
            response_idx: None,
            model,
            session_id,
            session_pool,
            sandbox_settings,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> HandleResult {
        if let (KeyModifiers::CONTROL, KeyCode::Char('c')) = (key.modifiers, key.code) {
            if self.is_streaming() {
                self.take_abort_handle();
                return HandleResult::Continue;
            }
            self.should_quit = true;
            return HandleResult::Quit;
        }

        if matches!(key.code, KeyCode::Enter)
            && !matches!(key.modifiers, KeyModifiers::CONTROL)
            && !self.is_streaming()
        {
            return HandleResult::Submit;
        }

        if matches!(key.code, KeyCode::Char('?')) && self.is_input_empty() {
            self.show_help = true;
            return HandleResult::Continue;
        }

        if self.show_help {
            self.show_help = false;
        }

        self.handle_editing(key);
        HandleResult::Continue
    }

    pub fn handle_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::StreamDelta(delta) => self.update_response(|msg| msg.set_content(delta)),
            AppEvent::StreamDone(output) => self.finish_stream(output),
            AppEvent::StreamError(err) => self.stream_error(&err),
            AppEvent::ToolCall { display, output } => {
                let truncated = truncate_str(&output, 120);
                let content = if truncated.is_empty() {
                    display
                } else {
                    format!("{display} → {truncated}")
                };
                self.add_message(ChatMessage::tool(&content));
            }
            AppEvent::ScrollUp => self.chat_state.scroll_up(3),
            AppEvent::ScrollDown => self.chat_state.scroll_down(3),
            AppEvent::Key(_) | AppEvent::Resize => {}
        }
    }

    // ── Streaming lifecycle ──────────────────────────────────────────

    fn update_response(&mut self, f: impl FnOnce(&mut ChatMessage)) {
        if let Some(idx) = self.response_idx
            && let Some(msg) = self.messages.get_mut(idx)
        {
            f(msg);
            self.chat_state.scroll_to_bottom();
        }
    }

    fn finish_stream(&mut self, output: String) {
        self.stream_state = StreamState::Idle;
        self.stream_abort = None;
        self.update_response(|msg| msg.set_content(output));
    }

    fn stream_error(&mut self, err: &str) {
        self.finish_stream(format!("Error: {err}"));
    }

    pub fn start_stream(&mut self, query: &str, tx: &mpsc::UnboundedSender<AppEvent>) {
        self.stream_state = StreamState::Active;
        self.add_message(ChatMessage::response());
        self.response_idx = Some(self.messages.len() - 1);

        let (abort_tx, abort_rx) = mpsc::unbounded_channel::<()>();
        self.stream_abort = Some(abort_tx);

        super::stream::spawn_stream(
            query.to_string(),
            self.model.clone(),
            self.sandbox_settings.clone(),
            self.session_id,
            self.session_pool.clone(),
            tx.clone(),
            abort_rx,
        );
    }

    pub fn is_streaming(&self) -> bool {
        matches!(self.stream_state, StreamState::Active)
    }

    pub fn take_abort_handle(&mut self) -> Option<mpsc::UnboundedSender<()>> {
        self.stream_abort.take()
    }

    // ── Message management ───────────────────────────────────────────

    pub fn add_message(&mut self, msg: ChatMessage) {
        if self.messages.len() >= MAX_MESSAGES {
            self.messages.remove(0);
            self.render_cache.trim_front(1);
            // Adjust response index for the shift
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

    // ── Input ────────────────────────────────────────────────────────

    pub fn submit_input(&mut self, tx: &mpsc::UnboundedSender<AppEvent>) {
        let text = self.take_input();
        if text.trim().is_empty() {
            return;
        }

        let cmd = Command::parse(&text);
        let action = Self::dispatch_command(cmd);
        match action {
            CommandAction::AddMessage(msg) => {
                self.add_message(ChatMessage::user(&text));
                self.add_message(msg);
            }
            CommandAction::ClearMessages => {
                self.clear_messages();
            }
            CommandAction::Stream(query) => {
                self.add_message(ChatMessage::user(&query));
                self.start_stream(&query, tx);
            }
            CommandAction::Quit => {
                self.should_quit = true;
            }
        }
    }

    fn dispatch_command(cmd: Command) -> CommandAction {
        match cmd {
            Command::Quit => CommandAction::Quit,
            Command::Help => CommandAction::AddMessage(ChatMessage::system(command::HELP_TEXT)),
            Command::ListSkills => {
                let text = build_skills_list();
                CommandAction::AddMessage(ChatMessage::system(&text))
            }
            Command::Clear => CommandAction::ClearMessages,
            Command::Send(query) => CommandAction::Stream(query),
            Command::Invoke {
                name,
                query,
                is_agent,
            } => {
                let rewritten = if is_agent {
                    format!("Use the subagent tool with agent_name=\"{name}\" to handle: {query}")
                } else if query.is_empty() {
                    format!("/{name}")
                } else {
                    format!("/{name} {query}")
                };
                CommandAction::Stream(rewritten)
            }
        }
    }

    pub fn render(&mut self, frame: &mut ratatui::Frame) {
        let area = frame.area();

        #[allow(clippy::cast_possible_truncation)]
        let input_lines = self.input_line_count().clamp(1, 8) as u16;
        let input_height = input_lines + 2;

        let chunks = ratatui::layout::Layout::default()
            .direction(ratatui::layout::Direction::Vertical)
            .constraints([
                ratatui::layout::Constraint::Min(5),
                ratatui::layout::Constraint::Length(input_height),
            ])
            .split(area);

        let messages_area = chunks.first().copied().unwrap_or(area);
        let input_area = chunks.get(1).copied().unwrap_or(area);

        if self.show_help {
            frame.render_widget(super::widgets::help::HelpOverlay, messages_area);
        } else {
            let lines = chat::build_chat_lines(
                &self.messages,
                &mut self.render_cache,
                messages_area.width as usize,
            );
            frame.render_stateful_widget(
                ChatView { lines: &lines },
                messages_area,
                &mut self.chat_state,
            );
        }

        self.tick_placeholder();
        self.render_input(frame, input_area);
        self.render_completions(frame, input_area);

        match (self.is_streaming(), self.stream_effect_active) {
            (true, false) => {
                let effect = fx::repeating(fx::hsl_shift_fg(
                    [30.0, 20.0, 10.0],
                    (1500, tachyonfx::Interpolation::SineInOut),
                ));
                let effect = effect.with_filter(CellFilter::FgColor(Color::Rgb(255, 140, 0)));
                self.effects.add_unique_effect("stream", effect);
                self.stream_effect_active = true;
            }
            (false, true) => {
                self.effects.cancel_unique_effect("stream");
                self.stream_effect_active = false;
            }
            _ => {}
        }

        let elapsed = self.last_frame.elapsed();
        self.last_frame = Instant::now();
        let border_area = Rect {
            x: input_area.x,
            y: input_area.y,
            width: input_area.width,
            height: 1,
        };
        self.effects
            .process_effects(elapsed.into(), frame.buffer_mut(), border_area);
    }

    fn render_input(&self, frame: &mut ratatui::Frame, area: Rect) {
        let is_empty = self.is_input_empty();
        let text = self.input_text();
        let text_lines: Vec<String> = if is_empty {
            vec![String::new()]
        } else {
            text.split('\n').map(ToString::to_string).collect()
        };
        let cursor = self.textarea.cursor();
        let placeholder = PLACEHOLDER_MESSAGES
            .get(self.placeholder_index)
            .copied()
            .unwrap_or("");

        let input_view = InputView {
            text_lines,
            cursor_row: cursor.0,
            placeholder,
            hint: &self.current_hint,
            is_empty,
            is_streaming: self.is_streaming(),
        };
        frame.render_widget(input_view, area);

        #[allow(clippy::cast_possible_truncation)]
        let visible_rows = area.height.saturating_sub(2) as usize;
        if cursor.0 < visible_rows {
            let (cx, cy) = cursor_position(area, cursor.0, cursor.1);
            frame.set_cursor_position((cx, cy));
        }
    }

    fn render_completions(&self, frame: &mut ratatui::Frame, input_area: Rect) {
        let candidates = self.completion.candidates();
        if candidates.is_empty() {
            return;
        }

        let popup = CompletionPopup {
            candidates,
            selected: self.completion.index(),
        };
        let popup_area = popup.popup_area(input_area);

        frame.render_widget(ratatui::widgets::Clear, popup_area);
        frame.render_widget(popup, popup_area);
    }

    // ── Textarea helpers ─────────────────────────────────────────────

    pub fn input_text(&self) -> String {
        self.textarea.lines().join("\n")
    }

    pub fn is_input_empty(&self) -> bool {
        self.textarea.lines().iter().all(String::is_empty)
    }

    pub fn input_line_count(&self) -> usize {
        self.textarea.lines().len().max(1)
    }

    pub fn cursor_is_at_first_line_start(&self) -> bool {
        let lines = self.textarea.lines();
        let cursor = self.textarea.cursor();
        lines.len() == 1 && lines.first().is_some_and(String::is_empty)
            || (cursor.0, cursor.1) == (0, 0)
    }

    pub fn cursor_is_at_end(&self) -> bool {
        let lines = self.textarea.lines();
        let (row, col) = (self.textarea.cursor().0, self.textarea.cursor().1);
        let last_row = lines.len() - 1;
        let last_col = lines.last().map_or(0, String::len);
        row >= last_row && col >= last_col
    }

    pub fn input_key(&mut self, key: KeyEvent) -> bool {
        let handled = self.textarea.input(key);
        self.completion.update(&self.current_line());
        self.update_hint();
        handled
    }

    pub fn insert_char(&mut self, c: char) {
        self.textarea.insert_char(c);
        self.completion.update(&self.current_line());
        self.update_hint();
    }

    pub fn take_input(&mut self) -> String {
        let text = self.input_text();
        self.history.append(&text);
        self.current_hint.clear();
        self.completion.reset();
        let mut empty = TextArea::default();
        apply_textarea_style(&mut empty);
        self.textarea = empty;
        text
    }

    pub fn set_input_text(&mut self, text: &str) {
        let lines: Vec<String> = text.lines().map(String::from).collect();
        let mut ta = TextArea::new(lines);
        apply_textarea_style(&mut ta);
        self.textarea = ta;
        self.completion.reset();
        self.current_hint.clear();
    }

    pub fn tick_placeholder(&mut self) {
        if self.is_input_empty() && self.placeholder_changed.elapsed() >= Duration::from_secs(2) {
            self.placeholder_index = (self.placeholder_index + 1) % PLACEHOLDER_MESSAGES.len();
            self.placeholder_changed = Instant::now();
        }
    }

    pub fn completions_active(&self) -> bool {
        self.completion.is_active()
    }

    pub fn tab_complete(&mut self) {
        if !self.completion.is_active() {
            return;
        }
        self.completion.move_selection(Direction::Next);
        self.accept_completion();
    }

    pub fn completion_prev(&mut self) {
        self.completion.move_selection(Direction::Prev);
    }

    pub fn completion_next(&mut self) {
        self.completion.move_selection(Direction::Next);
    }

    pub fn accept_completion(&mut self) {
        if let Some(completion) = self.completion.selected().map(ToString::to_string) {
            self.apply_completion(&completion);
        }
    }

    pub fn dismiss_completions(&mut self) {
        self.completion.reset();
    }

    pub fn accept_hint(&mut self) {
        if self.current_hint.is_empty() {
            return;
        }
        for c in self.current_hint.chars() {
            self.textarea.insert_char(c);
        }
        self.current_hint.clear();
    }

    pub fn has_hint(&self) -> bool {
        !self.current_hint.is_empty()
    }

    pub fn history_prev(&mut self) {
        if let Some(text) = self.history.prev() {
            self.set_input_text(&text);
        }
    }

    pub fn history_next(&mut self) {
        if let Some(text) = self.history.next() {
            self.set_input_text(&text);
        }
    }

    fn handle_editing(&mut self, key: KeyEvent) {
        if self.completions_active() {
            match (key.modifiers, key.code) {
                (KeyModifiers::NONE, KeyCode::Up) => {
                    self.completion_prev();
                    return;
                }
                (KeyModifiers::NONE, KeyCode::Down) => {
                    self.completion_next();
                    return;
                }
                (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Enter)
                | (KeyModifiers::NONE, KeyCode::Tab) => {
                    self.accept_completion();
                    return;
                }
                (KeyModifiers::NONE, KeyCode::Esc) => {
                    self.dismiss_completions();
                    return;
                }
                _ => {}
            }
        }

        match (key.modifiers, key.code) {
            (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Enter) => {}
            (KeyModifiers::CONTROL, KeyCode::Enter) => {
                self.insert_char('\n');
            }
            (KeyModifiers::NONE, KeyCode::Tab) => {
                self.tab_complete();
            }
            (KeyModifiers::NONE, KeyCode::Up) if self.cursor_is_at_first_line_start() => {
                self.history_prev();
            }
            (KeyModifiers::NONE, KeyCode::Down) if self.cursor_is_at_end() => {
                self.history_next();
            }
            (KeyModifiers::NONE, KeyCode::Up) => {
                self.chat_state.scroll_up(1);
            }
            (KeyModifiers::NONE, KeyCode::Down) => {
                self.chat_state.scroll_down(1);
            }
            (KeyModifiers::NONE, KeyCode::PageUp) => {
                self.chat_state.scroll_up(20);
            }
            (KeyModifiers::NONE, KeyCode::PageDown) => {
                self.chat_state.scroll_down(20);
            }
            (KeyModifiers::NONE, KeyCode::Right) if self.cursor_is_at_end() && self.has_hint() => {
                self.accept_hint();
            }
            (KeyModifiers::NONE, KeyCode::Esc) if self.is_streaming() => {
                self.take_abort_handle();
            }
            _ => {
                self.input_key(key);
            }
        }
    }

    fn apply_completion(&mut self, completion: &str) {
        let row = self.textarea.cursor().0;
        let mut lines: Vec<String> = self.textarea.lines().iter().map(String::from).collect();
        if let Some(line) = lines.get_mut(row) {
            *line = completion.to_string();
        }
        let mut ta = TextArea::new(lines);
        apply_textarea_style(&mut ta);
        for _ in 0..completion.len() {
            ta.move_cursor(ratatui_textarea::CursorMove::End);
        }
        self.textarea = ta;
        self.current_hint.clear();
    }

    fn update_hint(&mut self) {
        self.current_hint.clear();
        let line = self.current_line();
        if !line.starts_with('/') || line.len() < 2 {
            return;
        }

        if let Some(hint) = self.history.find_hint(&line) {
            self.current_hint = hint;
            return;
        }

        if let Some(hint) = self.completion.find_hint(&line) {
            self.current_hint = hint;
        }
    }

    fn current_line(&self) -> String {
        let row = self.textarea.cursor().0;
        self.textarea.lines().get(row).cloned().unwrap_or_default()
    }
}

fn build_skills_list() -> String {
    let agents = crate::agent::get_all_agents();
    if agents.is_empty() {
        return "No agents found.".to_string();
    }
    let names: Vec<&str> = agents.iter().map(|a| a.name.as_str()).collect();
    format!("Agents: {}", names.join(", "))
}

fn apply_textarea_style(textarea: &mut TextArea<'static>) {
    textarea.set_cursor_line_style(Style::default());
    textarea.set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
    textarea.set_style(Style::default().fg(Color::White));
}

/// Collapse multiline to one line and truncate, safe on UTF-8 boundaries.
fn truncate_str(s: &str, max_len: usize) -> String {
    let single_line: String = s.lines().collect::<Vec<_>>().join(" ");
    if single_line.len() <= max_len {
        return single_line;
    }
    let end = single_line.ceil_char_boundary(max_len);
    format!("{}…", &single_line[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::session::Session;
    use std::sync::Arc;

    fn test_pool() -> Arc<db::DbPool> {
        Arc::new(db::create_test_pool().unwrap())
    }

    fn test_model() -> Model {
        Model::test_dummy().unwrap()
    }

    #[test]
    fn new_session_has_welcome_message_first() {
        let pool = test_pool();
        let session = Session::create(pool).unwrap();
        let app = AppModel::new(test_model(), &session, PathBuf::from("/tmp"));

        assert_eq!(app.messages.len(), 1, "new session should have welcome");
        assert_eq!(
            app.messages[0].role,
            Role::System,
            "first message should be system welcome"
        );
        assert!(
            app.messages[0].content.contains("Welcome"),
            "welcome message should contain 'Welcome'"
        );
    }

    #[test]
    fn restored_session_places_history_after_welcome() {
        let pool = test_pool();
        let mut session = Session::create(pool.clone()).unwrap();
        session.add_user("hello").unwrap();
        session.add_assistant("hi there").unwrap();

        let session = Session::load(pool, session.id).unwrap();
        let app = AppModel::new(test_model(), &session, PathBuf::from("/tmp"));

        assert_eq!(app.messages.len(), 3);
        assert!(
            app.messages[0].content.contains("Welcome"),
            "welcome must be first, got: {:?}",
            app.messages[0].content
        );
        assert_eq!(app.messages[1].role, Role::User);
        assert_eq!(app.messages[1].content, "hello");
        assert_eq!(app.messages[2].role, Role::Assistant);
        assert_eq!(app.messages[2].content, "hi there");
    }

    #[test]
    fn restored_session_auto_scrolls_to_bottom() {
        let pool = test_pool();
        let mut session = Session::create(pool.clone()).unwrap();
        for i in 0..20 {
            session.add_user(&format!("query {i}")).unwrap();
            session.add_assistant(&format!("answer {i}")).unwrap();
        }

        let session = Session::load(pool, session.id).unwrap();
        let app = AppModel::new(test_model(), &session, PathBuf::from("/tmp"));

        assert!(
            app.chat_state.auto_scroll,
            "restored session should auto_scroll to show latest"
        );
    }

    #[test]
    fn add_message_auto_scrolls() {
        let pool = test_pool();
        let session = Session::create(pool).unwrap();
        let mut app = AppModel::new(test_model(), &session, PathBuf::from("/tmp"));

        app.chat_state.auto_scroll = false;
        app.add_message(ChatMessage::user("test"));

        assert!(
            app.chat_state.auto_scroll,
            "add_message should enable auto_scroll"
        );
    }

    #[test]
    fn truncate_str_safe_on_multibyte() {
        // "café" is 5 bytes — 'é' is 2 bytes. Truncating at 4 should land on char boundary.
        let result = truncate_str("café coffee", 4);
        assert_eq!(result, "café…");
    }
}
