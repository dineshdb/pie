use std::future::Future;
use tuirealm::application::Application;
use tuirealm::event::{Event, KeyEvent};
use tuirealm::listener::{Poll, PortResult};

/// Component identifiers for tuirealm Application.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Id {
    Chat,
}

/// Custom events bridging tokio streaming into tuirealm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    Delta(String),
    Done(String),
    Error(String),
    ToolCall {
        name: String,
        display: String,
        output: String,
    },
    PlanUpdate,
    ModelList(Vec<String>),
}

/// Messages returned by `AppComponent::on()` — processed in the update function.
#[derive(Debug, PartialEq, Clone)]
pub enum Msg {
    Submit(String),
    Quit,
    CloseHelp,

    StreamDone(String),
    StreamError(String),

    KeyboardToInput(KeyEvent),

    /// Explicitly scroll the chat view (e.g. mouse).
    ScrollChat(i16),
    /// Keyboard scroll request (may fall back to history).
    KeyboardScroll(i16),
    /// Copy selected text to clipboard.
    CopySelection,
    /// Fetch models for a specific provider.
    FetchModels(String),
    /// Switch provider and model.
    SwitchProviderAndModel(String, String),
    /// Trigger a UI redraw.
    Redraw,
}

/// Bridges tokio mpsc events into tuirealm's `SyncPort` system.
pub struct StreamPort {
    rx: tokio::sync::mpsc::UnboundedReceiver<StreamEvent>,
}

impl StreamPort {
    pub fn new(rx: tokio::sync::mpsc::UnboundedReceiver<StreamEvent>) -> Self {
        Self { rx }
    }
}

impl Poll<StreamEvent> for StreamPort {
    fn poll(&mut self) -> PortResult<Option<Event<StreamEvent>>> {
        match self.rx.try_recv() {
            Ok(ev) => Ok(Some(Event::User(ev))),
            Err(_) => Ok(None),
        }
    }
}

/// Type alias for the tuirealm Application used throughout the app.
pub type App = Application<Id, Msg, StreamEvent>;

/// Run an async future synchronously from the TUI render loop.
///
/// Uses `block_in_place` to avoid blocking the tokio runtime, then
/// `block_on` to drive the future to completion.
pub fn run_sync<F, T>(fut: F) -> T
where
    F: Future<Output = T>,
{
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
}
