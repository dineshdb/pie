use crate::db::DbPool;
use serde::{Deserialize, Serialize};
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

#[derive(Clone)]
pub struct HistoryEntry {
    pub role: Role,
    pub content: String,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

#[derive(sqlx::FromRow)]
struct MessageRow {
    role: String,
    content: String,
}

// ── Session ────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct Session {
    pub id: Uuid,
    pub pool: Arc<DbPool>,
    cache: Vec<HistoryEntry>,
}

impl Session {
    pub async fn create(pool: Arc<DbPool>) -> anyhow::Result<Self> {
        let id = Uuid::now_v7();
        let cwd = std::env::current_dir()?.to_string_lossy().to_string();
        let id_str = id.to_string();
        sqlx::query!("INSERT INTO sessions (id, cwd) VALUES (?, ?)", id_str, cwd)
            .execute(&*pool)
            .await?;
        Ok(Self {
            id,
            pool,
            cache: Vec::new(),
        })
    }

    pub async fn load(pool: Arc<DbPool>, session_id: Uuid) -> anyhow::Result<Self> {
        let sid = session_id.to_string();
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?)")
            .bind(sid)
            .fetch_one(&*pool)
            .await?;
        if !exists {
            anyhow::bail!("Session {session_id} not found");
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
            Some(sid) => {
                let id = Uuid::parse_str(&sid)?;
                Ok(Some(Self::load(pool, id).await?))
            }
            None => Ok(None),
        }
    }

    pub fn history_entries(&self) -> &[HistoryEntry] {
        &self.cache
    }

    pub fn pool(&self) -> &Arc<DbPool> {
        &self.pool
    }

    async fn add_message(&mut self, role: Role, content: &str) -> anyhow::Result<()> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let sid = self.id.to_string();
        let ts = now_ms * 1000;
        let role_str = role.as_str();

        sqlx::query!(
            "INSERT INTO messages (session_id, ts, role, content) VALUES (?, ?, ?, ?)",
            sid,
            ts,
            role_str,
            content,
        )
        .execute(&*self.pool)
        .await?;

        self.cache.push(HistoryEntry {
            role,
            content: content.to_string(),
        });
        Ok(())
    }

    pub async fn add_user(&mut self, content: &str) -> anyhow::Result<()> {
        self.add_message(Role::User, content).await
    }

    pub async fn add_assistant(&mut self, content: &str) -> anyhow::Result<()> {
        self.add_message(Role::Assistant, content).await
    }

    pub async fn add_tool(&mut self, content: &str) -> anyhow::Result<()> {
        self.add_message(Role::Tool, content).await
    }

    async fn rebuild_cache(&mut self) -> anyhow::Result<()> {
        let sid = self.id.to_string();
        let rows = sqlx::query_as!(
            MessageRow,
            "SELECT role, content FROM messages WHERE session_id = ? AND compacted = 0 ORDER BY id",
            sid,
        )
        .fetch_all(&*self.pool)
        .await?;

        self.cache = rows
            .into_iter()
            .filter_map(|r| {
                Some(HistoryEntry {
                    role: r.role.parse().ok()?,
                    content: r.content,
                })
            })
            .collect();
        Ok(())
    }
}

#[allow(clippy::indexing_slicing)]
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
        let result = Session::load(pool, Uuid::now_v7()).await;
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
        assert_eq!(entries[0].role, Role::User);
        assert_eq!(entries[0].content, "hello");
        assert_eq!(entries[1].role, Role::Assistant);
        assert_eq!(entries[1].content, "hi there");
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
        assert_eq!(entries[0].content, "first");
        assert_eq!(entries[1].content, "second");
        Ok(())
    }
}
