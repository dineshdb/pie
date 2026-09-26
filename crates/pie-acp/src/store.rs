//! pie-tui's pie-local persistence: the SQL behind pie-core's store
//! traits ([`pie_core::store::UsageStore`] + [`pie_core::store::TokenStore`]),
//! on turso — the in-process SQLite-compatible engine.
//!
//! Two tables in the same `~/.pie/pie.db` the daemon always used (so
//! existing usage history and OAuth grants carry over): `llm_usage` (one
//! row per run, behind `pie usage`) and `mcp_oauth_tokens` (one JSON blob
//! per MCP server, behind `pie mcp login`). The conversation transcript
//! is deliberately NOT here — the TUI hydrates it from the a2acp
//! gateway's history, and the engine holds it in memory for the
//! process's lifetime.
//!
//! Schema: one idempotent baseline (below) instead of a migration
//! runner. It no-ops on a current file, creates the two tables on a
//! fresh one, and cleans up the dead tables old installs carried. The
//! one non-idempotent case — a `llm_usage` still foreign-keyed to the
//! long-gone `sessions` table — is detected and rebuilt before the
//! drop, rows kept.

use pie_core::store::{TokenStore, UsageEntry, UsageStore};
use pie_core::usage::{ModelUsage, UsageReport};
use std::sync::Arc;
use tokio::sync::Mutex;
use turso::Value;
use turso::params::Params;

/// The baseline: applied on every open, idempotent by construction.
const BASELINE_SQL: &str = r"
CREATE TABLE IF NOT EXISTS llm_usage (
    id                INTEGER PRIMARY KEY,
    session_id        TEXT    NOT NULL,
    ts                INTEGER NOT NULL,
    model             TEXT    NOT NULL,
    agent             TEXT,
    requests          INTEGER NOT NULL,
    prompt_tokens     INTEGER NOT NULL,
    completion_tokens INTEGER NOT NULL,
    cached_tokens     INTEGER NOT NULL,
    reasoning_tokens  INTEGER NOT NULL,
    total_tokens      INTEGER NOT NULL,
    cost_usd          REAL
);
CREATE INDEX IF NOT EXISTS idx_llm_usage_session_id ON llm_usage(session_id);
CREATE INDEX IF NOT EXISTS idx_llm_usage_ts ON llm_usage(ts);
CREATE TABLE IF NOT EXISTS mcp_oauth_tokens (
    server_name TEXT PRIMARY KEY,
    credentials TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
DROP TABLE IF EXISTS messages;
DROP TABLE IF EXISTS cron_runs;
DROP TABLE IF EXISTS a2a_artifacts;
DROP TABLE IF EXISTS a2a_seen_messages;
DROP TABLE IF EXISTS a2a_push_configs;
DROP TABLE IF EXISTS a2a_tasks;
DROP TABLE IF EXISTS sessions;
DROP TABLE IF EXISTS steps;
DROP TABLE IF EXISTS cron_jobs;
DROP TABLE IF EXISTS _sqlx_migrations;
";

/// Rebuild `llm_usage` without its dangling foreign key to `sessions`,
/// keeping every row. Only runs when the old shape is detected.
const REBUILD_LLM_USAGE_SQL: &str = r"
CREATE TABLE llm_usage_new (
    id                INTEGER PRIMARY KEY,
    session_id        TEXT    NOT NULL,
    ts                INTEGER NOT NULL,
    model             TEXT    NOT NULL,
    agent             TEXT,
    requests          INTEGER NOT NULL,
    prompt_tokens     INTEGER NOT NULL,
    completion_tokens INTEGER NOT NULL,
    cached_tokens     INTEGER NOT NULL,
    reasoning_tokens  INTEGER NOT NULL,
    total_tokens      INTEGER NOT NULL,
    cost_usd          REAL
);
INSERT INTO llm_usage_new
    (id, session_id, ts, model, agent, requests, prompt_tokens,
     completion_tokens, cached_tokens, reasoning_tokens, total_tokens, cost_usd)
    SELECT id, session_id, ts, model, agent, requests, prompt_tokens,
     completion_tokens, cached_tokens, reasoning_tokens, total_tokens, cost_usd
    FROM llm_usage;
DROP TABLE llm_usage;
ALTER TABLE llm_usage_new RENAME TO llm_usage;
CREATE INDEX idx_llm_usage_session_id ON llm_usage(session_id);
CREATE INDEX idx_llm_usage_ts ON llm_usage(ts);
";

/// Token counts are bounded by context sizes; the precision loss of
/// `i64 -> f64` is far below a cent of cost.
#[allow(clippy::cast_precision_loss)]
fn to_f64(n: i64) -> f64 {
    n as f64
}

/// The pie-local store: usage bookkeeping + MCP OAuth grants, over
/// `~/.pie/pie.db`. One value implements both traits, so call sites hold
/// a single `Arc<Store>` and coerce it to whichever trait they take.
/// The workload is a handful of tiny statements per turn — one
/// connection behind a mutex carries it.
#[derive(Debug, Clone)]
pub struct Store {
    conn: Arc<Mutex<turso::Connection>>,
}

/// Open (creating) the persistent store at `~/.pie/pie.db` and apply the
/// schema baseline.
///
/// # Errors
///
/// Fails when the home directory or the database cannot be set up.
pub async fn create_persistent_pool() -> anyhow::Result<Store> {
    let home = pie_core::config::pie_home();
    let db_path = home.join("pie.db");
    std::fs::create_dir_all(&home)?;
    let store = open(&db_path.display().to_string()).await?;
    store.migrate().await?;
    Ok(store)
}

/// In-memory store for tests.
///
/// # Errors
///
/// Fails when the in-memory database cannot be set up.
#[cfg(test)]
pub async fn create_test_pool() -> anyhow::Result<Store> {
    let store = open(":memory:").await?;
    store.migrate().await?;
    Ok(store)
}

async fn open(path: &str) -> anyhow::Result<Store> {
    let db = turso::Builder::new_local(path).build().await?;
    let conn = db.connect()?;
    // Several pie processes share this file (the `pie acp` daemon, every
    // single-shot run). Without a busy timeout a concurrent writer makes
    // the loser fail the whole run with "database is locked" — retry
    // instead; the workload is a few tiny statements per turn.
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(Store {
        conn: Arc::new(Mutex::new(conn)),
    })
}

impl Store {
    /// Detect the legacy `llm_usage → sessions` foreign key and rebuild
    /// the table without it, then apply the idempotent baseline.
    async fn migrate(&self) -> anyhow::Result<()> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'llm_usage'",
                Params::None,
            )
            .await?;
        let mut legacy_fk = false;
        if let Some(row) = rows.next().await?
            && let Value::Text(sql) = row.get_value(0)?
        {
            legacy_fk = sql.contains("REFERENCES");
        }
        if legacy_fk {
            conn.execute_batch(REBUILD_LLM_USAGE_SQL).await?;
        }
        conn.execute_batch(BASELINE_SQL).await?;
        Ok(())
    }

    /// Test helper: insert one usage row with an explicit timestamp,
    /// bypassing `record_usage`'s clock (aggregation windows need old
    /// rows).
    #[cfg(test)]
    async fn insert_run(
        &self,
        session_id: &str,
        ts: i64,
        model: &str,
        cost: Option<f64>,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO llm_usage (session_id, ts, model, agent, requests, prompt_tokens, \
             completion_tokens, cached_tokens, reasoning_tokens, total_tokens, cost_usd) \
             VALUES (?, ?, ?, NULL, 1, 100, 50, 80, 20, 150, ?)",
            Params::Positional(vec![
                Value::from(session_id),
                Value::from(ts),
                Value::from(model),
                match cost {
                    Some(c) => Value::from(c),
                    None => Value::Null,
                },
            ]),
        )
        .await?;
        anyhow::Ok(())
    }
}

