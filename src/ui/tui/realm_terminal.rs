//! tuirealm-based TUI entry point.
//!
//! `ChatComponent` is the active tuirealm component — it routes all keyboard events,
//! delegating editing keys to `InputComponent` via `Msg::KeyboardToInput`.
//!
//! `InputComponent` is held directly (not mounted in the App) since it never receives
//! events through tuirealm's event system — all interaction goes through direct calls.
//!
//! Each frame: drain all events, merge them, then render once.

use crate::config::{PieConfig, ResolvedProvider};
use crate::providers::{Model, fetch_models};
use crate::session::Session;
use crate::ui::tui::command::{Command, CommandAction};
use crate::ui::tui::components::chat::{ActiveDialog, ChatComponent};
use crate::ui::tui::components::input::InputComponent;
use crate::ui::tui::realm::{App, Id, Msg, StreamEvent, StreamPort};
use crate::ui::tui::state::ChatMessage;
use crate::ui::tui::widgets::status_bar::StatusBar;
use crate::ui::tui::widgets::task_list::TaskView;
use anyhow::{Context, Result};
use arboard::Clipboard;
use p1e_srt::SandboxConfig;
use std::io::stdout;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tuirealm::application::PollStrategy;
use tuirealm::event::{Key, KeyModifiers};
use tuirealm::listener::EventListenerCfg;
use tuirealm::props::{Color, Style};
use tuirealm::ratatui::backend::CrosstermBackend;
use tuirealm::ratatui::crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use tuirealm::ratatui::crossterm::execute;
use tuirealm::ratatui::layout::{Constraint, Direction, Layout};
use tuirealm::ratatui::widgets::{Block, BorderType, Borders};

type Terminal = tuirealm::ratatui::Terminal<CrosstermBackend<std::io::Stdout>>;

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

