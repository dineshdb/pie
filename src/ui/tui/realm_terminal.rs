//! tuirealm-based TUI entry point.
//!
//! `ChatComponent` is the active tuirealm component — it routes all keyboard events,
//! delegating editing keys to `InputComponent` via `Msg::KeyboardToInput`.
//!
//! `InputComponent` is held directly (not mounted in the App) since it never receives
//! events through tuirealm's event system — all interaction goes through direct calls.
//!
//! Each frame: drain all events, merge them, then render once.

use crate::config::{PieConfig, ResolvedProvider, get_providers_data};
use crate::session::{Role, Session};
use crate::ui::tui::command::{Command, CommandAction};
use crate::ui::tui::components::chat::{ActiveDialog, ChatComponent, ModelSelectorState};
use crate::ui::tui::components::input::InputComponent;
use crate::ui::tui::realm::{App, Id, Msg, StreamEvent, StreamPort, run_sync};
use crate::ui::tui::state::ChatMessage;
use crate::ui::tui::widgets::mode_bar::ModeBar;
use crate::ui::tui::widgets::status_bar::StatusBar;
use anyhow::{Context, Result};
use arboard::Clipboard;
use p1e_sandbox::SandboxConfig;
use std::io::stdout;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tuirealm::application::PollStrategy;
use tuirealm::event::{Key, KeyModifiers};
use tuirealm::listener::EventListenerCfg;
use tuirealm::ratatui::backend::CrosstermBackend;
use tuirealm::ratatui::crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use tuirealm::ratatui::crossterm::execute;
use tuirealm::ratatui::layout::{Constraint, Direction, Layout};

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

/// Persist the current mode marker to session.
fn apply_mode_toggle(input: &mut InputComponent, app: &mut App) {
    input.pending_mode_toggles = 0;
    input.mode_toggle_deadline = None;
    let target = input.mode;
    let session_id = input.session_id.clone();
    let pool = input.session_pool.clone();
    tokio::spawn(async move {
        if let Ok(mut session) = Session::load(pool, session_id).await {
            let _ = session.add_system(&target.system_marker()).await;
        }
    });
    if let Some(chat) = chat_mut!(app) {
        chat.add_message(ChatMessage::system(&format!(
            "Switched to **{}** mode (Ctrl+K)",
            target.short_name()
        )));
    }
}

/// Flush pending mode toggles: immediately for new messages,
/// or after debounce deadline for idle waits.
fn flush_pending_toggle(input: &mut InputComponent, app: &mut App, force: bool) {
    if input.pending_mode_toggles == 0 {
        return;
    }
    if !force
        && let Some(deadline) = input.mode_toggle_deadline
        && Instant::now() < deadline
    {
        return;
    }
    apply_mode_toggle(input, app);
}

