use crate::db::DbPool;
use crate::error::{AppError, Result};
use agentsdk::core::messages::{self, Message, Messages};
use agentsdk::openai::api::ChatCompletionRequestUserMessageContent;
use serde::{Deserialize, Serialize};
use sqlx::Row as _;
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
    pub call_id: String,
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
    #[allow(dead_code)]
    pub parent_id: Option<String>,
    cache: Vec<HistoryEntry>,
}

impl Session {
    pub async fn create(pool: Arc<DbPool>) -> Result<Self> {
        Self::create_with_parent(pool, None).await
    }

    pub async fn create_with_parent(pool: Arc<DbPool>, parent_id: Option<&str>) -> Result<Self> {
        let id = SessionId::new();
        let cwd = std::env::current_dir()?.to_string_lossy().to_string();
        let id_str = id.to_string();
        sqlx::query("INSERT OR IGNORE INTO sessions (id, cwd, parent_id) VALUES (?, ?, ?)")
            .bind(&id_str)
            .bind(&cwd)
            .bind(parent_id)
            .execute(&*pool)
            .await?;
        Self::load(pool, id).await
    }

    pub async fn create_with_id(pool: Arc<DbPool>, id: SessionId) -> Result<Self> {
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

    pub async fn load(pool: Arc<DbPool>, session_id: SessionId) -> Result<Self> {
        let sid = session_id.to_string();
        let row = sqlx::query("SELECT parent_id FROM sessions WHERE id = ?")
            .bind(&sid)
            .fetch_optional(&*pool)
            .await?;
        let Some(row) = row else {
            return Err(AppError::NotFound(session_id.to_string()));
        };
        let parent_id: Option<String> = row.try_get("parent_id")?;
        let mut session = Self {
            id: session_id,
            pool,
            parent_id,
            cache: Vec::new(),
        };
        session.rebuild_cache().await?;
        Ok(session)
    }

    pub async fn find_latest_for_cwd(pool: Arc<DbPool>, cwd: &str) -> Result<Option<Self>> {
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

    async fn add_entry(&mut self, entry: HistoryEntry) -> Result<()> {
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

    pub async fn add_user(&mut self, content: &str) -> Result<()> {
        self.add_entry(HistoryEntry::User(content.to_string()))
            .await
    }

    #[allow(dead_code)]
    pub async fn add_assistant(&mut self, content: &str) -> Result<()> {
        self.add_entry(HistoryEntry::Assistant(content.to_string()))
            .await
    }

    pub async fn add_system(&mut self, content: &str) -> Result<()> {
        self.add_entry(HistoryEntry::System(content.to_string()))
            .await
    }

    pub async fn rebuild_cache(&mut self) -> Result<()> {
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

    async fn insert_message(&self, session_id: &str, role: &str, content: &str) -> Result<()> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let ts = now_ms * 1000;
        sqlx::query!(
            "INSERT INTO messages (session_id, ts, role, content) VALUES (?, ?, ?, ?)",
            session_id,
            ts,
            role,
            content,
        )
        .execute(&*self.pool)
        .await?;
        Ok(())
    }

    async fn update_tool_output(
        &self,
        session_id: &str,
        call_id: &str,
        output: String,
    ) -> Result<()> {
        // Find existing ToolCall row
        let row = sqlx::query!(
            "SELECT content FROM messages WHERE session_id = ? AND role = 'tool' AND json_extract(content, '$.call_id') = ?",
            session_id,
            call_id
        )
        .fetch_optional(&*self.pool)
        .await?;

        if let Some(row) = row
            && let Ok(mut tc) = serde_json::from_str::<ToolCall>(&row.content)
        {
            tc.output = Some(Ok(serde_json::Value::String(output)));
            let new_content = serde_json::to_string(&tc).unwrap_or(row.content);
            sqlx::query!(
                "UPDATE messages SET content = ? WHERE session_id = ? AND role = 'tool' AND json_extract(content, '$.call_id') = ?",
                new_content,
                session_id,
                call_id
            )
            .execute(&*self.pool)
            .await?;
        }
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
                HistoryEntry::System(c) => vec![messages::system(c)],
            })
            .collect()
    }

    pub async fn sync_from_messages(&mut self, messages: &[Message]) -> Result<()> {
        let sid = self.id.to_string();

        sqlx::query("DELETE FROM messages WHERE session_id = ?")
            .bind(&sid)
            .execute(&*self.pool)
            .await?;

        for msg in messages {
            self.push_to_db(&sid, msg).await?;
        }

        sqlx::query("UPDATE sessions SET updated_at = unixepoch('subsec') * 1000 WHERE id = ?")
            .bind(&sid)
            .execute(&*self.pool)
            .await?;

        self.rebuild_cache().await
    }

    async fn push_to_db(&self, session_id: &str, message: &Message) -> Result<()> {
        match message {
            Message::SystemMessage(_) | Message::FunctionMessage(_) => {} // system prompt is not persisted to user history
            Message::UserMessage(u) => {
                let content = match &u.content {
                    Some(ChatCompletionRequestUserMessageContent::String(s)) => s.clone(),
                    _ => String::new(),
                };
                self.insert_message(session_id, "user", &content).await?;
            }
            Message::AssistantMessage(a) => {
                if let Some(content) = &a.content
                    && !content.is_empty()
                {
                    self.insert_message(session_id, "assistant", content)
                        .await?;
                }
                if let Some(calls) = &a.tool_calls {
                    for call in calls {
                        let tc = ToolCall {
                            call_id: call.id.clone(),
                            tool_name: call.function.name.clone(),
                            params: serde_json::from_str(&call.function.arguments)
                                .unwrap_or(serde_json::Value::Null),
                            output: None,
                        };
                        let content = serde_json::to_string(&tc).unwrap_or_default();
                        self.insert_message(session_id, "tool", &content).await?;
                    }
                }
            }
            Message::ToolMessage(t) => {
                if let Some(content) = &t.content {
                    self.update_tool_output(session_id, &t.tool_call_id, content.clone())
                        .await?;
                }
            }
        }
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
