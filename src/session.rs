use crate::db::DbPool;
use agentsdk::AgentSdkError;
use agentsdk::core::history::HistoryStore;
use agentsdk::core::messages::{self, Message, Messages};
use agentsdk::openai::api::ChatCompletionRequestUserMessageContent;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::str::FromStr;
use std::sync::Arc;
use strum::{AsRefStr, EnumString, IntoStaticStr};
use uuid::Uuid;

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
    pub call_id: Uuid,
    pub tool_name: String,
    pub params: serde_json::Value,
    pub output: Option<Result<serde_json::Value, serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "role", content = "content", rename_all = "lowercase")]
pub enum HistoryEntry {
    User(String),
    Assistant(String),
    System(String),
    Tool(ToolCall),
}

impl HistoryEntry {
    pub fn role(&self) -> Role {
        match self {
            Self::User(_) => Role::User,
            Self::Assistant(_) => Role::Assistant,
            Self::System(_) => Role::System,
            Self::Tool(_) => Role::Tool,
        }
    }

    pub fn content(&self) -> String {
        match self {
            Self::User(c) | Self::Assistant(c) | Self::System(c) => c.clone(),
            Self::Tool(info) => serde_json::to_string(info).unwrap_or_default(),
        }
    }
}

impl Role {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

impl<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> for HistoryEntry {
    fn from_row(row: &'r sqlx::sqlite::SqliteRow) -> Result<Self, sqlx::Error> {
        let role_str: &str = row.try_get("role")?;
        let role: Role = Role::from_str(role_str)
            .map_err(|e| sqlx::Error::Decode(format!("unknown role: {e}").into()))?;
        let content: String = row.try_get("content")?;
        match role {
            Role::User => Ok(Self::User(content)),
            Role::Assistant => Ok(Self::Assistant(content)),
            Role::System => Ok(Self::System(content)),
            Role::Tool => serde_json::from_str(&content)
                .map(Self::Tool)
                .map_err(|e| sqlx::Error::Decode(Box::new(e))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

    pub fn subagent(&self, agent_name: &str) -> Self {
        let suffix = format!("-{agent_name}");
        if self.0.ends_with(&suffix) {
            self.clone()
        } else {
            Self(format!("{}{}", self.0, suffix))
        }
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

// ── Session ────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct Session {
    pub id: SessionId,
    pub pool: Arc<DbPool>,
    cache: Vec<HistoryEntry>,
}

impl Session {
    pub async fn create(pool: Arc<DbPool>) -> anyhow::Result<Self> {
        let id = SessionId::new();
        Self::create_with_id(pool, id).await
    }

    pub async fn create_with_id(pool: Arc<DbPool>, id: SessionId) -> anyhow::Result<Self> {
        let cwd = std::env::current_dir()?.to_string_lossy().to_string();
        let id_str = id.to_string();
        sqlx::query!(
            "INSERT OR IGNORE INTO sessions (id, cwd) VALUES (?, ?)",
            id_str,
            cwd
        )
        .execute(&*pool)
        .await?;
        Self::load(pool, id).await
    }

    pub async fn load(pool: Arc<DbPool>, session_id: SessionId) -> anyhow::Result<Self> {
        let sid = session_id.to_string();
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?)")
            .bind(&sid)
            .fetch_one(&*pool)
            .await?;
        if !exists {
            anyhow::bail!("Session {sid} not found");
        }
        let mut session = Self {
            id: session_id,
            pool,
            cache: Vec::new(),
        };
        session.rebuild_cache().await?;
        Ok(session)
    }

    pub async fn find_latest_for_cwd(pool: Arc<DbPool>, cwd: &str) -> anyhow::Result<Option<Self>> {
        let id_str: Option<String> = sqlx::query_scalar(
            "SELECT id FROM sessions WHERE cwd = ? ORDER BY updated_at DESC LIMIT 1",
        )
        .bind(cwd)
        .fetch_optional(&*pool)
        .await?;
        match id_str {
            Some(sid) => Ok(Some(Self::load(pool, SessionId::from(sid)).await?)),
            None => Ok(None),
        }
    }

    pub fn history_entries(&self) -> &[HistoryEntry] {
        &self.cache
    }

    pub fn pool(&self) -> &Arc<DbPool> {
        &self.pool
    }

    async fn add_entry(&mut self, entry: HistoryEntry) -> anyhow::Result<()> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let sid = self.id.to_string();
        let ts = now_ms * 1000;
        let role_str = entry.role().as_str();
        let content = entry.content();

        sqlx::query!(
            "INSERT INTO messages (session_id, ts, role, content) VALUES (?, ?, ?, ?)",
            sid,
            ts,
            role_str,
            content,
        )
        .execute(&*self.pool)
        .await?;

        sqlx::query!(
            "UPDATE sessions SET updated_at = unixepoch('subsec') * 1000 WHERE id = ?",
            sid,
        )
        .execute(&*self.pool)
        .await?;

        self.cache.push(entry);
        Ok(())
    }

    pub async fn add_user(&mut self, content: &str) -> anyhow::Result<()> {
        self.add_entry(HistoryEntry::User(content.to_string()))
            .await
    }

    #[allow(dead_code)]
    pub async fn add_assistant(&mut self, content: &str) -> anyhow::Result<()> {
        self.add_entry(HistoryEntry::Assistant(content.to_string()))
            .await
    }

    #[allow(dead_code)]
    pub async fn add_system(&mut self, content: &str) -> anyhow::Result<()> {
        self.add_entry(HistoryEntry::System(content.to_string()))
            .await
    }

    async fn add_tool(&mut self, info: ToolCall) -> anyhow::Result<()> {
        self.add_entry(HistoryEntry::Tool(info)).await
    }

    pub async fn record_tool_call(&mut self, info: ToolCall) -> anyhow::Result<ToolCall> {
        // Try to find an existing entry with the same call_id.
        let Some(existing) = self.cache.iter_mut().rev().find_map(|e| match e {
            HistoryEntry::Tool(t) if t.call_id == info.call_id => Some(t),
            _ => None,
        }) else {
            // New tool call — persist and return.
            let merged = info.clone();
            self.add_tool(info).await?;
            return Ok(merged);
        };

        // Merge fields into the existing entry.
        if !info.params.is_null() {
            existing.params = info.params;
        }
        if info.output.is_some() {
            existing.output = info.output;
        }

        let sid = self.id.to_string();
        let entry = HistoryEntry::Tool(existing.clone());
        let content = entry.content();
        let call_id_str = existing.call_id.to_string();

        sqlx::query!(
            "UPDATE messages SET content = ? WHERE session_id = ? AND role = 'tool' AND json_extract(content, '$.call_id') = ?",
            content,
            sid,
            call_id_str
        )
        .execute(&*self.pool)
        .await?;

        Ok(existing.clone())
    }

    pub async fn rebuild_cache(&mut self) -> anyhow::Result<()> {
        let sid = self.id.to_string();
        let rows = sqlx::query_as::<_, HistoryEntry>(
            "SELECT role, content FROM messages WHERE session_id = ? AND compacted = 0 ORDER BY id",
        )
        .bind(&sid)
        .fetch_all(&*self.pool)
        .await?;

        self.cache = rows;
        Ok(())
    }

    /// Convert this session's history entries into agentsdk `Message`s.
    pub fn to_messages(&self) -> Messages {
        self.cache
            .iter()
            .flat_map(|entry| match entry {
                HistoryEntry::User(c) => vec![messages::user(c)],
                HistoryEntry::Assistant(c) => vec![messages::assistant(c)],
                HistoryEntry::Tool(tc) => {
                    let mut msgs = Vec::new();
                    let call_id = tc.call_id.to_string();
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
                HistoryEntry::System(c) => vec![messages::system(c)],
            })
            .collect()
    }
}

#[async_trait]
impl HistoryStore for Session {
    async fn load(&self, _id: &str) -> agentsdk::error::Result<Messages> {
        Ok(self.to_messages())
    }

    async fn push(&self, id: &str, message: Message) -> agentsdk::error::Result<()> {
        // Session is behind Arc<...> in practice, but HistoryStore requires &self.
        // We use the pool directly to append.
        let pool = self.pool.clone();
        let role_str = match &message {
            Message::SystemMessage(_) => "system",
            Message::UserMessage(_) => "user",
            Message::AssistantMessage(a) if a.tool_calls.is_some() => "tool",
            Message::AssistantMessage(_) | Message::FunctionMessage(_) => "assistant",
            Message::ToolMessage(_) => "tool",
        };
        let content = match &message {
            Message::SystemMessage(s) => s.content.clone().unwrap_or_default(),
            Message::UserMessage(u) => match &u.content {
                Some(ChatCompletionRequestUserMessageContent::String(s)) => s.clone(),
                _ => String::new(),
            },
            Message::AssistantMessage(a) => {
                if a.tool_calls.is_some() {
                    // Serialize tool call info for the tool HistoryEntry
                    let tc = ToolCall {
                        call_id: Uuid::parse_str(
                            a.tool_calls
                                .as_ref()
                                .and_then(|calls| calls.first().map(|c| c.id.as_str()))
                                .unwrap_or_default(),
                        )
                        .unwrap_or_else(|_| Uuid::now_v7()),
                        tool_name: a
                            .tool_calls
                            .as_ref()
                            .and_then(|calls| calls.first().map(|c| c.function.name.clone()))
                            .unwrap_or_default(),
                        params: a
                            .tool_calls
                            .as_ref()
                            .and_then(|calls| {
                                calls
                                    .first()
                                    .and_then(|c| serde_json::from_str(&c.function.arguments).ok())
                            })
                            .unwrap_or(serde_json::Value::Null),
                        output: None,
                    };
                    serde_json::to_string(&tc).unwrap_or_default()
                } else {
                    a.content.clone().unwrap_or_default()
                }
            }
            Message::ToolMessage(t) => t.content.clone().unwrap_or_default(),
            Message::FunctionMessage(_) => String::new(),
        };
        let now_ms = chrono::Utc::now().timestamp_millis();
        let ts = now_ms * 1000;
        sqlx::query!(
            "INSERT INTO messages (session_id, ts, role, content) VALUES (?, ?, ?, ?)",
            id,
            ts,
            role_str,
            content,
        )
        .execute(&*pool)
        .await
        .map_err(|e| AgentSdkError::IoError(std::io::Error::other(e.to_string())))?;

        sqlx::query!(
            "UPDATE sessions SET updated_at = unixepoch('subsec') * 1000 WHERE id = ?",
            id,
        )
        .execute(&*pool)
        .await
        .map_err(|e| AgentSdkError::IoError(std::io::Error::other(e.to_string())))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    async fn pool() -> anyhow::Result<Arc<DbPool>> {
        Ok(Arc::new(db::create_test_pool().await?))
    }

    #[tokio::test]
    async fn create_session() -> anyhow::Result<()> {
        let pool = pool().await?;
        let session = Session::create(pool.clone()).await?;
        assert!(session.history_entries().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn load_nonexistent_session() -> anyhow::Result<()> {
        let pool = pool().await?;
        let result = Session::load(pool, SessionId::new()).await;
        assert!(result.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn find_latest_for_cwd_returns_most_recent() -> anyhow::Result<()> {
        let pool = pool().await?;
        let cwd = std::env::current_dir()?.to_string_lossy().to_string();

        let _s1 = Session::create(pool.clone()).await?;
        let mut s2 = Session::create(pool.clone()).await?;
        s2.add_user("ensure updated_at is later").await?;

        let found = Session::find_latest_for_cwd(pool, &cwd)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no session found"))?;
        assert_eq!(found.id, s2.id);
        Ok(())
    }

    #[tokio::test]
    async fn find_latest_for_cwd_returns_none_when_empty() -> anyhow::Result<()> {
        let pool = pool().await?;
        let result = Session::find_latest_for_cwd(pool, "/nonexistent").await?;
        assert!(result.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn add_user_and_assistant() -> anyhow::Result<()> {
        let pool = pool().await?;
        let mut session = Session::create(pool.clone()).await?;
        session.add_user("hello").await?;
        session.add_assistant("hi there").await?;

        let entries = session.history_entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].role(), Role::User);
        assert_eq!(entries[0].content(), "hello");
        assert_eq!(entries[1].role(), Role::Assistant);
        assert_eq!(entries[1].content(), "hi there");
        Ok(())
    }

    #[tokio::test]
    async fn history_persists_after_load() -> anyhow::Result<()> {
        let pool = pool().await?;
        let id = {
            let mut session = Session::create(pool.clone()).await?;
            session.add_user("first").await?;
            session.add_assistant("second").await?;
            session.id
        };

        let loaded = Session::load(pool, id).await?;
        let entries = loaded.history_entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].content(), "first");
        assert_eq!(entries[1].content(), "second");
        Ok(())
    }
}
