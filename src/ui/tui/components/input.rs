//! `InputComponent` — text input area with history, completion, hints, and effects.
//!
//! Not mounted in the tuirealm App — accessed directly from the main loop.
//! Only `ChatComponent` is the active tuirealm component.

use crate::config::pie_home;
use crate::providers::Model;
use crate::session::Session;
use crate::ui::tui::command;
use crate::ui::tui::realm::{Msg, StreamEvent};
use crate::ui::tui::widgets::completion::{CompletionPopup, CompletionState, Direction};
use crate::ui::tui::widgets::history::InputHistory;
use crate::ui::tui::widgets::input::{InputView, cursor_position};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tachyonfx::{CellFilter, EffectManager, fx};
use tokio::sync::mpsc;
use tui_textarea::{Input as TaInput, Key as TaKey, TextArea};
use tuirealm::event::{Key, KeyModifiers};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::style::{Color, Modifier, Style};

const PLACEHOLDER: &str = "Type a query or /help for commands";

pub struct InputComponent {
    pub textarea: TextArea<'static>,
    pub history: InputHistory,
    pub completion: CompletionState,
    pub current_hint: String,
    pub effects: EffectManager<&'static str>,
    pub last_frame: Instant,
    stream_effect_active: bool,
    pub model: Model,
    pub session_id: uuid::Uuid,
    pub session_pool: Arc<crate::db::DbPool>,
    pub sandbox_settings: PathBuf,
    pub stream_abort: Option<mpsc::UnboundedSender<()>>,
}

impl InputComponent {
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

        Self {
            textarea,
            history,
            completion: CompletionState::new(command::build_all_completions()),
            current_hint: String::new(),
            effects: EffectManager::default(),
            last_frame: Instant::now(),
            stream_effect_active: false,
            model,
            session_id,
            session_pool,
            sandbox_settings,
            stream_abort: None,
        }
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

    fn cursor_is_at_first_line_start(&self) -> bool {
        let lines = self.textarea.lines();
        let cursor = self.textarea.cursor();
        lines.len() == 1 && lines.first().is_some_and(String::is_empty)
            || (cursor.0, cursor.1) == (0, 0)
    }

    fn cursor_is_at_end(&self) -> bool {
        let lines = self.textarea.lines();
        let (row, col) = (self.textarea.cursor().0, self.textarea.cursor().1);
        let last_row = lines.len() - 1;
        let last_col = lines.last().map_or(0, String::len);
        row >= last_row && col >= last_col
    }

    fn input_key(&mut self, key: &tuirealm::event::KeyEvent) -> bool {
        let handled = self.textarea.input(TaInput::from(KeyInput(key)));
        self.completion.update(&self.current_line());
        self.update_hint();
        handled
    }

