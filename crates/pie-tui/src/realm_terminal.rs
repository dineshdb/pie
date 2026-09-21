//! tuirealm-based TUI entry point.
//!
//! `ChatComponent` is the active tuirealm component — it routes all keyboard events,
//! delegating editing keys to `InputComponent` via `Msg::KeyboardToInput`.
//!
//! `InputComponent` is held directly (not mounted in the App) since it never receives
//! events through tuirealm's event system — all interaction goes through direct calls.
//!
//! Each frame: drain all events, merge them, then render once. Everything
//! agent-shaped flows through the door client; pie-specific wants with no
//! A2A counterpart answer with a "not available over the bridge" notice
//! rather than fake success (see the gap list in `door`'s docs).

use crate::command::{Command, CommandAction};
use crate::components::chat::{ActiveDialog, ChatComponent};
use crate::components::input::InputComponent;

pub use crate::components::input::ProviderView;
use crate::door::Client;
use crate::notify;
use crate::realm::{App, Id, Msg, SessionId, StreamEvent, StreamPort};
use crate::state::ChatMessage;
use crate::widgets::mode_bar::ModeBar;
use crate::widgets::status_bar::StatusBar;
use anyhow::{Context, Result};
use arboard::Clipboard;
use pie_core::plugin::AgentMode;
use pie_core::plugin::modes::load_mode_file;
use pie_core::registry::Registry;
use pie_core::session::{HistoryEntry, Role};
use std::io::stdout;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tuirealm::application::PollStrategy;
use tuirealm::event::{Key, KeyModifiers};
use tuirealm::listener::EventListenerCfg;
use tuirealm::ratatui::backend::CrosstermBackend;
use tuirealm::ratatui::crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use tuirealm::ratatui::crossterm::execute;
use tuirealm::ratatui::layout::{Constraint, Direction, Layout};

type Terminal = tuirealm::ratatui::Terminal<CrosstermBackend<std::io::Stdout>>;

/// What the TUI needs from the embedding process: the A2A door client,
/// its event stream, and the leaf data for the first render.
#[derive(Debug)]
pub struct TuiDeps {
    /// Sends A2A requests through the front door.
    pub client: Client,
    /// The door's projected event stream — wired into the app's `StreamPort`.
    pub events: tokio::sync::mpsc::UnboundedReceiver<StreamEvent>,
    /// The conversation the TUI displays (input-history key).
    pub session_id: SessionId,
    /// The conversation's transcript so far.
    pub history: Vec<HistoryEntry>,
    /// Provider name/model for display.
    pub provider: ProviderView,
    pub registry: Arc<Registry>,
}

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

/// The one-line notice every bridge gap answers with.
fn bridge_gap(app: &mut App, what: &str) {
    if let Some(chat) = chat_mut!(app) {
        chat.add_message(ChatMessage::system(&format!(
            "{what} — not available over the bridge"
        )));
    }
}

/// Process a single message. Returns `Some(Msg::Quit)` if the app should exit.
fn process_msg(msg: Msg, app: &mut App, input: &mut InputComponent) -> Option<Msg> {
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
                return process_msg(inner, app, input);
            }
        }

        Msg::Submit(text) => {
            return handle_submit(&text, app, input);
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
            notify::turn_complete(query.as_deref());
        }

        Msg::StreamError(err) => {
            if let Some(chat) = chat_mut!(app) {
                chat.stream_error(&err);
            }
            let _ = input.finish_stream();
        }

        Msg::SessionSwitched(session_id) => {
            // The next prompt starts a fresh conversation: clear the
            // views onto it.
            if let Some(chat) = chat_mut!(app) {
                chat.clear_messages();
                chat.add_message(ChatMessage::system("Welcome to pie! Type ? for help."));
            }
            input.reset_session(session_id);
        }

        Msg::ToggleMode => {
            bridge_gap(app, "mode switching");
        }

        Msg::AnswerPermission(id, allow) => {
            input.client.answer_permission(&id, allow);
        }

        _ => {}
    }
    None
}