#[async_trait::async_trait]
impl UsageStore for Store {
    async fn record_usage(&self, entry: UsageEntry) -> anyhow::Result<()> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO llm_usage (session_id, ts, model, agent, requests, prompt_tokens, \
             completion_tokens, cached_tokens, reasoning_tokens, total_tokens, cost_usd) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            Params::Positional(vec![
                Value::from(entry.session_id),
                Value::from(now_ms),
                Value::from(entry.model),
                entry.agent.map_or(Value::Null, Value::Text),
                Value::from(i64::from(entry.usage.requests)),
                Value::from(entry.usage.prompt_tokens),
                Value::from(entry.usage.completion_tokens),
                Value::from(entry.usage.cached_tokens),
                Value::from(entry.usage.reasoning_tokens),
                Value::from(entry.usage.total_tokens),
                entry.cost_usd.map_or(Value::Null, Value::Real),
            ]),
        )
        .await?;
        Ok(())
    }

    async fn usage_by_model(&self, since_ms: i64) -> anyhow::Result<Vec<ModelUsage>> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT model, COUNT(*) AS requests, SUM(prompt_tokens) AS prompt, \
                 SUM(completion_tokens) AS completion, SUM(cached_tokens) AS cached, \
                 SUM(reasoning_tokens) AS reasoning, SUM(total_tokens) AS total, \
                 SUM(cost_usd) AS cost \
                 FROM llm_usage WHERE (?1 = 0 OR ts >= ?1) \
                 GROUP BY model \
                 ORDER BY cost IS NULL, cost DESC, total DESC",
                Params::Positional(vec![Value::from(since_ms)]),
            )
            .await?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            let Value::Text(model) = row.get_value(0)? else {
                anyhow::bail!("llm_usage.model must be text")
            };
            let int = |v: turso::Result<Value>| -> anyhow::Result<i64> {
                match v? {
                    Value::Integer(n) => Ok(n),
                    _ => anyhow::bail!("llm_usage aggregate must be integer"),
                }
            };
            let cost = match row.get_value(7)? {
                Value::Real(c) => Some(c),
                Value::Null => None,
                _ => anyhow::bail!("llm_usage.cost must be real or null"),
            };
            let prompt_tokens = int(row.get_value(2))?;
            let cached_tokens = int(row.get_value(4))?;
            out.push(ModelUsage {
                model,
                usage: UsageReport {
                    requests: int(row.get_value(1))?
                        .try_into()
                        .map_err(|e: std::num::TryFromIntError| anyhow::anyhow!(e))?,
                    prompt_tokens,
                    completion_tokens: int(row.get_value(3))?,
                    total_tokens: int(row.get_value(6))?,
                    cached_tokens,
                    reasoning_tokens: int(row.get_value(5))?,
                    cache_rate: (prompt_tokens > 0)
                        .then(|| to_f64(cached_tokens) / to_f64(prompt_tokens)),
                    cost_usd: cost,
                },
            });
        }
        Ok(out)
    }
}

