use crate::db::DbPool;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct CronRun {
    pub id: String,
    pub cron_id: String,
    pub session_id: String,
    pub status: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub exit_code: Option<i64>,
    pub notes: String,
}

const SELECT: &str =
    "SELECT id, cron_id, session_id, status, started_at, finished_at, exit_code, notes";

impl CronRun {
    pub async fn cleanup_stale(pool: &Arc<DbPool>) -> Result<(), sqlx::Error> {
        let cutoff = Utc::now().timestamp_millis() - 3_600_000; // 1 hour ago
        sqlx::query(
            "UPDATE cron_runs SET status = 'failed', finished_at = unixepoch('subsec') * 1000 \
             WHERE status = 'running' AND started_at < ?",
        )
        .bind(cutoff)
        .execute(&**pool)
        .await?;
        Ok(())
    }

    pub async fn start(
        pool: &Arc<DbPool>,
        schedule_id: &str,
        session_id: &str,
    ) -> Result<Self, sqlx::Error> {
        let id = crate::session::SessionId::new().to_string();
        sqlx::query_as::<_, Self>(
            "INSERT INTO cron_runs (id, cron_id, session_id, status) \
             VALUES (?, ?, ?, 'running') \
             RETURNING id, cron_id, session_id, status, started_at, finished_at, exit_code, notes",
        )
        .bind(&id)
        .bind(schedule_id)
        .bind(session_id)
        .fetch_one(&**pool)
        .await
    }

    pub async fn finish(
        &self,
        pool: &Arc<DbPool>,
        exit_code: i64,
        notes: &str,
    ) -> Result<(), sqlx::Error> {
        let status = if exit_code == 0 {
            "completed"
        } else {
            "failed"
        };
        sqlx::query(
            "UPDATE cron_runs SET status = ?, finished_at = unixepoch('subsec') * 1000, exit_code = ?, notes = ? WHERE id = ?",
        )
        .bind(status)
        .bind(exit_code)
        .bind(notes)
        .bind(&self.id)
        .execute(&**pool)
        .await?;
        Ok(())
    }

    pub async fn last_run_for_schedule(
        pool: &Arc<DbPool>,
        schedule_id: &str,
    ) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as::<_, Self>(&format!(
            "{SELECT} FROM cron_runs WHERE cron_id = ? ORDER BY started_at DESC LIMIT 1"
        ))
        .bind(schedule_id)
        .fetch_optional(&**pool)
        .await
    }

    pub async fn is_running_for_schedule(
        pool: &Arc<DbPool>,
        schedule_id: &str,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM cron_runs WHERE cron_id = ? AND status = 'running')",
        )
        .bind(schedule_id)
        .fetch_one(&**pool)
        .await
    }

    pub async fn recent_for_schedule(
        pool: &Arc<DbPool>,
        schedule_id: &str,
    ) -> Result<Vec<Self>, sqlx::Error> {
        sqlx::query_as::<_, Self>(&format!(
            "{SELECT} FROM cron_runs WHERE cron_id = ? ORDER BY started_at DESC LIMIT 20"
        ))
        .bind(schedule_id)
        .fetch_all(&**pool)
        .await
    }

    pub async fn recent_all(pool: &Arc<DbPool>) -> Result<Vec<Self>, sqlx::Error> {
        sqlx::query_as::<_, Self>(&format!(
            "{SELECT} FROM cron_runs ORDER BY started_at DESC LIMIT 10"
        ))
        .fetch_all(&**pool)
        .await
    }
}
