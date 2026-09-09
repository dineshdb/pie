use pie_core::session::Role;

/// Why this message exists — controls rendering order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    /// Regular message rendered in insertion order.
    Normal,
    /// The main LLM response — always rendered last (after tool calls).
    Response,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    pub kind: MessageKind,
    /// Set only on a pending tool-call message — pairs it with the
    /// completion event that fills in its result line.
    pub tool_id: Option<String>,
}

impl ChatMessage {
    pub fn user(content: &str) -> Self {
        Self {
            role: Role::User,
            content: content.to_string(),
            kind: MessageKind::Normal,
            tool_id: None,
        }
    }

    pub fn assistant(content: &str) -> Self {
        Self {
            role: Role::Assistant,
            content: content.to_string(),
            kind: MessageKind::Normal,
            tool_id: None,
        }
    }

    /// Create a streaming response placeholder — rendered last, content updated via deltas.
    pub fn response() -> Self {
        Self {
            role: Role::Assistant,
            content: String::new(),
            kind: MessageKind::Response,
            tool_id: None,
        }
    }

    pub fn system(content: &str) -> Self {
        Self {
            role: Role::System,
            content: content.to_string(),
            kind: MessageKind::Normal,
            tool_id: None,
        }
    }

    pub fn tool(content: &str) -> Self {
        Self {
            role: Role::Tool,
            content: content.to_string(),
            kind: MessageKind::Normal,
            tool_id: None,
        }
    }

    /// A tool call before its result is known — `id` pairs it with the
    /// completion event that appends the result line to `content`.
    pub fn tool_call(id: &str, content: &str) -> Self {
        Self {
            role: Role::Tool,
            content: content.to_string(),
            kind: MessageKind::Normal,
            tool_id: Some(id.to_string()),
        }
    }

    pub fn set_content(&mut self, content: String) {
        self.content = content;
    }

    pub fn finalize_response(&mut self) {
        self.kind = MessageKind::Normal;
    }

    #[cfg(test)]
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
            tool_id: None,
        }
    }
}
