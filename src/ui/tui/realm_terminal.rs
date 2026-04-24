//! tuirealm-based TUI entry point.
//!
//! `ChatComponent` is the active tuirealm component — it routes all keyboard events,
//! delegating editing keys to `InputComponent` via `Msg::KeyboardToInput`.
//!
//! `InputComponent` is held directly (not mounted in the App) since it never receives
//! events through tuirealm's event system — all interaction goes through direct calls.
//!
//! Each frame: drain all events, merge them, then render once.

use crate::providers::Model;
use crate::session::Session;
use crate::ui::tui::command::{Command, CommandAction};
use crate::ui::tui::components::chat::ChatComponent;
use crate::ui::tui::components::input::InputComponent;
use crate::ui::tui::realm::{App, Id, Msg, StreamEvent, StreamPort};
use crate::ui::tui::state::ChatMessage;
use anyhow::Result;
use p1e_srt::SandboxConfig;
use std::io::stdout;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tuirealm::application::PollStrategy;
use tuirealm::listener::EventListenerCfg;
use tuirealm::ratatui::backend::CrosstermBackend;
use tuirealm::ratatui::crossterm::event::EnableMouseCapture;
use tuirealm::ratatui::crossterm::execute;

type Terminal = tuirealm::ratatui::Terminal<CrosstermBackend<std::io::Stdout>>;

/// Target frame interval. 30 fps ≈ 33ms.
const FRAME_INTERVAL: Duration = Duration::from_millis(33);

/// Helper to downcast `ChatComponent` mutably.
macro_rules! chat_mut {
    ($app:expr) => {
        $app.get_component_mut(&Id::Chat)
            .and_then(|c| c.as_any_mut().downcast_mut::<ChatComponent>())
    };
}

/// Helper to downcast `ChatComponent` immutably.
macro_rules! chat_ref {
    ($app:expr) => {
        $app.get_component(&Id::Chat)
            .and_then(|c| c.as_any().downcast_ref::<ChatComponent>())
    };
}

/// Drain all pending events into a single Vec.
/// Blocks on the first poll (up to `FRAME_INTERVAL`), then drains non-blocking.
fn drain_all(app: &mut App) -> DrainResult {
    let mut msgs = Vec::new();
    let mut listener_dead = false;

    loop {
        match app.tick(PollStrategy::UpTo(1000, FRAME_INTERVAL)) {
            Ok(batch) => {
                if batch.is_empty() {
                    break;
                }
                msgs.extend(batch);
            }
            Err(tuirealm::application::ApplicationError::Listener(_)) => {
                listener_dead = true;
                break;
            }
            Err(e) => {
                tracing::error!("tick error: {e}");
                listener_dead = true;
                break;
            }
        }
    }

    DrainResult {
        msgs,
        listener_dead,
    }
}

struct DrainResult {
    msgs: Vec<Msg>,
    listener_dead: bool,
}

/// Merged result of draining all events for one frame.
struct FrameUpdate {
    quit: bool,
    scroll_up: u16,
    scroll_down: u16,
}

impl FrameUpdate {
    /// Process all drained Msgs: merge scrolls, handle quit, apply state changes.
    fn from_msgs(
        msgs: Vec<Msg>,
        app: &mut App,
        input: &mut InputComponent,
        tx: &mpsc::UnboundedSender<StreamEvent>,
    ) -> Self {
        let mut out = FrameUpdate {
            quit: false,
            scroll_up: 0,
            scroll_down: 0,
        };

        for msg in msgs {
            out.process_one(msg, app, input, tx);
            if out.quit {
                break;
            }
        }

        out
    }

    fn process_one(
        &mut self,
        msg: Msg,
        app: &mut App,
        input: &mut InputComponent,
        tx: &mpsc::UnboundedSender<StreamEvent>,
    ) {
        match msg {
            Msg::ScrollUp(n) => self.scroll_up = self.scroll_up.saturating_add(n),
            Msg::ScrollDown(n) => self.scroll_down = self.scroll_down.saturating_add(n),
            Msg::Quit => self.quit = true,

            Msg::KeyboardToInput(key) => {
                if let Some(inner) = input.handle_key_event(&key) {
                    self.process_one(inner, app, input, tx);
                }
            }

            Msg::Submit(text) => self.handle_submit(&text, app, input, tx),

            Msg::StreamDone(output) => {
                if let Some(chat) = chat_mut!(app) {
                    chat.finish_stream(output);
                }
                input.finish_stream();
            }

            Msg::FetchModels(provider_name) => {
                let providers = input.available_providers.clone();
                tracing::info!(provider = %provider_name, "fetching models for provider");
                if let Some(cfg) = providers.get(&provider_name)
                    && let Ok(mut resolved) = crate::config::ResolvedProvider::try_from(cfg.clone())
                {
                    resolved.name = provider_name;
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        match crate::providers::fetch_models(&resolved).await {
                            Ok(models) => {
                                tracing::info!(count = models.len(), "fetched models");
                                let _ = tx.send(StreamEvent::ModelList(models));
                            }
                            Err(e) => {
                                tracing::error!(error = %e, "failed to fetch models");
                                let _ = tx.send(StreamEvent::Error(e.to_string()));
                            }
                        }
                    });
                } else {
                    tracing::error!(provider = %provider_name, "provider config not found or invalid");
                }
            }

