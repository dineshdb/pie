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
    /// Row id — strictly increasing, the exact cursor for partial reads.
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

impl<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> for HistoryEntry {
    fn from_row(row: &'r sqlx::sqlite::SqliteRow) -> Result<Self, sqlx::Error> {
        let id: i64 = row.try_get("id")?;
        let ts: i64 = row.try_get("ts")?;
        let role_str: &str = row.try_get("role")?;
        let role: Role = Role::from_str(role_str)
            .map_err(|e| sqlx::Error::Decode(format!("unknown role: {e}").into()))?;
        let content: String = row.try_get("content")?;
        Ok(Self {
            id,
            ts,
            role,
            content,
        })
    }
}

/// Filters for a partial history read — [`Session::history_page`].
#[derive(Debug, Clone, Default)]
pub struct HistoryFilter {
    /// Only messages written in a strictly later millisecond than this
    /// epoch value. Convenient for "what changed since"; for exact
    /// incremental sync prefer [`HistoryFilter::after_id`].
    pub since_ms: Option<i64>,
    /// Only messages with a row id strictly greater than this. Exact and
    /// race-free: pass the last `id` you have already seen.
    pub after_id: Option<i64>,
    /// Cap the result at this many messages, oldest first; `HistoryPage::
    /// truncated` says whether more exist (continue with `after_id`).
    pub limit: Option<u32>,
}

/// A page of history read through [`Session::history_page`].
#[derive(Debug, Clone)]
pub struct HistoryPage {
    pub entries: Vec<HistoryEntry>,
    /// The filter's `limit` cut a longer history short.
    pub truncated: bool,
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

/// A session summary row for listings.
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct SessionSummary {
    pub id: String,
    pub cwd: String,
    pub title: String,
    /// Last activity, milliseconds since the epoch.
    pub updated_at: i64,
}

#[derive(Clone)]
pub struct Session {
    pub id: SessionId,
    pub pool: Arc<DbPool>,
    #[allow(dead_code)]
    pub parent_id: Option<String>,
    /// The workspace this session runs in.
    pub cwd: String,
    cache: Vec<HistoryEntry>,
}

impl Session {
    /// Create a session rooted at `cwd`. Explicit by design: the process
    /// working directory means nothing once one process serves concurrent
    /// runs in different directories.
    pub async fn create(pool: Arc<DbPool>, cwd: &std::path::Path) -> Result<Self> {
        Self::create_with_parent(pool, cwd, None).await
    }

    pub async fn create_with_parent(
        pool: Arc<DbPool>,
        cwd: &std::path::Path,
        parent_id: Option<&str>,
    ) -> Result<Self> {
        let id = SessionId::new();
        let id_str = id.to_string();
        sqlx::query("INSERT OR IGNORE INTO sessions (id, cwd, parent_id) VALUES (?, ?, ?)")
            .bind(&id_str)
            .bind(cwd.to_string_lossy().to_string())
            .bind(parent_id)
            .execute(&*pool)
            .await?;
        let mut session = Self::load(pool, id).await?;
        session.cwd = cwd.to_string_lossy().to_string();
        Ok(session)
    }