fn handle_submit(text: &str, app: &mut App, input: &mut InputComponent) -> Option<Msg> {
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
        CommandAction::Model => {
            bridge_gap(app, "model switching");
        }
        CommandAction::Mode(args) => {
            handle_mode_command(args.as_deref(), app);
        }
        CommandAction::Help => {
            if let Some(chat) = chat_mut!(app) {
                chat.set_help_dialog();
            }
        }
        CommandAction::NewSession => {
            // The door drops the conversation; the views reset when the
            // SessionSwitched event arrives.
            input.client.new_session();
        }
        CommandAction::Stream(query) => {
            if let Some(chat) = chat_mut!(app) {
                chat.add_message(ChatMessage::user(&query));
                chat.start_response();
            }
            input.start_stream(&query);
        }
        CommandAction::Shell(command) => {
            // A `!command` escape: shown locally, answered with the gap
            // notice — the bridge has no shell channel.
            if let Some(chat) = chat_mut!(app) {
                chat.add_message(ChatMessage::user(&format!("!{command}")));
            }
            bridge_gap(app, "shell escapes");
        }
        CommandAction::Quit => return Some(Msg::Quit),
    }
    None
}

/// `/mode` without arguments lists pie's modes (local knowledge);
/// switching one is a bridge gap — A2A carries no mode method.
fn handle_mode_command(args: Option<&str>, app: &mut App) {
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
            chat.add_message(ChatMessage::system(&format!(
                "Available modes (switching is not available over the bridge):\n{modes}"
            )));
        }
        return;
    };
    // Validate the name so typos are reported as such, not as bridge gaps.
    match mode_name.parse::<AgentMode>() {
        Ok(_) => bridge_gap(app, "mode switching"),
        Err(e) => {
            if let Some(chat) = chat_mut!(app) {
                chat.add_message(ChatMessage::system(&format!("Error: {e}")));
            }
        }
    }
}

/// Run the TUI until the user quits.
///
/// # Errors
///
/// Returns an error if the terminal cannot be initialized, an event
/// poll fails, or a frame render fails.
pub async fn run_tui(deps: TuiDeps) -> Result<()> {
    let (mut terminal, mut app, mut input) = setup_tui(deps)?;

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
            if let Some(Msg::Quit) = process_msg(msg, &mut app, &mut input) {
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

fn setup_tui(deps: TuiDeps) -> Result<(Terminal, App, InputComponent)> {
    let mut terminal = tuirealm::ratatui::init();
    terminal.clear()?;

    execute!(stdout(), EnableMouseCapture)?;

    let mut messages = vec![ChatMessage::system("Welcome to pie! Type ? for help.")];
    for entry in &deps.history {
        let msg = match entry.role() {
            Role::User => ChatMessage::user(&entry.content()),
            Role::Assistant => ChatMessage::assistant(&entry.content()),
            Role::System => ChatMessage::system(&entry.content()),
            Role::Tool => ChatMessage::tool(&entry.content()),
        };
        messages.push(msg);
    }

    let listener_cfg = EventListenerCfg::<StreamEvent>::default()
        .crossterm_input_listener(Duration::from_millis(10), 3)
        .add_port(
            Box::new(StreamPort::new(deps.events)),
            Duration::from_millis(20),
            1,
        )
        .tick_interval(Duration::from_millis(20));

    let mut app = App::init(listener_cfg);

    let mut input = InputComponent::new(
        deps.client,
        deps.provider,
        deps.session_id,
        deps.registry.clone(),
    );

    app.mount(
        Id::Chat,
        Box::new(ChatComponent::new(messages, deps.registry)),
        vec![],
    )?;
    app.active(&Id::Chat)?;

    render(&mut app, &mut input, &mut terminal)?;

    Ok((terminal, app, input))
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
