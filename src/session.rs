use crate::db::DbPool;
use crate::error::{AppError, Result};
use agentsdk::core::messages::{self, Messages};
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
pub struct HistoryEntry {
    pub id: i64,
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

impl<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> for HistoryEntry {
    fn from_row(row: &'r sqlx::sqlite::SqliteRow) -> Result<Self, sqlx::Error> {
        let id: i64 = row.try_get("id")?;
        let role_str: &str = row.try_get("role")?;
        let role: Role = Role::from_str(role_str)
            .map_err(|e| sqlx::Error::Decode(format!("unknown role: {e}").into()))?;
        let content: String = row.try_get("content")?;
        Ok(Self { id, role, content })
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

    async fn add_entry(&mut self, role: Role, content: String) -> Result<i64> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let sid = self.id.to_string();
        let ts = now_ms * 1000;
        let role_str = role.as_str();

        let row: (i64,) = sqlx::query_as(
            "INSERT INTO messages (session_id, ts, role, content) VALUES (?, ?, ?, ?) RETURNING id",
        )
        .bind(&sid)
        .bind(ts)
        .bind(role_str)
        .bind(&content)
        .fetch_one(&*self.pool)
        .await?;
        let id = row.0;

        sqlx::query("UPDATE sessions SET updated_at = unixepoch('subsec') * 1000 WHERE id = ?")
            .bind(&sid)
            .execute(&*self.pool)
            .await?;

        self.cache.push(HistoryEntry { id, role, content });
        Ok(id)
    }

    pub async fn add_user(&mut self, content: &str) -> Result<i64> {
        self.add_entry(Role::User, content.to_string()).await
    }

    pub async fn add_assistant(&mut self, content: &str) -> Result<i64> {
        self.add_entry(Role::Assistant, content.to_string()).await
    }

    pub async fn add_system(&mut self, content: &str) -> Result<i64> {
        self.add_entry(Role::System, content.to_string()).await
    }

    pub async fn add_tool_call(&mut self, tc: &ToolCall) -> Result<i64> {
        let content = serde_json::to_string(tc).unwrap_or_default();
        self.add_entry(Role::Tool, content).await
    }

    pub async fn rebuild_cache(&mut self) -> Result<()> {
        let sid = self.id.to_string();
        let rows = sqlx::query_as::<_, HistoryEntry>(
            "SELECT id, role, content FROM messages WHERE session_id = ? AND compacted = 0 ORDER BY id",
        )
        .bind(&sid)
        .fetch_all(&*self.pool)
        .await?;

        self.cache = rows;
        Ok(())
    }

    pub async fn update_tool_output_by_id(&mut self, id: i64, output: String) -> Result<()> {
        let sid = self.id.to_string();

        let row: Option<(String,)> = sqlx::query_as(
            "SELECT content FROM messages WHERE id = ? AND session_id = ? AND role = 'tool'",
        )
        .bind(id)
        .bind(&sid)
        .fetch_optional(&*self.pool)
        .await?;

        if let Some((content,)) = row
            && let Ok(mut tc) = serde_json::from_str::<ToolCall>(&content)
        {
            tc.output = Some(Ok(serde_json::Value::String(output)));
            let new_content = serde_json::to_string(&tc).unwrap_or(content);
            sqlx::query("UPDATE messages SET content = ? WHERE id = ?")
                .bind(new_content)
                .bind(id)
                .execute(&*self.pool)
                .await?;

            // Update cache
            if let Some(entry) = self.cache.iter_mut().find(|e| e.id == id) {
                entry.content = serde_json::to_string(&tc).unwrap_or(entry.content.clone());
            }
        }
        Ok(())
    }

    /// Convert this session's history entries into agentsdk `Message`s.
    pub fn to_messages(&self) -> Messages {
        self.cache
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
    use crate::db;

    async fn pool() -> anyhow::Result<Arc<DbPool>> {
        Ok(Arc::new(db::create_test_pool().await?))
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
}
