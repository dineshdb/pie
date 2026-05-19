use crate::db::DbPool;
use chrono::Utc;
use croner::Cron;
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use std::sync::Arc;

#[derive(
    Debug, Clone, Serialize, Deserialize, PartialEq, Eq, strum::Display, strum::EnumString,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum JobType {
    Shell,
    Prompt,
}

impl TryFrom<String> for JobType {
    type Error = <Self as FromStr>::Err;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct CronJob {
    pub id: String,
    pub name: String,
    #[sqlx(try_from = "String")]
    pub job_type: JobType,
    pub payload: String,
    pub cron: String,
    pub cwd: String,
    pub next_run_at: i64,
    pub enabled: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct CronRun {
    pub id: String,
    pub cron_id: String,
    pub session_id: String,
    pub status: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub exit_code: Option<i64>,
}

const SELECT_COLS: &str = "SELECT id, name, type as job_type, payload, cron, cwd, next_run_at, enabled, created_at, updated_at FROM cron_jobs";

impl CronJob {
    pub async fn insert(
        pool: &Arc<DbPool>,
        name: &str,
        job_type: &JobType,
        payload: &str,
        cron: &str,
        cwd: &str,
    ) -> Result<Self, sqlx::Error> {
        let id = crate::session::SessionId::new().to_string();
        let next_run_at = next_run_timestamp(cron).unwrap_or(0);
        sqlx::query_as::<_, Self>(
            "INSERT INTO cron_jobs (id, name, type, payload, cron, cwd, next_run_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?) \
             RETURNING id, name, type as job_type, payload, cron, cwd, next_run_at, enabled, created_at, updated_at",
        )
        .bind(&id)
        .bind(name)
        .bind(job_type.to_string())
        .bind(payload)
        .bind(cron)
        .bind(cwd)
        .bind(next_run_at)
        .fetch_one(&**pool)
        .await
    }

    pub async fn list_all(pool: &Arc<DbPool>) -> Result<Vec<Self>, sqlx::Error> {
        sqlx::query_as::<_, Self>(&format!("{SELECT_COLS} ORDER BY name"))
            .fetch_all(&**pool)
            .await
    }

    pub async fn delete(pool: &Arc<DbPool>, id: &str) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM cron_jobs WHERE id = ?")
            .bind(id)
            .execute(&**pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn find_due(pool: &Arc<DbPool>) -> Result<Vec<Self>, sqlx::Error> {
        let now_ms = Utc::now().timestamp_millis();
        sqlx::query_as::<_, Self>(&format!(
            "{SELECT_COLS} WHERE enabled = 1 AND next_run_at <= ? ORDER BY next_run_at"
        ))
        .bind(now_ms)
        .fetch_all(&**pool)
        .await
    }

    pub async fn update_next_run(&self, pool: &Arc<DbPool>) -> Result<(), sqlx::Error> {
        let next = next_run_timestamp(&self.cron).unwrap_or(0);
        sqlx::query("UPDATE cron_jobs SET next_run_at = ?, updated_at = unixepoch('subsec') * 1000 WHERE id = ?")
            .bind(next)
            .bind(&self.id)
            .execute(&**pool)
            .await?;
        Ok(())
    }
}

fn next_run_timestamp(cron_expr: &str) -> Option<i64> {
    let cron = Cron::from_str(cron_expr).ok()?;
    let next = cron.find_next_occurrence(&Utc::now(), false).ok()?;
    Some(next.timestamp_millis())
}

impl CronRun {
    pub async fn start(
        pool: &Arc<DbPool>,
        cron_id: &str,
        session_id: &str,
    ) -> Result<Self, sqlx::Error> {
        let id = crate::session::SessionId::new().to_string();
        sqlx::query_as::<_, Self>(
            "INSERT INTO cron_runs (id, cron_id, session_id, status) \
             VALUES (?, ?, ?, 'running') \
             RETURNING id, cron_id, session_id, status, started_at, finished_at, exit_code",
        )
        .bind(&id)
        .bind(cron_id)
        .bind(session_id)
        .fetch_one(&**pool)
        .await
    }

    pub async fn finish(&self, pool: &Arc<DbPool>, exit_code: i64) -> Result<(), sqlx::Error> {
        let status = if exit_code == 0 {
            "completed"
        } else {
            "failed"
        };
        sqlx::query(
            "UPDATE cron_runs SET status = ?, finished_at = unixepoch('subsec') * 1000, exit_code = ? WHERE id = ?",
        )
        .bind(status)
        .bind(exit_code)
        .bind(&self.id)
        .execute(&**pool)
        .await?;
        Ok(())
    }

    pub async fn find_running_for_cron(
        pool: &Arc<DbPool>,
        cron_id: &str,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM cron_runs WHERE cron_id = ? AND status = 'running')",
        )
        .bind(cron_id)
        .fetch_one(&**pool)
        .await
    }

    pub async fn find_recent_for_cron(
        pool: &Arc<DbPool>,
        cron_id: &str,
    ) -> Result<Vec<CronRun>, sqlx::Error> {
        sqlx::query_as::<_, CronRun>(
            "SELECT id, cron_id, session_id, status, started_at, finished_at, exit_code \
             FROM cron_runs WHERE cron_id = ? ORDER BY started_at DESC LIMIT 20",
        )
        .bind(cron_id)
        .fetch_all(&**pool)
        .await
    }
}
