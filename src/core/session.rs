use crate::core::db::DbPool;
use crate::core::prompt::HistoryEntry;
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

pub struct Session {
    id: Uuid,
    pool: Arc<DbPool>,
    cache: Vec<HistoryEntry>,
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
        };
        session.rebuild_cache()?;
        Ok(session)
    }

    pub fn find_latest_for_cwd(pool: Arc<DbPool>, cwd: &str) -> anyhow::Result<Option<Self>> {
        let id_str = {
            let conn = pool.get()?;
            conn.query_row(
                "SELECT id FROM sessions WHERE cwd = ? ORDER BY created_at DESC LIMIT 1",
                rusqlite::params![cwd],
                |row| row.get::<_, String>(0),
            )
            .ok()
        };
        match id_str {
            Some(id_str) => {
                let id = Uuid::parse_str(&id_str)?;
                Ok(Some(Self::load(pool, id)?))
            }
            None => Ok(None),
        }
    }

    pub fn history_entries(&self) -> &[HistoryEntry] {
        &self.cache
    }

    pub fn id(&self) -> Uuid {
        self.id
    }

    fn add_message(&mut self, role: Role, content: &str) -> anyhow::Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO messages (session_id, ts, role, content) VALUES (?, ?, ?, ?)",
            rusqlite::params![
                self.id.to_string(),
                chrono::Utc::now().timestamp_micros(),
                role,
                content,
            ],
        )?;
        self.cache.push(HistoryEntry {
            role: role.as_str(),
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
                Ok(HistoryEntry {
                    role: role.as_str(),
                    content,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        self.cache = messages;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::db;

    fn pool() -> Arc<DbPool> {
        Arc::new(db::create_test_pool())
    }

    #[test]
    fn create_session() {
        let pool = pool();
        let session = Session::create(pool.clone()).unwrap();
        assert!(session.history_entries().is_empty());
    }

    #[test]
    fn load_existing_session() {
        let pool = pool();
        let session = Session::create(pool.clone()).unwrap();
        let loaded = Session::load(pool, session.id()).unwrap();
        assert!(loaded.history_entries().is_empty());
    }

    #[test]
    fn load_nonexistent_session() {
        let pool = pool();
        let result = Session::load(pool, Uuid::now_v7());
        assert!(result.is_err());
    }

    #[test]
    fn find_latest_for_cwd_returns_most_recent() {
        let pool = pool();
        let cwd = "/test/path";

        // Insert sessions with explicit timestamps to guarantee ordering
        let conn = pool.get().unwrap();
        let id1 = Uuid::now_v7().to_string();
        conn.execute(
            "INSERT INTO sessions (id, cwd, created_at, updated_at) VALUES (?, ?, 1000, 1000)",
            rusqlite::params![id1, cwd],
        )
        .unwrap();

        let id2 = Uuid::now_v7().to_string();
        conn.execute(
            "INSERT INTO sessions (id, cwd, created_at, updated_at) VALUES (?, ?, 2000, 2000)",
            rusqlite::params![id2, cwd],
        )
        .unwrap();
        drop(conn);

        let found = Session::find_latest_for_cwd(pool, cwd).unwrap().unwrap();
        assert_eq!(found.id(), Uuid::parse_str(&id2).unwrap());
    }

    #[test]
    fn find_latest_for_cwd_returns_none_when_empty() {
        let pool = pool();
        let result = Session::find_latest_for_cwd(pool, "/nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn add_user_and_assistant() {
        let pool = pool();
        let mut session = Session::create(pool.clone()).unwrap();
        session.add_user("hello").unwrap();
        session.add_assistant("hi there").unwrap();

        let entries = session.history_entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].role, "user");
        assert_eq!(entries[0].content, "hello");
        assert_eq!(entries[1].role, "assistant");
        assert_eq!(entries[1].content, "hi there");
    }

    #[test]
    fn history_persists_after_load() {
        let pool = pool();
        let id = {
            let mut session = Session::create(pool.clone()).unwrap();
            session.add_user("first").unwrap();
            session.add_assistant("second").unwrap();
            session.id()
        };

        let loaded = Session::load(pool, id).unwrap();
        let entries = loaded.history_entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].content, "first");
        assert_eq!(entries[1].content, "second");
    }

    #[test]
    fn cache_rebuilt_after_rebuild_cache() {
        let pool = pool();
        let mut session = Session::create(pool.clone()).unwrap();
        session.add_user("msg").unwrap();
        assert_eq!(session.history_entries().len(), 1);

        // Manually clear cache and rebuild
        session.cache.clear();
        assert!(session.history_entries().is_empty());

        session.rebuild_cache().unwrap();
        assert_eq!(session.history_entries().len(), 1);
        assert_eq!(session.history_entries()[0].content, "msg");
    }
}
