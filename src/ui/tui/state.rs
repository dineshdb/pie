use crate::session::Role;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    Idle,
    Active,
}

pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    pub is_streaming: bool,
}

impl ChatMessage {
    pub fn user(content: &str) -> Self {
        Self {
            role: Role::User,
            content: content.to_string(),
            is_streaming: false,
        }
    }

    pub fn assistant(content: &str) -> Self {
        Self {
            role: Role::Assistant,
            content: content.to_string(),
            is_streaming: false,
        }
    }

    pub fn assistant_streaming(content: &str) -> Self {
        Self {
            role: Role::Assistant,
            content: content.to_string(),
            is_streaming: true,
        }
    }

    pub fn system(content: &str) -> Self {
        Self {
            role: Role::System,
            content: content.to_string(),
            is_streaming: false,
        }
    }

    pub fn tool(content: &str) -> Self {
        Self {
            role: Role::Tool,
            content: content.to_string(),
            is_streaming: false,
        }
    }

    pub fn set_content(&mut self, content: String) {
        self.content = content;
    }
}

impl From<Role> for ChatMessage {
    fn from(role: Role) -> Self {
        Self {
            role,
            content: String::new(),
            is_streaming: false,
        }
    }
}
