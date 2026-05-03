use crate::db::DbPool;
use agentsdk::core::tools::{Tool, ToolExecute};
use anyhow::Context;
use rusqlite::params;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use strum::{AsRefStr, Display, EnumString};

#[derive(
    Debug,
    Clone,
    JsonSchema,
    Default,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    AsRefStr,
    Display,
    EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum TaskStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Serialize, JsonSchema, Deserialize, PartialEq, Eq)]
pub struct Task {
    pub id: Option<i64>,
    pub name: String,
    #[serde(default)]
    pub status: TaskStatus,
}

pub trait TaskRepo {
    fn load_tasks(&self, session_id: &str) -> anyhow::Result<Vec<Task>>;
    fn save_task(&self, session_id: &str, task: &Task) -> anyhow::Result<i64>;
    fn update_task_status(
        &self,
        session_id: &str,
        name: &str,
        status: TaskStatus,
    ) -> anyhow::Result<()>;
    fn delete_tasks(&self, session_id: &str) -> anyhow::Result<()>;
}

impl TaskRepo for DbPool {
    fn load_tasks(&self, session_id: &str) -> anyhow::Result<Vec<Task>> {
        let conn = self.get()?;
        let mut stmt = conn.prepare(
            "SELECT id, name, status FROM tasks WHERE session_id = ? ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            let status_str: String = row.get(2)?;
            Ok(Task {
                id: Some(row.get(0)?),
                name: row.get(1)?,
                status: status_str.parse().unwrap_or(TaskStatus::Pending),
            })
        })?;

        Ok(rows.flatten().collect())
    }

    fn save_task(&self, session_id: &str, task: &Task) -> anyhow::Result<i64> {
        let conn = self.get()?;
        let status_ref: &str = task.status.as_ref();
        conn.execute(
            "INSERT INTO tasks (session_id, name, status, updated_at) 
             VALUES (?1, ?2, ?3, unixepoch('subsec') * 1000)
             ON CONFLICT(session_id, name) DO UPDATE SET 
             status = excluded.status, 
             updated_at = excluded.updated_at",
            params![session_id, task.name, status_ref],
        )?;
        Ok(conn.last_insert_rowid())
    }

    fn update_task_status(
        &self,
        session_id: &str,
        name: &str,
        status: TaskStatus,
    ) -> anyhow::Result<()> {
        let conn = self.get()?;
        let status_ref: &str = status.as_ref();
        conn.execute(
            "UPDATE tasks SET status = ?, updated_at = unixepoch('subsec') * 1000 
             WHERE session_id = ? AND name = ?",
            params![status_ref, session_id, name],
        )?;
        Ok(())
    }

    fn delete_tasks(&self, session_id: &str) -> anyhow::Result<()> {
        let conn = self.get()?;
        conn.execute(
            "DELETE FROM tasks WHERE session_id = ?",
            params![session_id],
        )?;
        Ok(())
    }
}

pub fn enforce_planning(pool: &Arc<DbPool>, session_id: &str, tool: &str) -> Result<(), String> {
    let tasks = pool.load_tasks(session_id).unwrap_or_default();
    if tasks.is_empty() {
        return Err(format!(
            "CRITICAL ERROR: You called '{tool}' without a task list. \
             You MUST call 'task_add' with a full plan before taking any actions. \
             This is your CORE MANDATE for reliability."
        ));
    }
    Ok(())
}

#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct TaskListInput {
    pub tasks: Vec<Task>,
}

#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct GetTasksInput {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskUpdate {
    pub name: String,
    pub status: TaskStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskUpdateList {
    pub updates: Vec<TaskUpdate>,
}

// ── Tool builders ──────────────────────────────────────────────────

fn build_task_add(pool: Arc<DbPool>, session_id: String) -> anyhow::Result<Tool> {
    Tool::builder()
        .name("task_add")
        .description(
            r#"CRITICAL: Planning phase. Call this FIRST. Input: {"tasks": [{"name": "..."}]}"#,
        )
        .input_schema(schemars::schema_for!(TaskListInput))
        .execute(ToolExecute::from_sync(move |_ctx, params| {
            let input: TaskListInput = serde_json::from_value(params.clone())
                .map_err(|e| format!("Invalid input: {e}"))?;

            for t in input.tasks {
                let _ = pool.save_task(&session_id, &t);
            }

            Ok(json!({ "status": "ok" }).to_string())
        }))
        .build()
        .context("failed to build task_add tool")
}

fn build_task_update(pool: Arc<DbPool>, session_id: String) -> anyhow::Result<Tool> {
    Tool::builder()
        .name("task_update")
        .description(
            r#"MANDATORY: Update task status after a step. Input: {"updates": [{"name": "...", "status": "completed"}]}"#,
        )
        .input_schema(schemars::schema_for!(TaskUpdateList))
        .execute(ToolExecute::from_sync(move |_ctx, params| {
            let input: TaskUpdateList = serde_json::from_value(params.clone())
                .map_err(|e| format!("Invalid input: {e}"))?;

            for update in input.updates {
                let _ = pool.update_task_status(&session_id, &update.name, update.status);
            }

            Ok(json!({ "status": "ok" }).to_string())
        }))
        .build()
        .context("failed to build task_update tool")
}

fn build_task_list(pool: Arc<DbPool>, session_id: String) -> anyhow::Result<Tool> {
    Tool::builder()
        .name("task_list")
        .description("List all tasks and their current status for this session.")
        .input_schema(schemars::schema_for!(GetTasksInput))
        .execute(ToolExecute::from_sync(move |_ctx, _params| {
            let tasks = pool.load_tasks(&session_id).unwrap_or_default();
            Ok(json!({ "tasks": tasks }).to_string())
        }))
        .build()
        .context("failed to build task_list tool")
}

pub fn task_tools(pool: Arc<DbPool>, session_id: String) -> anyhow::Result<Vec<Tool>> {
    Ok(vec![
        build_task_add(pool.clone(), session_id.clone())?,
        build_task_update(pool.clone(), session_id.clone())?,
        build_task_list(pool, session_id)?,
    ])
}