/// Process a single message. Returns `Some(Msg::Quit)` if the app should exit.
fn process_msg(
    msg: Msg,
    app: &mut App,
    input: &mut InputComponent,
    tx: &mpsc::UnboundedSender<StreamEvent>,
) -> Option<Msg> {
    match msg {
        Msg::Quit => return Some(Msg::Quit),

        Msg::KeyboardToInput(key) => {
            // Open help on '?' when input is empty
            if key.code == Key::Char('?')
                && key.modifiers == KeyModifiers::NONE
                && input.is_input_empty()
            {
                if let Some(chat) = chat_mut!(app) {
                    chat.set_help_dialog();
                }
                return None;
            }
            if let Some(inner) = input.handle_key_event(&key) {
                return process_msg(inner, app, input, tx);
            }
        }

        Msg::Submit(text) => {
            return handle_submit(&text, app, input, tx);
        }

        Msg::CopySelection => {
            if let Some(chat) = chat_mut!(app)
                && let Some(text) = chat.get_selected_text()
                && let Ok(mut cb) = Clipboard::new()
            {
                let _ = cb.set_text(text);
            }
        }

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
                && let Ok(mut resolved) = ResolvedProvider::try_from(cfg.clone())
            {
                resolved.name = provider_name;
                let tx = tx.clone();
                tokio::spawn(async move {
                    match fetch_models(&resolved).await {
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
                && let Ok(mut resolved) = ResolvedProvider::try_from(cfg.clone())
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

        _ => {}
    }
    None
}

fn handle_submit(
    text: &str,
    app: &mut App,
    input: &mut InputComponent,
    tx: &mpsc::UnboundedSender<StreamEvent>,
) -> Option<Msg> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }

    input.take_input();

    let cmd = Command::parse(text, &input.registry);
    match cmd.dispatch(&input.registry) {
        CommandAction::AddMessage(msg) => {
            if let Some(chat) = chat_mut!(app) {
                chat.add_message(ChatMessage::user(text));
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
                    chat.active_dialog = ActiveDialog::ModelSelector {
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
                    match fetch_models(&provider).await {
                        Ok(models) => {
                            tracing::info!(count = models.len(), "fetched initial models");
                            let _ = tx.send(StreamEvent::ModelList(models));
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "failed to fetch initial models");
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
        CommandAction::Help => {
            if let Some(chat) = chat_mut!(app) {
                chat.set_help_dialog();
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
            input.start_stream(&query, tx);
        }
        CommandAction::Quit => return Some(Msg::Quit),
    }
    None
}

pub async fn run_tui(
    model: Model,
    provider: ResolvedProvider,
    session: Session,
    sandbox_settings: Arc<SandboxConfig>,
    max_steps: u32,
    pie_config: PieConfig,
    registry: Arc<crate::registry::Registry>,
) -> Result<()> {
    let mut terminal = tuirealm::ratatui::init();
    terminal.clear()?;

    // Needed so that `MouseEvents` don't get turned into keyboard events.
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
        .crossterm_input_listener(Duration::from_millis(10), 3)
        .add_port(Box::new(StreamPort::new(rx)), Duration::from_millis(20), 1)
        .tick_interval(Duration::from_millis(20));

    let mut app = App::init(listener_cfg);
    let mut input = InputComponent::new(
        model,
        provider,
        &session,
        sandbox_settings,
        max_steps,
        pie_config.provider.clone(),
        registry.clone(),
    );
    let current_model = input.provider.model.clone();

    app.mount(
        Id::Chat,
        Box::new(ChatComponent::new(
            messages,
            current_model,
            registry,
            input.task_list.clone(),
        )),
        vec![],
    )?;
    app.active(&Id::Chat)?;

    // Render initial frame immediately.
    let mut last_frame;
    let mut batch_buf: Vec<Msg> = Vec::with_capacity(32);
    render(&mut app, &mut input, &mut terminal)?;

    loop {
        last_frame = Instant::now();
        batch_buf.clear();
        let mut scroll_delta: i16 = 0;
        let batch = app
            .tick(PollStrategy::UpTo(100, Duration::from_millis(8)))
            .context("can't poll events")?;

        let elapsed = last_frame.elapsed().as_micros();
        tracing::debug!(elapsed, "render");

        let mut exit = false;
        if batch.is_empty() && elapsed < 100_000 {
            continue;
        }
        for msg in batch {
            match msg {
                Msg::Quit => {
                    exit = true;
                    break;
                }
                Msg::ScrollChat(delta) | Msg::KeyboardScroll(delta) => {
                    scroll_delta = scroll_delta.saturating_add(delta);
                }
                Msg::CloseHelp => {
                    if let Some(chat) = chat_mut!(app) {
                        chat.active_dialog = ActiveDialog::None;
                    }
                }
                other => batch_buf.push(other),
            }
        }

        if exit {
            break;
        }

        if let Some(chat) = chat_mut!(app) {
            if scroll_delta < 0 {
                chat.scroll_up(scroll_delta.unsigned_abs());
            } else if scroll_delta > 0 {
                chat.scroll_down(scroll_delta.unsigned_abs());
            }
        }

        for msg in batch_buf.drain(..) {
            if let Some(Msg::Quit) = process_msg(msg, &mut app, &mut input, &tx) {
                cleanup(&mut terminal);
                return Ok(());
            }
        }

        render(&mut app, &mut input, &mut terminal)?;
        tracing::debug!(render = last_frame.elapsed().as_micros(), "render");
    }

    cleanup(&mut terminal);
    Ok(())
}

/// Restore terminal state: disable mouse capture, restore cooked mode.
fn cleanup(terminal: &mut Terminal) {
    let _ = execute!(stdout(), DisableMouseCapture);
    let _ = terminal.clear();
    tuirealm::ratatui::restore();
}

/// Render a single frame: chat messages + input area.
fn render(app: &mut App, input: &mut InputComponent, terminal: &mut Terminal) -> Result<()> {
    terminal.draw(|f| {
        let area = f.area();
        #[allow(clippy::cast_possible_truncation)]
        let input_lines = input.input_line_count().clamp(1, 8) as u16;
        let input_height = input_lines;

        let (show_tasks, task_count) = if let Some(chat) = chat_ref!(app) {
            let guard = crate::tools::safe_lock(&chat.task_list);
            (
                chat.show_tasks && !guard.tasks.is_empty(),
                guard.tasks.len(),
            )
        } else {
            (false, 0)
        };

        let mut constraints = vec![
            Constraint::Min(5),    // Messages
            Constraint::Length(1), // Status Bar
        ];

        if show_tasks {
            #[allow(clippy::cast_possible_truncation)]
            let tasks_height = (task_count as u16 + 1).min(10); // +1 for top border
            constraints.push(Constraint::Length(tasks_height));
        }

        constraints.push(Constraint::Length(input_height)); // Input

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);

        let messages_area = chunks.first().copied().unwrap_or(area);
        let status_bar_area = chunks.get(1).copied().unwrap_or(area);

        let (tasks_area, input_area) = if show_tasks {
            (
                chunks.get(2).copied(),
                chunks.get(3).copied().unwrap_or(area),
            )
        } else {
            (None, chunks.get(2).copied().unwrap_or(area))
        };

        app.view(&Id::Chat, f, messages_area);

        // Status Bar rendering
        let is_streaming = chat_ref!(app).is_some_and(ChatComponent::is_streaming);
        let active_tasks = input.active_tasks(is_streaming);
        let status_bar = StatusBar::new(active_tasks, is_streaming, input.spinner_frame);
        f.render_widget(status_bar, status_bar_area);

        if let Some(t_area) = tasks_area
            && let Some(chat) = chat_ref!(app)
        {
            let guard = crate::tools::safe_lock(&chat.task_list);
            let task_view = TaskView::new(&guard).block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(Color::DarkGray))
                    .border_type(BorderType::Plain)
                    .title(" Tasks "),
            );
            f.render_widget(task_view, t_area);
        }

        input.render(f, input_area, is_streaming);
    })?;
    Ok(())
}
