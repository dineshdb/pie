use crate::session::Role;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    Idle,
    Active,
}

/// Why this message exists — controls rendering order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    /// Regular message rendered in insertion order.
    Normal,
    /// The main LLM response — always rendered last (after tool calls).
    Response,
}

pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    pub kind: MessageKind,
}

impl ChatMessage {
    pub fn user(content: &str) -> Self {
        Self {
            role: Role::User,
            content: content.to_string(),
            kind: MessageKind::Normal,
        }
    }

    pub fn assistant(content: &str) -> Self {
        Self {
            role: Role::Assistant,
            content: content.to_string(),
            kind: MessageKind::Normal,
        }
    }

    /// Create a streaming response placeholder — rendered last, content updated via deltas.
    pub fn response() -> Self {
        Self {
            role: Role::Assistant,
            content: String::new(),
            kind: MessageKind::Response,
        }
    }

    pub fn system(content: &str) -> Self {
        Self {
            role: Role::System,
            content: content.to_string(),
            kind: MessageKind::Normal,
        }
    }

    pub fn tool(content: &str) -> Self {
        Self {
            role: Role::Tool,
            content: content.to_string(),
            kind: MessageKind::Normal,
        }
    }

    pub fn set_content(&mut self, content: String) {
        self.content = content;
    }

    pub fn finalize_response(&mut self) {
        self.kind = MessageKind::Normal;
    }

    pub fn is_response(&self) -> bool {
        self.kind == MessageKind::Response
    }
}

impl From<Role> for ChatMessage {
    fn from(role: Role) -> Self {
        Self {
            role,
            content: String::new(),
            kind: MessageKind::Normal,
        }
    }
}