            Msg::SwitchProviderAndModel(provider_name, model_name) => {
                let providers = input.available_providers.clone();
                if let Some(cfg) = providers.get(&provider_name)
                    && let Ok(mut resolved) = crate::config::ResolvedProvider::try_from(cfg.clone())
                {
                    resolved.name = provider_name;
                    resolved.model = model_name;
                    input.set_provider(resolved);
                    if let Some(chat) = chat_mut!(app) {
                        chat.add_message(ChatMessage::system(&format!(
                            "Switched to provider: {} / model: {}",
                            input.provider.name, input.provider.model
                        )));
                        chat.current_model.clone_from(&input.provider.model);
                    }
                }
            }

            Msg::Redraw => {}

            Msg::ToggleHelp => {
                if let Some(chat) = chat_mut!(app) {
                    use crate::ui::tui::components::chat::ActiveDialog;
                    if chat.active_dialog == ActiveDialog::Help {
                        chat.active_dialog = ActiveDialog::None;
                    } else {
                        chat.active_dialog = ActiveDialog::Help;
                    }
                }
            }
            Msg::CloseHelp => {
                if let Some(chat) = chat_mut!(app) {
                    chat.active_dialog = crate::ui::tui::components::chat::ActiveDialog::None;
                }
            }
        }
    }

    fn handle_submit(
        &mut self,
        text: &str,
        app: &mut App,
        input: &mut InputComponent,
        tx: &mpsc::UnboundedSender<StreamEvent>,
    ) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        let cmd = Command::parse(&text);
        match cmd.dispatch() {
            CommandAction::AddMessage(msg) => {
                if let Some(chat) = chat_mut!(app) {
                    if text != "/help" && text != "/h" {
                        chat.add_message(ChatMessage::user(&text));
                    }
                    chat.add_message(msg);
                }
            }
            CommandAction::Model(name) => {
                if let Some(new_model) = name {
                    input.set_model(&new_model);
                    if let Some(chat) = chat_mut!(app) {
                        chat.add_message(ChatMessage::system(&format!(
                            "Switched to model: {new_model}"
                        )));
                        chat.current_model = new_model;
                    }
                } else {
                    input.take_input();
                    let provider = input.get_provider();
                    let mut available_providers = input
                        .available_providers
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>();
                    available_providers.sort();
                    let provider_idx = available_providers
                        .iter()
                        .position(|p| p == &provider.name)
                        .unwrap_or(0);

                    if let Some(chat) = chat_mut!(app) {
                        chat.active_dialog =
                            crate::ui::tui::components::chat::ActiveDialog::ModelSelector {
                                providers: available_providers,
                                provider_idx,
                                models: Vec::new(),
                                selected_idx: None,
                                is_loading: true,
                                error: None,
                            };
                    }
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        tracing::info!(provider = %provider.name, "fetching initial model list");
                        match crate::providers::fetch_models(&provider).await {
                            Ok(models) => {
                                tracing::info!(count = models.len(), "fetched initial models");
                                let _ = tx.send(StreamEvent::ModelList(models));
                            }
                            Err(e) => {
                                tracing::error!(error = %e, "failed to fetch initial models");
                                // Make sure this error reaches the component to clear the dialog
                                let _ = tx.send(StreamEvent::Error(e.to_string()));
                            }
                        }
                    });
                }
            }
            CommandAction::ClearMessages => {
                if let Some(chat) = chat_mut!(app) {
                    chat.clear_messages();
                }
            }
            CommandAction::NewSession => {
                if let Ok(new_session) = Session::create(input.session_pool.clone()) {
                    if let Some(chat) = chat_mut!(app) {
                        chat.clear_messages();
                        chat.add_message(ChatMessage::system("Welcome to pie! Type ? for help."));
                    }
                    input.reset_session(new_session.id);
                }
            }
            CommandAction::Stream(query) => {
                if let Some(chat) = chat_mut!(app) {
                    chat.add_message(ChatMessage::user(&query));
                    chat.start_response();
                }
                input.take_input();
                input.start_stream(&query, tx);
            }
            CommandAction::Quit => self.quit = true,
        }
    }
}

