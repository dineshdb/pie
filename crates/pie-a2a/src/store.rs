//! Durable A2A task state: SQLite rows backing every task the daemon has
//! ever handed out.
//!
//! The HTTP transport is stateless (no session affinity, no connection
//! state), but task *state* is durable, per the spec: resolved tasks stay
//! retrievable via `GetTask`, push notification configs survive restarts,
//! and follow-up messages find their conversation. Only the running turn
//! itself lives in memory ([`super::LiveTask`]); everything else is here.

use crate::a2a::TaskState;
use chrono::Utc;
use pie_core::db::DbPool;
use serde_json::Value;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use std::sync::Arc;

/// One persisted task, as stored. `turn` counts the finished turns (1 =
/// the first); `updated_at` is the epoch-millisecond timestamp of the
/// last state write — the `ListTasks` ordering key.
pub(crate) struct TaskRow {
    pub id: String,
    pub context_id: String,
    pub state: TaskState,
    pub status_message: Option<String>,
    pub response: String,
    pub completion: Option<Value>,
    pub artifact_id: String,
    pub turn: u32,
    pub updated_at: i64,
}

/// One finished turn's answer, as stored (oldest turn first).
pub(crate) struct ArtifactRow {
    pub artifact_id: String,
    pub text: String,
}

/// The registered webhook for a task, as stored.
pub(crate) struct PushConfigRow {
    pub url: String,
    pub token: Option<String>,
    pub auth_scheme: Option<String>,
    pub auth_credentials: Option<String>,
}

/// The `ListTasks` query: filters plus the pagination cursor. The cursor is
/// `(updated_at, id)` of the last row of the previous page — rows sort by
/// `updated_at DESC, id DESC`, so the next page is strictly "after" it.
pub(crate) struct ListFilter<'a> {
    pub context_id: Option<&'a str>,
    pub state: Option<TaskState>,
    pub updated_after: Option<i64>,
    pub cursor_updated: Option<i64>,
    pub cursor_id: Option<&'a str>,
    pub limit: u32,
}

/// Task-state persistence. Cheap handle; clone freely.
#[derive(Clone)]
pub(crate) struct TaskStore {
    pool: Arc<DbPool>,
}

/// What a finished turn leaves behind as the task's durable record.
pub(crate) struct TurnRecord<'a> {
    pub state: TaskState,
    pub status_message: Option<&'a str>,
    pub response: &'a str,
    pub artifact_id: &'a str,
    pub completion: Option<&'a Value>,
}

impl TaskStore {
    pub(crate) fn new(pool: Arc<DbPool>) -> Self {
        Self { pool }
    }

