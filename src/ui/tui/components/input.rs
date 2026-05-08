//! `InputComponent` — text input area with history, completion, hints, and effects.
//!
//! Not mounted in the tuirealm App — accessed directly from the main loop.
//! Only `ChatComponent` is the active tuirealm component.

use crate::config::{ProviderConfig, ResolvedProvider, pie_home};
use crate::providers::Model;
use crate::registry::Registry;
use crate::session::Session;
use crate::tools::plan::{PlanRepo, StepStatus};
use crate::ui::tui::realm::{Msg, StreamEvent};
use crate::ui::tui::stream::{StreamContext, spawn_stream};
use crate::ui::tui::widgets::completion::{
    CompletionPopup, CompletionState, Direction, slash_token_range,
};
use crate::ui::tui::widgets::history::InputHistory;
use crate::ui::tui::widgets::input::{InputView, cursor_position};
use p1e_sandbox::SandboxConfig;
use std::collections::HashMap;
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
    pub last_tick: Instant,
    pub spinner_frame: usize,
    stream_effect_active: bool,
    pub model: Model,
    pub provider: ResolvedProvider,
    pub available_providers: HashMap<String, ProviderConfig>,
    pub session_id: uuid::Uuid,
    pub session_pool: Arc<crate::db::DbPool>,
    pub sandbox_settings: Arc<SandboxConfig>,
    pub max_steps: u32,
    pub stream_abort: Option<mpsc::UnboundedSender<()>>,
    last_query: Option<String>,
    pub registry: Arc<Registry>,
}

