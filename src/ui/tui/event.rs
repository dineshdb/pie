use crossterm::event::KeyEvent;

#[derive(Debug, Clone)]
pub enum AppEvent {
    Key(KeyEvent),
    ScrollUp,
    ScrollDown,
    StreamDelta(String),
    StreamDone(String),
    StreamError(String),
    ToolCallStart { name: String, params: String },
    ToolCallEnd { output: String },
    Resize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleResult {
    Continue,
    Quit,
    Submit,
}