    /// Insert the row for a task whose turn just started (or reset an
    /// existing row to a fresh WORKING turn on the same task id) and
    /// return the new turn number (1 = first turn).
    pub(crate) async fn record_turn_start(
        &self,
        id: &str,
        context_id: &str,
        artifact_id: &str,
    ) -> Result<u32, String> {
        let now = Utc::now().timestamp_millis();
        let (turn,): (i64,) = sqlx::query_as(
            "INSERT INTO a2a_tasks (id, context_id, state, response, artifact_id, turn, updated_at) \
             VALUES (?, ?, 'TASK_STATE_WORKING', '', ?, 1, ?) \
             ON CONFLICT (id) DO UPDATE SET \
             state = 'TASK_STATE_WORKING', status_message = NULL, response = '', \
             completion = NULL, artifact_id = excluded.artifact_id, \
             turn = a2a_tasks.turn + 1, updated_at = excluded.updated_at \
             RETURNING turn",
        )
        .bind(id)
        .bind(context_id)
        .bind(artifact_id)
        .bind(now)
        .fetch_one(&*self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(u32::try_from(turn).unwrap_or(1))
    }

    /// Persist the turn-final record: the answer text, status message and
    /// usage metadata become the task's durable record, and a non-empty
    /// answer is stored as the turn's artifact (one per turn — a
    /// conversation-task accumulates them).
    pub(crate) async fn record_turn_end(
        &self,
        id: &str,
        turn: u32,
        record: TurnRecord<'_>,
    ) -> Result<(), String> {
        sqlx::query(
            "UPDATE a2a_tasks SET state = ?, status_message = ?, response = ?, \
             completion = ?, updated_at = ? WHERE id = ?",
        )
        .bind(record.state.as_str())
        .bind(record.status_message)
        .bind(record.response)
        .bind(record.completion.map(ToString::to_string))
        .bind(Utc::now().timestamp_millis())
        .bind(id)
        .execute(&*self.pool)
        .await
        .map_err(|e| e.to_string())?;
        if !record.response.is_empty() {
            sqlx::query(
                "INSERT INTO a2a_artifacts (task_id, turn, artifact_id, text) \
                 VALUES (?, ?, ?, ?) ON CONFLICT (task_id, turn) DO UPDATE SET \
                 artifact_id = excluded.artifact_id, text = excluded.text",
            )
            .bind(id)
            .bind(turn)
            .bind(record.artifact_id)
            .bind(record.response)
            .execute(&*self.pool)
            .await
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Load a persisted task.
    pub(crate) async fn load(&self, id: &str) -> Result<Option<TaskRow>, String> {
        let row = sqlx::query(
            "SELECT id, context_id, state, status_message, response, completion, artifact_id, \
             turn, updated_at FROM a2a_tasks WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| e.to_string())?
        .as_ref()
        .map(task_row_ref);
        Ok(row)
    }

    /// Every artifact of a task, oldest turn first.
    pub(crate) async fn artifacts(&self, task_id: &str) -> Result<Vec<ArtifactRow>, String> {
        let rows = sqlx::query(
            "SELECT turn, artifact_id, text FROM a2a_artifacts WHERE task_id = ? ORDER BY turn",
        )
        .bind(task_id)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(rows
            .into_iter()
            .map(|row| ArtifactRow {
                artifact_id: row.try_get("artifact_id").unwrap_or_default(),
                text: row.try_get("text").unwrap_or_default(),
            })
            .collect())
    }

    /// One page of tasks, newest activity first, plus the total matching
    /// row count (ignoring the cursor). Fetches `limit + 1` rows so the
    /// caller can tell whether a next page exists.
    pub(crate) async fn list(&self, filter: ListFilter<'_>) -> Result<(Vec<TaskRow>, i64), String> {
        let rows = sqlx::query(
            "SELECT id, context_id, state, status_message, response, completion, artifact_id, \
             turn, updated_at FROM a2a_tasks \
             WHERE (?1 IS NULL OR context_id = ?1) \
             AND (?2 IS NULL OR state = ?2) \
             AND (?3 IS NULL OR updated_at > ?3) \
             AND (?4 IS NULL OR updated_at < ?4 OR (updated_at = ?4 AND id < ?5)) \
             ORDER BY updated_at DESC, id DESC LIMIT ?6",
        )
        .bind(filter.context_id)
        .bind(filter.state.map(TaskState::as_str))
        .bind(filter.updated_after)
        .bind(filter.cursor_updated)
        .bind(filter.cursor_id)
        .bind(filter.limit)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| e.to_string())?
        .iter()
        .map(task_row_ref)
        .collect();
        let total = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM a2a_tasks \
             WHERE (?1 IS NULL OR context_id = ?1) \
             AND (?2 IS NULL OR state = ?2) \
             AND (?3 IS NULL OR updated_at > ?3)",
        )
        .bind(filter.context_id)
        .bind(filter.state.map(TaskState::as_str))
        .bind(filter.updated_after)
        .fetch_one(&*self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok((rows, total))
    }

    /// Record a delivered message id. Returns false when it was already
    /// seen — the delivery is a duplicate and must not start another turn.
    pub(crate) async fn remember_message(
        &self,
        message_id: &str,
        task_id: &str,
    ) -> Result<bool, String> {
        let result = sqlx::query(
            "INSERT INTO a2a_seen_messages (message_id, task_id, created_at) \
             VALUES (?, ?, ?) ON CONFLICT (message_id) DO NOTHING",
        )
        .bind(message_id)
        .bind(task_id)
        .bind(Utc::now().timestamp_millis())
        .execute(&*self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(result.rows_affected() == 1)
    }

    /// The task a previously seen message id produced.
    pub(crate) async fn message_task(&self, message_id: &str) -> Result<Option<String>, String> {
        let task_id: Option<String> =
            sqlx::query_scalar("SELECT task_id FROM a2a_seen_messages WHERE message_id = ?")
                .bind(message_id)
                .fetch_optional(&*self.pool)
                .await
                .map_err(|e| e.to_string())?;
        Ok(task_id)
    }

    /// Register or replace the webhook for a task.
    pub(crate) async fn set_push_config(
        &self,
        task_id: &str,
        config: &PushConfigRow,
    ) -> Result<(), String> {
        sqlx::query(
            "INSERT INTO a2a_push_configs (task_id, url, token, auth_scheme, auth_credentials) \
             VALUES (?, ?, ?, ?, ?) ON CONFLICT (task_id) DO UPDATE SET \
             url = excluded.url, token = excluded.token, auth_scheme = excluded.auth_scheme, \
             auth_credentials = excluded.auth_credentials",
        )
        .bind(task_id)
        .bind(&config.url)
        .bind(&config.token)
        .bind(&config.auth_scheme)
        .bind(&config.auth_credentials)
        .execute(&*self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// The webhook registered for a task, if any.
    pub(crate) async fn push_config(&self, task_id: &str) -> Result<Option<PushConfigRow>, String> {
        let row = sqlx::query(
            "SELECT url, token, auth_scheme, auth_credentials \
             FROM a2a_push_configs WHERE task_id = ?",
        )
        .bind(task_id)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| e.to_string())?
        .map(|row: SqliteRow| PushConfigRow {
            url: row.try_get("url").unwrap_or_default(),
            token: row.try_get("token").unwrap_or_default(),
            auth_scheme: row.try_get("auth_scheme").unwrap_or_default(),
            auth_credentials: row.try_get("auth_credentials").unwrap_or_default(),
        });
        Ok(row)
    }

    /// Turns a restart interrupted mid-flight can never finish: report
    /// them as failed, so clients observe a terminal state instead of an
    /// eternal `WORKING`. Runs once at daemon startup.
    pub(crate) async fn fail_stale_working(&self) -> Result<u64, String> {
        let result = sqlx::query(
            "UPDATE a2a_tasks SET state = 'TASK_STATE_FAILED', \
             status_message = 'daemon restarted during the turn', updated_at = ? \
             WHERE state = 'TASK_STATE_WORKING'",
        )
        .bind(Utc::now().timestamp_millis())
        .execute(&*self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(result.rows_affected())
    }
}

fn task_row_ref(row: &SqliteRow) -> TaskRow {
    let state: String = row.try_get("state").unwrap_or_default();
    let completion: Option<String> = row.try_get("completion").unwrap_or_default();
    TaskRow {
        id: row.try_get("id").unwrap_or_default(),
        context_id: row.try_get("context_id").unwrap_or_default(),
        state: TaskState::parse(&state).unwrap_or(TaskState::Failed),
        status_message: row.try_get("status_message").unwrap_or_default(),
        response: row.try_get("response").unwrap_or_default(),
        completion: completion.and_then(|c| serde_json::from_str(&c).ok()),
        artifact_id: row.try_get("artifact_id").unwrap_or_default(),
        turn: u32::try_from(row.try_get::<i64, _>("turn").unwrap_or(0)).unwrap_or(0),
        updated_at: row.try_get("updated_at").unwrap_or_default(),
    }
}
