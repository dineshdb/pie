use crate::db::DbPool;
use crate::tools::tasks::SharedTaskList;
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

impl rusqlite::types::ToSql for Role {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        self.as_str().to_sql()
    }
}

impl rusqlite::types::FromSql for Role {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        let s = String::column_result(value)?;
        s.parse()
            .map_err(|_| rusqlite::types::FromSqlError::InvalidType)
    }
}

// ── Session ────────────────────────────────────────────────────────

pub struct Session {
    pub id: Uuid,
    pool: Arc<DbPool>,
    cache: Vec<HistoryEntry>,
    pub task_state: SharedTaskList,
}

impl Session {
    pub fn create(pool: Arc<DbPool>) -> anyhow::Result<Self> {
        let id = Uuid::now_v7();
        let cwd = std::env::current_dir()?.to_string_lossy().to_string();
        let conn = pool.get()?;
        conn.execute(
            "INSERT INTO sessions (id, cwd) VALUES (?, ?)",
            rusqlite::params![id.to_string(), cwd],
        )?;
        Ok(Self {
            id,
            pool,
            cache: Vec::new(),
            task_state: SharedTaskList::default(),
        })
    }

    pub fn load(pool: Arc<DbPool>, session_id: Uuid) -> anyhow::Result<Self> {
        let exists = {
            let conn = pool.get()?;
            conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?)",
                rusqlite::params![session_id.to_string()],
                |row| row.get::<_, bool>(0),
            )?
        };
        if !exists {
            anyhow::bail!("Session {session_id} not found");
        }
        let mut session = Self {
            id: session_id,
            pool,
            cache: Vec::new(),
            task_state: SharedTaskList::default(),
        };
        session.rebuild_cache()?;
        Ok(session)
    }

    pub fn find_latest_for_cwd(pool: Arc<DbPool>, cwd: &str) -> anyhow::Result<Option<Self>> {
        let id_str = {
            let conn = pool.get()?;
            conn.query_row(
                "SELECT id FROM sessions WHERE cwd = ? ORDER BY updated_at DESC LIMIT 1",
                rusqlite::params![cwd],
                |row| row.get::<_, String>(0),
            )
            .ok()
        };
        let Some(id_str) = id_str else {
            return Ok(None);
        };
        Ok(Some(Self::load(pool, Uuid::parse_str(&id_str)?)?))
    }

    pub fn history_entries(&self) -> &[HistoryEntry] {
        &self.cache
    }

    pub fn pool(&self) -> &Arc<DbPool> {
        &self.pool
    }

    fn add_message(&mut self, role: Role, content: &str) -> anyhow::Result<()> {
        let conn = self.pool.get()?;
        let now_ms = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO messages (session_id, ts, role, content) VALUES (?, ?, ?, ?)",
            rusqlite::params![self.id.to_string(), now_ms * 1000, role, content,],
        )?;
        conn.execute(
            "UPDATE sessions SET updated_at = ? WHERE id = ?",
            rusqlite::params![now_ms, self.id.to_string()],
        )?;
        self.cache.push(HistoryEntry {
            role,
            content: content.to_string(),
        });
        Ok(())
    }

    pub fn add_user(&mut self, content: &str) -> anyhow::Result<()> {
        self.add_message(Role::User, content)
    }

    pub fn add_assistant(&mut self, content: &str) -> anyhow::Result<()> {
        self.add_message(Role::Assistant, content)
    }

    pub fn add_tool(&mut self, content: &str) -> anyhow::Result<()> {
        self.add_message(Role::Tool, content)
    }

    fn rebuild_cache(&mut self) -> anyhow::Result<()> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT role, content FROM messages \
             WHERE session_id = ? AND compacted = 0 \
             ORDER BY id",
        )?;
        let messages: Vec<HistoryEntry> = stmt
            .query_map(rusqlite::params![self.id.to_string()], |row| {
                let role: Role = row.get(0)?;
                let content: String = row.get(1)?;
                Ok(HistoryEntry { role, content })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        self.cache = messages;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn pool() -> anyhow::Result<Arc<DbPool>> {
        Ok(Arc::new(db::create_test_pool()?))
    }

    #[test]
    fn create_session() -> anyhow::Result<()> {
        let pool = pool()?;
        let session = Session::create(pool.clone())?;
        assert!(session.history_entries().is_empty());
        Ok(())
    }

    #[test]
    fn load_nonexistent_session() -> anyhow::Result<()> {
        let pool = pool()?;
        let result = Session::load(pool, Uuid::now_v7());
        assert!(result.is_err());
        Ok(())
    }

    #[test]
    fn find_latest_for_cwd_returns_most_recent() -> anyhow::Result<()> {
        let pool = pool()?;
        let cwd = "/test/path";

        // Insert sessions with explicit timestamps to guarantee ordering
        let conn = pool.get()?;
        let id1 = Uuid::now_v7().to_string();
        conn.execute(
            "INSERT INTO sessions (id, cwd, created_at, updated_at) VALUES (?, ?, 1000, 1000)",
            rusqlite::params![id1, cwd],
        )?;

        let id2 = Uuid::now_v7().to_string();
        conn.execute(
            "INSERT INTO sessions (id, cwd, created_at, updated_at) VALUES (?, ?, 2000, 2000)",
            rusqlite::params![id2, cwd],
        )?;
        drop(conn);

        let found = Session::find_latest_for_cwd(pool, cwd)?
            .ok_or_else(|| anyhow::anyhow!("no session found"))?;
        assert_eq!(found.id, Uuid::parse_str(&id2)?);
        Ok(())
    }

    #[test]
    fn find_latest_for_cwd_returns_none_when_empty() -> anyhow::Result<()> {
        let pool = pool()?;
        let result = Session::find_latest_for_cwd(pool, "/nonexistent")?;
        assert!(result.is_none());
        Ok(())
    }

    #[test]
    fn add_user_and_assistant() -> anyhow::Result<()> {
        let pool = pool()?;
        let mut session = Session::create(pool.clone())?;
        session.add_user("hello")?;
        session.add_assistant("hi there")?;

        let entries = session.history_entries();
        assert_eq!(entries.len(), 2);
        let first = entries
            .first()
            .ok_or_else(|| anyhow::anyhow!("no entry 0"))?;
        assert_eq!(first.role, Role::User);
        assert_eq!(first.content, "hello");
        let second = entries
            .get(1)
            .ok_or_else(|| anyhow::anyhow!("no entry 1"))?;
        assert_eq!(second.role, Role::Assistant);
        assert_eq!(second.content, "hi there");
        Ok(())
    }

    #[test]
    fn history_persists_after_load() -> anyhow::Result<()> {
        let pool = pool()?;
        let id = {
            let mut session = Session::create(pool.clone())?;
            session.add_user("first")?;
            session.add_assistant("second")?;
            session.id
        };

        let loaded = Session::load(pool, id)?;
        let entries = loaded.history_entries();
        assert_eq!(entries.len(), 2);
        let first = entries
            .first()
            .ok_or_else(|| anyhow::anyhow!("no entry 0"))?;
        assert_eq!(first.content, "first");
        let second = entries
            .get(1)
            .ok_or_else(|| anyhow::anyhow!("no entry 1"))?;
        assert_eq!(second.content, "second");
        Ok(())
    }
}
