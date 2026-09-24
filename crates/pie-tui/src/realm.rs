use tuirealm::application::Application;
use tuirealm::event::{Event, KeyEvent};
use tuirealm::listener::{Poll, PortResult};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Id {
    Chat,
}

/// Identifies the conversation the TUI displays — its input-history key
/// and nothing more; turns address the agent by A2A context ids the
/// door client owns.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(pub String);

impl SessionId {
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Identifies a parked permission ask — the A2A task the turn is
/// parked on; the door client answers on that task.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AskId(pub String);

impl std::fmt::Display for AskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The TUI's projection of the A2A event stream: what the chat view
/// renders. Derived from the front door's stream frames — this crate's
/// only window onto the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    Delta(String),
    Done(String),
    Error(String),
    ToolCall {
        /// Pairs a call's pre-execution announcement with its completion —
        /// flattened status lines carry no pairing, so each is its own id.
        id: String,
        name: String,
        display: String,
        output: String,
        /// The call errored — `output` is the reason.
        failed: bool,
    },
    /// The agent needs a permission decision; answer through the door
    /// client (the `INPUT_REQUIRED` convention).
    PermissionAsk {
        id: AskId,
        skill: String,
        permissions: Vec<String>,
    },
    /// The next prompt starts a fresh conversation (`/new`).
    SessionSwitched(SessionId),
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
    /// The conversation moved to a fresh session — reset views onto it.
    SessionSwitched(SessionId),
    /// Trigger a UI redraw.
    Redraw,
    /// Cycle to the next advertised mode (Ctrl+K) — a pending selection
    /// riding the next turn; the handler answers with a notice when the
    /// agent's modes are not known yet.
    ToggleMode,
    /// Answer a parked permission ask.
    AnswerPermission(AskId, bool),
    /// The model picker confirmed an entry (its selection id).
    SelectModel(String),
    /// The theme picker confirmed a palette (its name).
    SelectTheme(&'static str),
}

/// Bridges the A2A event stream into tuirealm's `SyncPort` system.
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
            Ok(event) => Ok(Some(Event::User(event))),
            Err(_) => Ok(None),
        }
    }
}

/// Type alias for the tuirealm Application used throughout the app.
pub type App = Application<Id, Msg, StreamEvent>;