pub async fn run_tui(
    model: Model,
    provider: crate::config::ResolvedProvider,
    session: Session,
    sandbox_settings: Arc<SandboxConfig>,
    max_steps: u32,
    pie_config: crate::config::PieConfig,
) -> Result<()> {
    let mut terminal = tuirealm::ratatui::init();
    terminal.clear()?;

    // needed so that `MouseEvents` don't get turned into keyboard events.
    execute!(stdout(), EnableMouseCapture)?;

    // Build initial messages
    let mut messages = vec![ChatMessage::system("Welcome to pie! Type ? for help.")];
    for entry in session.history_entries() {
        let msg = match entry.role {
            crate::session::Role::User => ChatMessage::user(&entry.content),
            crate::session::Role::Assistant => ChatMessage::assistant(&entry.content),
            crate::session::Role::System => ChatMessage::system(&entry.content),
            crate::session::Role::Tool => ChatMessage::tool(&entry.content),
        };
        messages.push(msg);
    }

    let (tx, rx) = mpsc::unbounded_channel::<StreamEvent>();

    let listener_cfg = EventListenerCfg::<StreamEvent>::default()
        .crossterm_input_listener(Duration::from_millis(10), 1)
        .add_port(Box::new(StreamPort::new(rx)), FRAME_INTERVAL, 1)
        .tick_interval(FRAME_INTERVAL);

    let mut app = App::init(listener_cfg);
    let mut input = InputComponent::new(
        model,
        provider,
        &session,
        sandbox_settings,
        max_steps,
        pie_config.provider.clone(),
    );
    let current_model = input.provider.model.clone();

    app.mount(
        Id::Chat,
        Box::new(ChatComponent::new(messages, current_model)),
        vec![],
    )?;
    app.active(&Id::Chat)?;

    // Render initial frame immediately.
    let mut last_frame = Instant::now();
    render(&mut app, &mut input, &mut terminal)?;

    loop {
        // ── Phase 1: Drain all events ─────────────────────────────────
        let DrainResult {
            msgs,
            listener_dead,
        } = drain_all(&mut app);
        if listener_dead {
            break;
        }

        // ── Phase 2: Merge + apply ────────────────────────────────────
        let streaming = chat_ref!(app).is_some_and(ChatComponent::is_streaming);

        if !msgs.is_empty() || streaming {
            let frame = FrameUpdate::from_msgs(msgs, &mut app, &mut input, &tx);
            if frame.quit {
                break;
            }

            // Apply net scroll delta (merged from all ScrollUp/ScrollDown)
            if (frame.scroll_up > 0 || frame.scroll_down > 0)
                && let Some(chat) = chat_mut!(app)
            {
                if frame.scroll_up > frame.scroll_down {
                    chat.scroll_up(frame.scroll_up - frame.scroll_down);
                } else {
                    chat.scroll_down(frame.scroll_down - frame.scroll_up);
                }
            }

            // ── Phase 3: Render ───────────────────────────────────────
            let draw_start = Instant::now();
            render(&mut app, &mut input, &mut terminal)?;
            let draw_us = draw_start.elapsed().as_micros();
            let frame_ms = last_frame.elapsed().as_millis();
            last_frame = Instant::now();
            tracing::debug!("redraw: {draw_us}µs, frame: {frame_ms}ms");
        }
    }

    tuirealm::ratatui::restore();
    Ok(())
}

/// Render a single frame: chat messages + input area.
fn render(app: &mut App, input: &mut InputComponent, terminal: &mut Terminal) -> Result<()> {
    terminal.draw(|f| {
        let area = f.area();
        #[allow(clippy::cast_possible_truncation)]
        let input_lines = input.input_line_count().clamp(1, 8) as u16;
        let input_height = input_lines + 2;

        let chunks = tuirealm::ratatui::layout::Layout::default()
            .direction(tuirealm::ratatui::layout::Direction::Vertical)
            .constraints([
                tuirealm::ratatui::layout::Constraint::Min(5),
                tuirealm::ratatui::layout::Constraint::Length(input_height),
            ])
            .split(area);

        let messages_area = chunks.first().copied().unwrap_or(area);
        let input_area = chunks.get(1).copied().unwrap_or(area);

        app.view(&Id::Chat, f, messages_area);

        let is_streaming = chat_ref!(app).is_some_and(ChatComponent::is_streaming);
        input.render(f, input_area, is_streaming);
    })?;
    Ok(())
}