    fn insert_char(&mut self, c: char) {
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

    fn set_input_text(&mut self, text: &str) {
        let lines: Vec<String> = text.lines().map(String::from).collect();
        let mut ta = TextArea::new(lines);
        apply_textarea_style(&mut ta);
        self.textarea = ta;
        self.completion.reset();
        self.current_hint.clear();
    }

    fn completions_active(&self) -> bool {
        self.completion.is_active()
    }

    fn tab_complete(&mut self) {
        if !self.completion.is_active() {
            return;
        }
        self.completion.move_selection(Direction::Next);
        self.accept_completion();
    }

    fn completion_prev(&mut self) {
        self.completion.move_selection(Direction::Prev);
    }

    fn completion_next(&mut self) {
        self.completion.move_selection(Direction::Next);
    }

    fn accept_completion(&mut self) {
        if let Some(completion) = self.completion.selected().map(ToString::to_string) {
            self.apply_completion(&completion);
        }
    }

    pub fn dismiss_completions(&mut self) {
        self.completion.reset();
    }

    fn accept_hint(&mut self) {
        if self.current_hint.is_empty() {
            return;
        }
        for c in self.current_hint.chars() {
            self.textarea.insert_char(c);
        }
        self.current_hint.clear();
    }

    fn has_hint(&self) -> bool {
        !self.current_hint.is_empty()
    }

    fn history_prev(&mut self) {
        if let Some(text) = self.history.prev() {
            self.set_input_text(&text);
        }
    }

    fn history_next(&mut self) {
        if let Some(text) = self.history.next() {
            self.set_input_text(&text);
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
            ta.move_cursor(tui_textarea::CursorMove::End);
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

    // ── Keyboard handling ────────────────────────────────────────────

    pub fn handle_key_event(&mut self, key: &tuirealm::event::KeyEvent) -> Option<Msg> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, Key::Char('c')) {
            if self.is_streaming() {
                self.take_abort_handle();
                return None;
            }
            return Some(Msg::Quit);
        }

        if matches!(key.code, Key::Enter)
            && !key.modifiers.contains(KeyModifiers::CONTROL)
            && !self.is_streaming()
        {
            return Some(Msg::Submit(self.input_text()));
        }

        if matches!(key.code, Key::Char('?')) && self.is_input_empty() {
            return Some(Msg::ToggleHelp);
        }

        if self.completions_active() {
            match (
                &key.code,
                key.modifiers.contains(KeyModifiers::SHIFT) || key.modifiers == KeyModifiers::NONE,
            ) {
                (Key::Up, _) if key.modifiers == KeyModifiers::NONE => {
                    self.completion_prev();
                    return None;
                }
                (Key::Down, _) if key.modifiers == KeyModifiers::NONE => {
                    self.completion_next();
                    return None;
                }
                (Key::Enter, true) | (Key::Tab, _) => {
                    self.accept_completion();
                    return None;
                }
                (Key::Esc, _) if key.modifiers == KeyModifiers::NONE => {
                    self.dismiss_completions();
                    return None;
                }
                _ => {}
            }
        }

        match (&key.code, key.modifiers) {
            (Key::Enter, KeyModifiers::NONE | KeyModifiers::SHIFT) => {}
            (Key::Enter, _) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.insert_char('\n');
            }
            (Key::Tab, KeyModifiers::NONE) => {
                self.tab_complete();
            }
            (Key::Up, _) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.history_prev();
            }
            (Key::Down, _) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.history_next();
            }
            (Key::Up, _)
                if key.modifiers == KeyModifiers::NONE && self.cursor_is_at_first_line_start() =>
            {
                return Some(Msg::ScrollUp(5));
            }
            (Key::Down, _) if key.modifiers == KeyModifiers::NONE && self.cursor_is_at_end() => {
                return Some(Msg::ScrollDown(5));
            }
            (Key::Right, _)
                if key.modifiers == KeyModifiers::NONE
                    && self.cursor_is_at_end()
                    && self.has_hint() =>
            {
                self.accept_hint();
            }
            (Key::Esc, KeyModifiers::NONE) => {
                if self.is_streaming() {
                    self.take_abort_handle();
                } else {
                    return Some(Msg::CloseHelp);
                }
            }
            _ => {
                self.input_key(key);
            }
        }
        None
    }

    // ── Streaming ────────────────────────────────────────────────────

    pub fn is_streaming(&self) -> bool {
        self.stream_abort.is_some()
    }

    pub fn start_stream(&mut self, query: &str, tx: &mpsc::UnboundedSender<StreamEvent>) {
        let (abort_tx, abort_rx) = mpsc::unbounded_channel::<()>();
        self.stream_abort = Some(abort_tx);

        super::super::stream::spawn_stream(
            query.to_string(),
            self.model.clone(),
            self.sandbox_settings.clone(),
            self.session_id,
            self.session_pool.clone(),
            tx.clone(),
            abort_rx,
        );
    }

    pub fn take_abort_handle(&mut self) -> Option<mpsc::UnboundedSender<()>> {
        self.stream_abort.take()
    }

    pub fn finish_stream(&mut self) {
        self.stream_abort = None;
    }

    // ── Rendering ────────────────────────────────────────────────────

    pub fn render(&mut self, frame: &mut Frame, area: Rect, is_streaming: bool) {
        let is_empty = self.is_input_empty();
        let text_lines: Vec<String> = if is_empty {
            vec![String::new()]
        } else {
            self.textarea.lines().iter().map(String::from).collect()
        };
        let cursor = self.textarea.cursor();

        let input_view = InputView {
            text_lines,
            cursor_row: cursor.0,
            placeholder: PLACEHOLDER,
            hint: &self.current_hint,
            is_empty,
            is_streaming,
        };
        frame.render_widget(input_view, area);

        #[allow(clippy::cast_possible_truncation)]
        let visible_rows = area.height.saturating_sub(2) as usize;
        if cursor.0 < visible_rows {
            let (cx, cy) = cursor_position(area, cursor.0, cursor.1);
            frame.set_cursor_position((cx, cy));
        }

        // Streaming border effect
        match (is_streaming, self.stream_effect_active) {
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
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        };
        self.effects
            .process_effects(elapsed.into(), frame.buffer_mut(), border_area);

        // Completion popup
        let candidates = self.completion.candidates();
        if !candidates.is_empty() {
            let popup = CompletionPopup {
                candidates,
                selected: self.completion.index(),
            };
            let popup_area = popup.popup_area(area);
            frame.render_widget(tuirealm::ratatui::widgets::Clear, popup_area);
            frame.render_widget(popup, popup_area);
        }
    }
}

struct KeyInput<'a>(&'a tuirealm::event::KeyEvent);

impl From<KeyInput<'_>> for TaInput {
    fn from(ki: KeyInput<'_>) -> Self {
        let key = ki.0;
        let ta_key = match &key.code {
            Key::Char(c) => TaKey::Char(*c),
            Key::Enter => TaKey::Enter,
            Key::Backspace => TaKey::Backspace,
            Key::Delete => TaKey::Delete,
            Key::Left => TaKey::Left,
            Key::Right => TaKey::Right,
            Key::Up => TaKey::Up,
            Key::Down => TaKey::Down,
            Key::Home => TaKey::Home,
            Key::End => TaKey::End,
            Key::PageUp => TaKey::PageUp,
            Key::PageDown => TaKey::PageDown,
            Key::Tab => TaKey::Tab,
            Key::Esc => TaKey::Esc,
            Key::Function(n) => TaKey::F(*n),
            _ => TaKey::Null,
        };
        TaInput {
            key: ta_key,
            ctrl: key.modifiers.contains(KeyModifiers::CONTROL),
            alt: key.modifiers.contains(KeyModifiers::ALT),
            shift: key.modifiers.contains(KeyModifiers::SHIFT),
        }
    }
}

fn apply_textarea_style(textarea: &mut TextArea<'static>) {
    textarea.set_cursor_line_style(Style::default());
    textarea.set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
    textarea.set_style(Style::default().fg(Color::White));
}