/// Process a single message. Returns `Some(Msg::Quit)` if the app should exit.
fn process_msg(
    msg: Msg,
    app: &mut App,
    input: &mut InputComponent,
    tx: &mpsc::UnboundedSender<StreamEvent>,
) -> Option<Msg> {
    flush_pending_toggle(input, app, true);
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

        Msg::UserMessage(text) => {
            if let Some(chat) = chat_mut!(app) {
                chat.update_last_user_message(text);
            }
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
            let query = input.finish_stream();
            crate::ui::notify::turn_complete(query.as_deref());
        }

        Msg::StreamError(err) => {
            if let Some(chat) = chat_mut!(app) {
                chat.stream_error(&err);
            }
            let _ = input.finish_stream();
        }

        Msg::FetchModels(provider_name) => {
            let providers = input.available_providers.clone();
            tracing::info!(provider = %provider_name, "fetching models for provider");
            if let Some(cfg) = providers.get(&provider_name)
                && let Ok(providers_data) = get_providers_data()
                && let Ok(mut resolved) = ResolvedProvider::resolve(cfg.clone(), providers_data)
            {
                resolved.name = provider_name;
                let tx = tx.clone();
                tokio::spawn(async move {
                    match resolved.fetch_models().await {
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
                && let Ok(providers_data) = get_providers_data()
                && let Ok(mut resolved) = ResolvedProvider::resolve(cfg.clone(), providers_data)
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

        Msg::ModeChanged(mode) => {
            input.mode = mode;
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
                // For AddMessage (built-in help/skills), we can show the original text
                // or just skip adding the user message if it's a command.
                // But for consistency, let's just let it be.
                chat.add_message(ChatMessage::user(text));
                chat.add_message(msg);
            }
        }
        CommandAction::Model(name) => {
            handle_model_command(name, app, input, tx);
        }
        CommandAction::Mode(args) => {
            handle_mode_command(args.as_deref(), app, input);
        }
        CommandAction::Help => {
            if let Some(chat) = chat_mut!(app) {
                chat.set_help_dialog();
            }
        }
        CommandAction::NewSession => {
            let new_session = run_sync(Session::create(input.session_pool.clone()));
            if let Ok(new_session) = new_session {
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
        CommandAction::Shell(command) => {
            execute_shell_direct(&command, app, input, tx);
        }
        CommandAction::Quit => return Some(Msg::Quit),
    }
    None
}

fn handle_mode_command(args: Option<&str>, app: &mut App, input: &mut InputComponent) {
    use crate::plugin::AgentMode;
    use crate::plugin::modes::load_mode_file;

    let Some(mode_name) = args.filter(|a| !a.is_empty()) else {
        let modes = AgentMode::all()
            .iter()
            .filter_map(|m| {
                let desc = load_mode_file(*m).map(|f| f.description)?;
                Some(format!("  /mode {} — {}", m.short_name(), desc))
            })
            .collect::<Vec<_>>()
            .join("\n");
        if let Some(chat) = chat_mut!(app) {
            chat.add_message(ChatMessage::system(&format!("Available modes:\n{modes}")));
        }
        return;
    };

    let mode: AgentMode = match mode_name.parse() {
        Ok(m) => m,
        Err(e) => {
            if let Some(chat) = chat_mut!(app) {
                chat.add_message(ChatMessage::system(&format!("Error: {e}")));
            }
            return;
        }
    };

    let desc =
        load_mode_file(mode).map_or_else(|| mode.short_name().to_string(), |f| f.description);

    // Persist mode marker to session — ModePlugin reads it on next init
    let session_id = input.session_id.clone();
    let pool = input.session_pool.clone();
    tokio::spawn(async move {
        if let Ok(mut session) = Session::load(pool, session_id).await {
            let _ = session.add_system(&mode.system_marker()).await;
        }
    });

    input.mode = mode;

    if let Some(chat) = chat_mut!(app) {
        chat.add_message(ChatMessage::system(&format!(
            "Next message will be in **{}** mode — {}",
            mode.short_name(),
            desc
        )));
    }
}

fn handle_model_command(
    name: Option<String>,
    app: &mut App,
    input: &mut InputComponent,
    tx: &mpsc::UnboundedSender<StreamEvent>,
) {
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
            chat.active_dialog = ActiveDialog::ModelSelector(ModelSelectorState {
                providers: available_providers,
                provider_idx,
                models: Vec::new(),
                selected_idx: None,
                is_loading: true,
                error: None,
            });
        }
        let tx = tx.clone();
        tokio::spawn(async move {
            tracing::info!(provider = %provider.name, "fetching initial model list");
            match provider.fetch_models().await {
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

fn execute_shell_direct(
    command: &str,
    app: &mut App,
    input: &mut InputComponent,
    tx: &mpsc::UnboundedSender<StreamEvent>,
) {
    if let Some(chat) = chat_mut!(app) {
        chat.add_message(ChatMessage::user(&format!("!{command}")));
        chat.start_response();
    }

    let sandbox = input.sandbox_settings.clone();
    let tx = tx.clone();
    let command = command.to_string();
    let pool = input.session_pool.clone();
    let session_id = input.session_id.clone();

    tokio::spawn(async move {
        // Persist the user command
        if let Ok(mut session) = Session::load(pool.clone(), session_id.clone()).await {
            let _ = session.add_user(&format!("!{command}")).await;
        }

        let output = p1e_sandbox::build_shell_command(&command, &sandbox)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("PAGER", "cat")
            .env("EDITOR", "true")
            .output();

        let result = match output {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                let combined = if stderr.is_empty() {
                    stdout
                } else {
                    format!("{stdout}\n\nError:\n{stderr}")
                };
                jewels::redact(&crate::utils::anonymize_path(&combined)).into_owned()
            }
            Err(e) => format!("Failed to execute command: {e}"),
        };

        // Persist the assistant response
        if let Ok(mut session) = Session::load(pool, session_id).await {
            let _ = session.add_assistant(&result).await;
        }

        let _ = tx.send(StreamEvent::Done(result));
    });
}

pub async fn run_tui(
    model: agentsdk::OpenAI,
    provider: ResolvedProvider,
    session: Session,
    sandbox_settings: Arc<SandboxConfig>,
    max_steps: u32,
    pie_config: PieConfig,
    registry: Arc<crate::registry::Registry>,
) -> Result<()> {
    let (mut terminal, mut app, mut input, tx) = setup_tui(
        &session,
        model,
        provider,
        sandbox_settings,
        max_steps,
        &pie_config,
        registry,
    )?;

    let mut last_frame;
    let mut batch_buf: Vec<Msg> = Vec::with_capacity(32);

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
            if input.pending_mode_toggles > 0 {
                flush_pending_toggle(&mut input, &mut app, false);
            }
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

        if input.pending_mode_toggles > 0 {
            flush_pending_toggle(&mut input, &mut app, false);
        }
        render(&mut app, &mut input, &mut terminal)?;
        tracing::debug!(render = last_frame.elapsed().as_micros(), "render");
    }

    cleanup(&mut terminal);
    Ok(())
}

fn setup_tui(
    session: &Session,
    model: agentsdk::OpenAI,
    provider: ResolvedProvider,
    sandbox_settings: Arc<SandboxConfig>,
    max_steps: u32,
    pie_config: &PieConfig,
    registry: Arc<crate::registry::Registry>,
) -> Result<(
    Terminal,
    App,
    InputComponent,
    mpsc::UnboundedSender<StreamEvent>,
)> {
    let mut terminal = tuirealm::ratatui::init();
    terminal.clear()?;

    execute!(stdout(), EnableMouseCapture)?;

    let mut messages = vec![ChatMessage::system("Welcome to pie! Type ? for help.")];
    for entry in session.history_entries() {
        let msg = match entry.role() {
            Role::User => ChatMessage::user(&entry.content()),
            Role::Assistant => ChatMessage::assistant(&entry.content()),
            Role::System => ChatMessage::system(&entry.content()),
            Role::Tool => ChatMessage::tool(&entry.content()),
        };
        messages.push(msg);
    }

    let (tx, rx) = mpsc::unbounded_channel::<StreamEvent>();
    let listener_cfg = EventListenerCfg::<StreamEvent>::default()
        .crossterm_input_listener(Duration::from_millis(10), 3)
        .add_port(Box::new(StreamPort::new(rx)), Duration::from_millis(20), 1)
        .tick_interval(Duration::from_millis(20));

    let mut app = App::init(listener_cfg);

    let pending_permissions = Arc::new(std::sync::Mutex::new(None));

    let mut input = InputComponent::new(
        model,
        provider,
        session,
        sandbox_settings,
        max_steps,
        pie_config.provider.clone(),
        registry.clone(),
        pending_permissions.clone(),
    );
    let current_model = input.provider.model.clone();

    app.mount(
        Id::Chat,
        Box::new(ChatComponent::new(
            messages,
            current_model,
            registry,
            pending_permissions,
        )),
        vec![],
    )?;
    app.active(&Id::Chat)?;

    render(&mut app, &mut input, &mut terminal)?;

    Ok((terminal, app, input, tx))
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

        let constraints = vec![
            Constraint::Min(5),               // Messages
            Constraint::Length(1),            // Thinking Status Bar
            Constraint::Length(input_height), // Input
            Constraint::Length(1),            // Mode Bar
        ];

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);

        let messages_area = chunks.first().copied().unwrap_or(area);
        let status_bar_area = chunks.get(1).copied().unwrap_or(area);
        let input_area = chunks.get(2).copied().unwrap_or(area);
        let mode_bar_area = chunks.get(3).copied().unwrap_or(area);

        app.view(&Id::Chat, f, messages_area);

        // Status Bar rendering
        let is_streaming = chat_ref!(app).is_some_and(ChatComponent::is_streaming);
        let active_steps = InputComponent::active_steps(is_streaming);
        let status_bar = StatusBar::new(active_steps, is_streaming, input.spinner_frame);
        f.render_widget(status_bar, status_bar_area);

        let mode_bar = ModeBar::new(input.mode, input.provider.model.clone());
        f.render_widget(mode_bar, mode_bar_area);

        input.render(f, input_area, is_streaming);
    })?;
    Ok(())
}