impl InputComponent {
    pub fn new(
        model: Model,
        provider: ResolvedProvider,
        session: &Session,
        sandbox_settings: Arc<SandboxConfig>,
        max_steps: u32,
        available_providers: HashMap<String, ProviderConfig>,
        registry: Arc<Registry>,
    ) -> Self {
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
            completion: CompletionState::new(registry.clone()),
            current_hint: String::new(),
            effects: EffectManager::default(),
            last_frame: Instant::now(),
            last_tick: Instant::now(),
            spinner_frame: 0,
            stream_effect_active: false,
            model,
            provider,
            available_providers,
            session_id,
            session_pool,
            sandbox_settings,
            max_steps,
            stream_abort: None,
            last_query: None,
            registry,
        }
    }

    pub fn input_text(&self) -> String {
        self.textarea.lines().join("\n")
    }

    pub fn is_input_empty(&self) -> bool {
        self.textarea.lines().iter().all(String::is_empty)
    }

    pub fn input_line_count(&self) -> usize {
        self.textarea.lines().len().max(1)
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
        ta.move_cursor(tui_textarea::CursorMove::End);
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

    pub fn history_prev(&mut self) {
        let text = self.history.prev().map(ToOwned::to_owned);
        if let Some(text) = text {
            self.set_input_text(&text);
        }
    }

    pub fn history_next(&mut self) {
        let text = self.history.next().map(ToOwned::to_owned);
        if let Some(text) = text {
            self.set_input_text(&text);
        } else {
            // At the end of history — clear input
            let mut empty = TextArea::default();
            apply_textarea_style(&mut empty);
            self.textarea = empty;
            self.completion.reset();
            self.current_hint.clear();
        }
    }

    fn apply_completion(&mut self, completion: &str) {
        let row = self.textarea.cursor().0;
        let mut lines: Vec<String> = self.textarea.lines().iter().map(String::from).collect();
        if let Some(line) = lines.get_mut(row) {
            if let Some((start, end)) = slash_token_range(line) {
                *line = format!("{}{}{}", &line[..start], completion, &line[end..]);
            } else {
                *line = completion.to_string();
            }
        }
        let mut ta = TextArea::new(lines);
        apply_textarea_style(&mut ta);
        ta.move_cursor(tui_textarea::CursorMove::End);
        self.textarea = ta;
        self.current_hint.clear();
    }

    fn update_hint(&mut self) {
        self.current_hint.clear();
        let line = self.current_line();
        if slash_token_range(&line).is_none() || line.len() < 2 {
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
            (Key::Up, KeyModifiers::NONE) if self.input_line_count() <= 1 => {
                self.history_prev();
            }
            (Key::Down, KeyModifiers::NONE) if self.input_line_count() <= 1 => {
                self.history_next();
            }
            (Key::PageUp, KeyModifiers::NONE) => {
                return Some(Msg::KeyboardScroll(-20));
            }
            (Key::PageDown, KeyModifiers::NONE) => {
                return Some(Msg::KeyboardScroll(20));
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
        self.last_query = Some(query.to_string());

        let ctx = StreamContext::from(&*self);
        tokio::spawn(spawn_stream(ctx, query.to_string(), tx.clone(), abort_rx));
    }

    pub fn take_abort_handle(&mut self) -> Option<mpsc::UnboundedSender<()>> {
        self.stream_abort.take()
    }

    pub fn finish_stream(&mut self) -> Option<String> {
        self.stream_abort = None;
        self.last_query.take()
    }

    /// Returns a provider config based on current model.
    pub fn get_provider(&self) -> ResolvedProvider {
        self.provider.clone()
    }

    pub fn set_model(&mut self, model_name: &str) {
        self.provider.model = model_name.to_string();
        if let Ok(new_model) = crate::providers::build_from_resolved(&self.provider) {
            self.model = new_model;
        }
    }

    pub fn set_provider(&mut self, provider: ResolvedProvider) {
        self.provider = provider;
        if let Ok(new_model) = crate::providers::build_from_resolved(&self.provider) {
            self.model = new_model;
        }
    }

    pub fn active_steps(&self, is_streaming: bool) -> Vec<String> {
        if !is_streaming {
            return vec![];
        }

        let steps = crate::ui::tui::realm::run_sync(
            self.session_pool.load_steps(&self.session_id.to_string()),
        )
        .unwrap_or_default();

        let active: Vec<String> = steps
            .iter()
            .filter(|p| p.status == StepStatus::InProgress)
            .map(|p| p.name.clone())
            .collect();
        if !active.is_empty() {
            return active;
        }

        if steps.is_empty() {
            return vec!["Planning".to_string()];
        }

        steps
            .iter()
            .rfind(|p| {
                matches!(
                    p.status,
                    StepStatus::Completed | StepStatus::Failed | StepStatus::Skipped
                )
            })
            .map(|p| vec![p.name.clone()])
            .unwrap_or_default()
    }

    /// Reset to a new session: update `session_id`, create fresh history, clear input.
    pub fn reset_session(&mut self, session_id: uuid::Uuid) {
        self.session_id = session_id;
        let history_dir = pie_home().join("history");
        let history_path = history_dir.join(format!("{session_id}.txt"));
        self.history = InputHistory::new(history_path);
        self.completion.reset();
        self.current_hint.clear();
        let mut empty = TextArea::default();
        apply_textarea_style(&mut empty);
        self.textarea = empty;

        let _ = crate::ui::tui::realm::run_sync(PlanRepo::delete_steps(
            &*self.session_pool,
            &session_id.to_string(),
        ));
    }

    // ── Rendering ────────────────────────────────────────────────────

    pub fn render(&mut self, frame: &mut Frame, area: Rect, is_streaming: bool) {
        let is_empty = self.is_input_empty();
        let empty_line = vec![String::new()];
        let text_lines: &[String] = if is_empty {
            &empty_line
        } else {
            self.textarea.lines()
        };
        let cursor = self.textarea.cursor();

        // Update spinner frame
        if is_streaming {
            let elapsed = self.last_tick.elapsed();
            if elapsed >= std::time::Duration::from_millis(80) {
                self.spinner_frame = self.spinner_frame.wrapping_add(1);
                self.last_tick = Instant::now();
            }
        } else {
            self.spinner_frame = 0;
        }

        let input_view = InputView {
            text_lines,
            cursor_row: cursor.0,
            placeholder: PLACEHOLDER,
            hint: &self.current_hint,
            is_empty,
            is_streaming,
            completions: &self.registry.completions,
        };
        frame.render_widget(input_view, area);

        #[allow(clippy::cast_possible_truncation)]
        let visible_rows = area.height as usize;
        if cursor.0 < visible_rows {
            let (cx, cy) = cursor_position(area, cursor.0, cursor.1);
            frame.set_cursor_position((cx, cy));
        }

        // Streaming status bar effects (plan title only, border removed)
        match (is_streaming, self.stream_effect_active) {
            (true, false) => {
                // Plan title animation - smooth full spectrum cycling
                let title_fx = fx::repeating(fx::hsl_shift_fg(
                    [360.0, 0.0, 0.0],
                    (3000, tachyonfx::Interpolation::Linear),
                ));
                let title_fx = title_fx.with_filter(CellFilter::FgColor(Color::Cyan));
                self.effects.add_unique_effect("stream_title", title_fx);

                self.stream_effect_active = true;
            }
            (false, true) => {
                self.effects.cancel_unique_effect("stream_title");
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
                max_height: area.y,
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