    pub async fn load(pool: Arc<DbPool>, session_id: SessionId) -> Result<Self> {
        let sid = session_id.to_string();
        let row = sqlx::query("SELECT parent_id, cwd FROM sessions WHERE id = ?")
            .bind(&sid)
            .fetch_optional(&*pool)
            .await?;
        let Some(row) = row else {
            return Err(AppError::NotFound(session_id.to_string()));
        };
        let parent_id: Option<String> = row.try_get("parent_id")?;
        let cwd: String = row.try_get("cwd")?;
        let mut session = Self {
            id: session_id,
            pool,
            parent_id,
            cwd,
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

    /// Most recently active sessions, newest first.
    pub async fn list(pool: Arc<DbPool>, limit: u32) -> Result<Vec<SessionSummary>> {
        let rows = sqlx::query_as::<_, SessionSummary>(
            "SELECT id, cwd, title, updated_at FROM sessions ORDER BY updated_at DESC LIMIT ?",
        )
        .bind(i64::from(limit))
        .fetch_all(&*pool)
        .await?;
        Ok(rows)
    }

    /// Set the human-readable title (first prompt, task listings).
    pub async fn set_title(&self, title: &str) -> Result<()> {
        let sid = self.id.to_string();
        sqlx::query("UPDATE sessions SET title = ? WHERE id = ?")
            .bind(title)
            .bind(&sid)
            .execute(&*self.pool)
            .await?;
        Ok(())
    }

    pub fn history_entries(&self) -> &[HistoryEntry] {
        &self.cache
    }

    pub fn pool(&self) -> &Arc<DbPool> {
        &self.pool
    }

    async fn add_entry(&mut self, role: Role, content: String) -> Result<i64> {
        // True microseconds, not ms*1000: two messages written within the
        // same millisecond must still carry distinct timestamps, or a
        // `since` cursor would skip one of them.
        let ts = chrono::Utc::now().timestamp_micros();
        let sid = self.id.to_string();
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

        self.cache.push(HistoryEntry {
            id,
            ts,
            role,
            content,
        });
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

    /// Persist one run's LLM usage for bookkeeping. `cost_usd` is `None`
    /// when the model has no configured pricing.
    pub async fn record_usage(
        &self,
        usage: &crate::usage::RunUsage,
        model: &str,
        agent: Option<&str>,
        cost_usd: Option<f64>,
    ) -> Result<()> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let sid = self.id.to_string();
        sqlx::query(
            "INSERT INTO llm_usage (session_id, ts, model, agent, requests, prompt_tokens, \
             completion_tokens, cached_tokens, reasoning_tokens, total_tokens, cost_usd) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&sid)
        .bind(now_ms)
        .bind(model)
        .bind(agent)
        .bind(i64::from(usage.requests))
        .bind(usage.prompt_tokens)
        .bind(usage.completion_tokens)
        .bind(usage.cached_tokens)
        .bind(usage.reasoning_tokens)
        .bind(usage.total_tokens)
        .bind(cost_usd)
        .execute(&*self.pool)
        .await?;
        Ok(())
    }

    pub async fn rebuild_cache(&mut self) -> Result<()> {
        let sid = self.id.to_string();
        let rows = sqlx::query_as::<_, HistoryEntry>(
            "SELECT id, ts, role, content FROM messages WHERE session_id = ? AND compacted = 0 ORDER BY id",
        )
        .bind(&sid)
        .fetch_all(&*self.pool)
        .await?;

        self.cache = rows;
        Ok(())
    }

    /// Read a filtered slice of the transcript: `since`/`after_id` cut the
    /// head, `limit` caps the length. Oldest first, so an incremental
    /// reader paginates forward with `HistoryFilter::after_id` set to the
    /// last id it processed.
    pub async fn history_page(&self, filter: &HistoryFilter) -> Result<HistoryPage> {
        let sid = self.id.to_string();
        // SQLite: a negative LIMIT means no limit; `/` on integers is
        // floor division, so `ts / 1000 > since_ms` is "written in a
        // strictly later millisecond".
        let fetch = filter.limit.map_or(-1, |limit| i64::from(limit) + 1);
        let mut rows = sqlx::query_as::<_, HistoryEntry>(
            "SELECT id, ts, role, content FROM messages \
             WHERE session_id = ? AND compacted = 0 \
             AND (? IS NULL OR ts / 1000 > ?) \
             AND (? IS NULL OR id > ?) \
             ORDER BY id \
             LIMIT ?",
        )
        .bind(&sid)
        .bind(filter.since_ms)
        .bind(filter.since_ms)
        .bind(filter.after_id)
        .bind(filter.after_id)
        .bind(fetch)
        .fetch_all(&*self.pool)
        .await?;

        let truncated = match filter.limit {
            Some(limit) if rows.len() > limit as usize => {
                rows.truncate(limit as usize);
                true
            }
            _ => false,
        };
        Ok(HistoryPage {
            entries: rows,
            truncated,
        })
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
        let mut session = Session::create(pool.clone(), std::path::Path::new("/test")).await?;
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

    /// Seed three entries several milliseconds apart so their millisecond
    /// fields are distinct (the `since` cursor granularity).
    async fn spaced_session(pool: Arc<DbPool>) -> anyhow::Result<Session> {
        let mut session = Session::create(pool, std::path::Path::new("/test")).await?;
        for text in ["first", "second", "third"] {
            tokio::time::sleep(std::time::Duration::from_millis(3)).await;
            session.add_user(text).await?;
        }
        Ok(session)
    }

    #[tokio::test]
    async fn history_page_since_keeps_only_later_milliseconds() -> anyhow::Result<()> {
        let session = spaced_session(pool().await?).await?;
        let full = session
            .history_page(&HistoryFilter::default())
            .await?
            .entries;
        assert_eq!(full.len(), 3);
        assert!(full.windows(2).all(|w| w[0].ts < w[1].ts), "ts must grow");

        // Cursor = the millisecond of the second message: the first two
        // must be gone, including the exact one the cursor came from.
        let since_ms = full[1].ts / 1000;
        let page = session
            .history_page(&HistoryFilter {
                since_ms: Some(since_ms),
                ..HistoryFilter::default()
            })
            .await?;
        assert_eq!(
            page.entries.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![full[2].id]
        );
        Ok(())
    }

    #[tokio::test]
    async fn history_page_after_id_is_exact_cursor() -> anyhow::Result<()> {
        let session = spaced_session(pool().await?).await?;
        let full = session
            .history_page(&HistoryFilter::default())
            .await?
            .entries;

        // No sleeps needed here: row ids are strictly increasing, so the
        // cursor is exact even for same-millisecond writes.
        let page = session
            .history_page(&HistoryFilter {
                after_id: Some(full[0].id),
                ..HistoryFilter::default()
            })
            .await?;
        assert_eq!(
            page.entries.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![full[1].id, full[2].id]
        );
        assert!(!page.truncated);
        Ok(())
    }

    #[tokio::test]
    async fn history_page_limit_truncates_and_paginates_forward() -> anyhow::Result<()> {
        let session = spaced_session(pool().await?).await?;

        let page = session
            .history_page(&HistoryFilter {
                limit: Some(2),
                ..HistoryFilter::default()
            })
            .await?;
        assert_eq!(page.entries.len(), 2);
        assert!(page.truncated, "a longer history must be flagged");

        // Continue from the last id: the remainder comes next, untruncated.
        let cursor = page.entries.last().expect("entries exist").id;
        let rest = session
            .history_page(&HistoryFilter {
                after_id: Some(cursor),
                ..HistoryFilter::default()
            })
            .await?;
        assert_eq!(rest.entries.len(), 1);
        assert!(!rest.truncated);
        Ok(())
    }

    #[tokio::test]
    async fn record_usage_persists_and_aggregates() -> anyhow::Result<()> {
        use sqlx::Row as _;

        let pool = pool().await?;
        let session = Session::create(pool.clone(), std::path::Path::new("/test")).await?;

        let run = crate::usage::RunUsage {
            requests: 2,
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            cached_tokens: 80,
            reasoning_tokens: 20,
        };
        session
            .record_usage(&run, "test-model", None, Some(0.01))
            .await?;
        session
            .record_usage(&run, "test-model", Some("reviewer"), None)
            .await?;

        let row = sqlx::query(
            "SELECT COUNT(*) AS rows, SUM(requests) AS requests, SUM(total_tokens) AS total, \
             SUM(cached_tokens) AS cached, SUM(cost_usd) AS cost FROM llm_usage WHERE session_id = ?",
        )
        .bind(session.id.to_string())
        .fetch_one(&*pool)
        .await?;
        assert_eq!(row.try_get::<i64, _>("rows")?, 2);
        assert_eq!(row.try_get::<i64, _>("requests")?, 4);
        assert_eq!(row.try_get::<i64, _>("total")?, 300);
        assert_eq!(row.try_get::<i64, _>("cached")?, 160);
        assert!((row.try_get::<f64, _>("cost")? - 0.01).abs() < 1e-9);
        Ok(())
    }
}