#[async_trait::async_trait]
impl TokenStore for Store {
    async fn load_token(&self, server: &str) -> anyhow::Result<Option<String>> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT credentials FROM mcp_oauth_tokens WHERE server_name = ?1",
                Params::Positional(vec![Value::from(server)]),
            )
            .await?;
        match rows.next().await? {
            Some(row) => match row.get_value(0)? {
                Value::Text(json) => Ok(Some(json)),
                _ => anyhow::bail!("mcp_oauth_tokens.credentials must be text"),
            },
            None => Ok(None),
        }
    }

    async fn save_token(&self, server: &str, json: String) -> anyhow::Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO mcp_oauth_tokens (server_name, credentials, updated_at) \
             VALUES (?1, ?2, ?3) \
             ON CONFLICT(server_name) DO UPDATE SET \
             credentials = excluded.credentials, updated_at = excluded.updated_at",
            Params::Positional(vec![
                Value::from(server),
                Value::from(json),
                Value::from(chrono::Utc::now().to_rfc3339()),
            ]),
        )
        .await?;
        Ok(())
    }

    async fn clear_token(&self, server: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "DELETE FROM mcp_oauth_tokens WHERE server_name = ?1",
            Params::Positional(vec![Value::from(server)]),
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use pie_core::usage::RunUsage;

    fn run(cost: Option<f64>) -> UsageEntry {
        UsageEntry {
            session_id: "s-1".to_string(),
            model: "m1".to_string(),
            agent: None,
            usage: RunUsage {
                requests: 1,
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
                cached_tokens: 80,
                reasoning_tokens: 20,
            },
            cost_usd: cost,
        }
    }

    #[tokio::test]
    async fn record_round_trips_through_by_model() -> anyhow::Result<()> {
        let store = create_test_pool().await?;
        store.record_usage(run(Some(0.01))).await?;
        store.record_usage(run(None)).await?;

        let rows = store.usage_by_model(0).await?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].usage.requests, 2);
        assert_eq!(rows[0].usage.prompt_tokens, 200);
        assert_eq!(rows[0].usage.cache_rate, Some(0.8));
        // One priced row + one unpriced row: the sum covers the priced one.
        assert!((rows[0].usage.cost_usd.unwrap() - 0.01).abs() < 1e-9);
        anyhow::Ok(())
    }

    #[tokio::test]
    async fn by_model_aggregates_within_window() -> anyhow::Result<()> {
        let store = create_test_pool().await?;
        let now = chrono::Utc::now().timestamp_millis();

        // priced model, two runs (one recent, one old)
        store
            .insert_run("s-1", now - 1_000, "m1", Some(0.01))
            .await?;
        store
            .insert_run("s-1", now - 40 * 86_400_000, "m1", Some(0.02))
            .await?;
        // unpriced model, recent — cost stays NULL
        store.insert_run("s-1", now - 1_000, "m2", None).await?;

        // all time: both models, m1 aggregated, ordered by cost (unpriced last)
        let rows = store.usage_by_model(0).await?;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].model, "m1");
        assert_eq!(rows[0].usage.requests, 2);
        assert_eq!(rows[0].usage.prompt_tokens, 200);
        assert_eq!(rows[0].usage.total_tokens, 300);
        assert_eq!(rows[0].usage.cache_rate, Some(0.8));
        assert_eq!(rows[0].usage.cost_usd, Some(0.03));
        assert_eq!(rows[1].model, "m2");
        assert_eq!(rows[1].usage.cost_usd, None, "unpriced model has null cost");

        // window excludes the 40-day-old run
        let rows = store.usage_by_model(now - 30 * 86_400_000).await?;
        assert_eq!(rows.len(), 2, "m2 and the recent m1 run");
        assert_eq!(rows[0].usage.requests, 1);
        anyhow::Ok(())
    }

    #[tokio::test]
    async fn tokens_round_trip_and_clear() -> anyhow::Result<()> {
        use pie_core::store::TokenStore as _;

        let store = create_test_pool().await?;
        assert_eq!(store.load_token("linear").await?, None);
        store.save_token("linear", r#"{"a":1}"#.to_string()).await?;
        assert_eq!(
            store.load_token("linear").await?.as_deref(),
            Some(r#"{"a":1}"#)
        );
        // Upsert overwrites.
        store.save_token("linear", r#"{"a":2}"#.to_string()).await?;
        assert_eq!(
            store.load_token("linear").await?.as_deref(),
            Some(r#"{"a":2}"#)
        );
        store.clear_token("linear").await?;
        assert_eq!(store.load_token("linear").await?, None);
        anyhow::Ok(())
    }

    #[tokio::test]
    async fn dead_tables_from_older_installs_are_dropped() -> anyhow::Result<()> {
        let store = create_test_pool().await?;
        // Fresh installs never had them; an upgraded pie.db did — either
        // way the baseline leaves exactly the two live tables (plus
        // nothing else).
        let conn = store.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name",
                Params::None,
            )
            .await?;
        let mut tables = Vec::new();
        while let Some(row) = rows.next().await? {
            match row.get_value(0)? {
                Value::Text(name) => tables.push(name),
                _ => anyhow::bail!("table names must be text"),
            }
        }
        drop(conn);
        assert!(
            !tables.iter().any(|t| {
                matches!(
                    t.as_str(),
                    "sessions"
                        | "messages"
                        | "cron_runs"
                        | "a2a_tasks"
                        | "a2a_artifacts"
                        | "a2a_seen_messages"
                        | "a2a_push_configs"
                        | "_sqlx_migrations"
                )
            }),
            "dead tables survived the baseline: {tables:?}"
        );
        assert!(tables.contains(&"llm_usage".to_string()));
        assert!(tables.contains(&"mcp_oauth_tokens".to_string()));
        anyhow::Ok(())
    }

    /// A pie.db from an old install carries `llm_usage` foreign-keyed to
    /// the (dead) `sessions` table. The baseline must rebuild the table
    /// without the key and keep every row, then drop `sessions`.
    #[tokio::test]
    async fn legacy_foreign_keyed_usage_is_rebuilt_with_rows_kept() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("legacy.db");
        {
            let db = turso::Builder::new_local(&db_path.display().to_string())
                .build()
                .await?;
            let conn = db.connect()?;
            conn.execute_batch(
                "CREATE TABLE sessions (id TEXT PRIMARY KEY);
                 CREATE TABLE llm_usage (
                     id         INTEGER PRIMARY KEY,
                     session_id TEXT NOT NULL REFERENCES sessions(id),
                     ts         INTEGER NOT NULL,
                     model      TEXT NOT NULL,
                     agent      TEXT,
                     requests   INTEGER NOT NULL,
                     prompt_tokens INTEGER NOT NULL,
                     completion_tokens INTEGER NOT NULL,
                     cached_tokens INTEGER NOT NULL,
                     reasoning_tokens INTEGER NOT NULL,
                     total_tokens INTEGER NOT NULL,
                     cost_usd REAL
                 );
                 INSERT INTO llm_usage VALUES (1, 's-9', 123, 'm1', NULL, 2, 10, 5, 0, 0, 15, 0.5);",
            )
            .await?;
        }

        let store = open(&db_path.display().to_string()).await?;
        store.migrate().await?;

        let rows = store.usage_by_model(0).await?;
        assert_eq!(rows.len(), 1, "the legacy row survives the rebuild");
        assert_eq!(rows[0].model, "m1");
        // `requests` counts runs (rows), not the column's sum.
        assert_eq!(rows[0].usage.requests, 1);
        assert_eq!(rows[0].usage.total_tokens, 15);
        assert_eq!(rows[0].usage.cost_usd, Some(0.5));

        let conn = store.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'sessions'",
                Params::None,
            )
            .await?;
        assert!(
            rows.next().await?.is_none(),
            "the dead sessions table must be dropped"
        );
        anyhow::Ok(())
    }
}
