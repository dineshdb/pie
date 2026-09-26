//! The in-process agent's conversation memory. Stateless pie: the
//! durable transcript lives in the gateway's agent filesystem (a2acp
//! `context_history`); this is the engine's working set for the
//! process's lifetime — hydrated from the gateway at startup, held on
//! `Arc` so every clone (the persistence plugin, per-turn agents)
//! shares one history.

use crate::error::{AppError, Result};
use agentsdk::core::messages::{self, Messages};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex, PoisonError};
use strum::{AsRefStr, EnumString, IntoStaticStr};
use uuid::Uuid;

fn lock<T>(guard: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    guard.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::Display,
    EnumString,
    IntoStaticStr,
    AsRefStr,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    System,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolCall {
    pub call_id: String,
    pub tool_name: String,
    pub params: serde_json::Value,
    pub output: Option<Result<serde_json::Value, serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryEntry {
    /// Arrival order within the session — the exact cursor for
    /// partial reads.
    pub id: i64,
    /// Write time in microseconds since the epoch.
    pub ts: i64,
    pub role: Role,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "role", content = "content", rename_all = "lowercase")]
pub enum HistoryContent {
    User(String),
    Assistant(String),
    System(String),
    Tool(ToolCall),
}

impl HistoryEntry {
    pub fn role(&self) -> Role {
        self.role
    }

    pub fn content(&self) -> String {
        self.content.clone()
    }

    pub fn to_history_content(&self) -> Result<HistoryContent> {
        match self.role {
            Role::User => Ok(HistoryContent::User(self.content.clone())),
            Role::Assistant => Ok(HistoryContent::Assistant(self.content.clone())),
            Role::System => Ok(HistoryContent::System(self.content.clone())),
            Role::Tool => serde_json::from_str(&self.content)
                .map(HistoryContent::Tool)
                .map_err(AppError::from),
        }
    }
}

impl Role {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(String);

impl SessionId {
    pub fn new() -> Self {
        let full_uuid = Uuid::now_v7().to_string();
        let id = full_uuid.split('-').next_back().unwrap_or_default();
        let short = if id.len() >= 6 {
            &id[id.len() - 6..]
        } else {
            id
        };
        Self(short.to_string())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<String> for SessionId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for SessionId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

/// The history behind an [`Arc`]: clones of a [`Session`] share one
/// transcript, the way the database used to be the shared truth.
#[derive(Debug, Default)]
struct SharedHistory {
    entries: Vec<HistoryEntry>,
    next_id: i64,
}

/// A conversation's in-memory memory.
#[derive(Debug, Clone)]
pub struct Session {
    pub id: SessionId,
    /// The workspace this session runs in.
    pub cwd: String,
    history: Arc<Mutex<SharedHistory>>,
}

impl Session {
    /// A fresh session rooted at `cwd`. Explicit by design: the process
    /// working directory means nothing once one process serves
    /// concurrent runs in different directories.
    pub fn new(cwd: &std::path::Path) -> Self {
        Self {
            id: SessionId::new(),
            cwd: cwd.to_string_lossy().to_string(),
            history: Arc::new(Mutex::new(SharedHistory::default())),
        }
    }

    /// A session pre-seeded with a transcript — the stateless startup
    /// hydration from the gateway's `context_history`.
    pub fn with_history(id: SessionId, cwd: &std::path::Path, entries: Vec<HistoryEntry>) -> Self {
        let session = Self::new(cwd).with_id(id);
        session.hydrate(entries);
        session
    }

    /// Keep this session's id (the gateway's resume handle).
    #[must_use]
    pub fn with_id(mut self, id: SessionId) -> Self {
        self.id = id;
        self
    }

    /// Replace the shared history with a hydrated transcript — the
    /// stateless startup: every clone (engine, plugin) sees it.
    pub fn hydrate(&self, entries: Vec<HistoryEntry>) {
        let mut history = lock(&self.history);
        history.next_id = entries.iter().map(|entry| entry.id).max().unwrap_or(0) + 1;
        history.entries = entries;
    }

    pub fn history_entries(&self) -> Vec<HistoryEntry> {
        lock(&self.history).entries.clone()
    }

    fn add_entry(&self, role: Role, content: String) -> i64 {
        // True microseconds, not ms*1000: two messages written within the
        // same millisecond must still carry distinct timestamps, or a
        // `since` cursor would skip one of them.
        let ts = chrono::Utc::now().timestamp_micros();
        let mut history = lock(&self.history);
        let id = history.next_id;
        history.next_id += 1;
        history.entries.push(HistoryEntry {
            id,
            ts,
            role,
            content,
        });
        id
    }

    pub fn add_user(&self, content: &str) -> i64 {
        self.add_entry(Role::User, content.to_string())
    }

    pub fn add_assistant(&self, content: &str) -> i64 {
        self.add_entry(Role::Assistant, content.to_string())
    }

    pub fn add_system(&self, content: &str) -> i64 {
        self.add_entry(Role::System, content.to_string())
    }

    pub fn add_tool_call(&self, tc: &ToolCall) -> i64 {
        let content = serde_json::to_string(tc).unwrap_or_default();
        self.add_entry(Role::Tool, content)
    }

    /// Convert this session's history entries into agentsdk `Message`s.
    pub fn to_messages(&self) -> Messages {
        lock(&self.history)
            .entries
            .iter()
            .flat_map(|entry| match entry.to_history_content() {
                Ok(HistoryContent::User(c)) => vec![messages::user(c)],
                Ok(HistoryContent::Assistant(c)) => vec![messages::assistant(c)],
                Ok(HistoryContent::Tool(tc)) => {
                    let mut msgs = Vec::new();
                    let call_id = tc.call_id.clone();
                    msgs.push(messages::assistant_tool_call(
                        &tc.tool_name,
                        &call_id,
                        &tc.params,
                    ));
                    if let Some(res) = &tc.output {
                        let content = match res {
                            Ok(v) | Err(v) => v.to_string(),
                        };
                        msgs.push(messages::tool(content, &call_id));
                    }
                    msgs
                }
                Ok(HistoryContent::System(c)) => vec![messages::system(c)],
                Err(e) => {
                    tracing::warn!("Failed to convert history entry {}: {}", entry.id, e);
                    vec![]
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_append_in_order_and_read_back() {
        let session = Session::new(std::path::Path::new("/test"));
        session.add_user("hello");
        session.add_assistant("hi there");

        let entries = session.history_entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].role(), Role::User);
        assert_eq!(entries[0].content(), "hello");
        assert_eq!(entries[1].role(), Role::Assistant);
        assert_eq!(entries[1].content(), "hi there");
    }

    /// The persistence plugin and the engine hold different clones —
    /// they must see one shared transcript, or the engine's memory
    /// loses what the plugin wrote.
    #[test]
    fn clones_share_one_history() {
        let session = Session::new(std::path::Path::new("/test"));
        let plugin_copy = session.clone();
        plugin_copy.add_assistant("from the plugin");

        let entries = session.history_entries();
        assert_eq!(entries.len(), 1, "the clone wrote into the shared history");
        assert_eq!(entries[0].content(), "from the plugin");
    }

    #[test]
    fn tool_calls_round_trip_into_messages() {
        let session = Session::new(std::path::Path::new("/test"));
        session.add_tool_call(&ToolCall {
            call_id: "c-1".into(),
            tool_name: "bash".into(),
            params: serde_json::json!({ "cmd": "ls" }),
            output: Some(Ok(serde_json::json!({ "files": 3 }))),
        });

        let messages = session.to_messages();
        assert_eq!(messages.len(), 2, "tool call + tool result");
    }

    #[test]
    fn with_history_hydrates_and_continues_the_sequence() {
        let seeded = vec![
            HistoryEntry {
                id: 1,
                ts: 1,
                role: Role::User,
                content: "old question".into(),
            },
            HistoryEntry {
                id: 2,
                ts: 2,
                role: Role::Assistant,
                content: "old answer".into(),
            },
        ];
        let session = Session::with_history(
            SessionId::from("c-1".to_string()),
            std::path::Path::new("/test"),
            seeded,
        );
        session.add_user("new question");

        let entries = session.history_entries();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[2].id, 3, "the sequence continues past the seed");
        assert_eq!(entries[2].content(), "new question");
    }
}
