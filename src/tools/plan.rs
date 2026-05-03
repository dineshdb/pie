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
pub enum StepStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Serialize, JsonSchema, Deserialize, PartialEq, Eq)]
pub struct Step {
    pub id: Option<i64>,
    pub name: String,
    #[serde(default)]
    pub status: StepStatus,
}

pub trait PlanRepo {
    fn load_steps(&self, session_id: &str) -> anyhow::Result<Vec<Step>>;
    fn save_step(&self, session_id: &str, step: &Step) -> anyhow::Result<i64>;
    fn update_step_status(
        &self,
        session_id: &str,
        name: &str,
        status: StepStatus,
    ) -> anyhow::Result<()>;
    fn delete_steps(&self, session_id: &str) -> anyhow::Result<()>;
}

impl PlanRepo for DbPool {
    fn load_steps(&self, session_id: &str) -> anyhow::Result<Vec<Step>> {
        let conn = self.get()?;
        let mut stmt = conn.prepare(
            "SELECT id, name, status FROM steps WHERE session_id = ? ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            let status_str: String = row.get(2)?;
            Ok(Step {
                id: Some(row.get(0)?),
                name: row.get(1)?,
                status: status_str.parse().unwrap_or(StepStatus::Pending),
            })
        })?;

        Ok(rows.flatten().collect())
    }

    fn save_step(&self, session_id: &str, step: &Step) -> anyhow::Result<i64> {
        let conn = self.get()?;
        let status_ref: &str = step.status.as_ref();
        conn.execute(
            "INSERT INTO steps (session_id, name, status, updated_at)
             VALUES (?1, ?2, ?3, unixepoch('subsec') * 1000)
             ON CONFLICT(session_id, name) DO UPDATE SET
             status = excluded.status,
             updated_at = excluded.updated_at",
            params![session_id, step.name, status_ref],
        )?;
        Ok(conn.last_insert_rowid())
    }

    fn update_step_status(
        &self,
        session_id: &str,
        name: &str,
        status: StepStatus,
    ) -> anyhow::Result<()> {
        let conn = self.get()?;
        let status_ref: &str = status.as_ref();
        conn.execute(
            "UPDATE steps SET status = ?, updated_at = unixepoch('subsec') * 1000
             WHERE session_id = ? AND name = ?",
            params![status_ref, session_id, name],
        )?;
        Ok(())
    }

    fn delete_steps(&self, session_id: &str) -> anyhow::Result<()> {
        let conn = self.get()?;
        conn.execute(
            "DELETE FROM steps WHERE session_id = ?",
            params![session_id],
        )?;
        Ok(())
    }
}

pub fn enforce_planning(pool: &Arc<DbPool>, session_id: &str, tool: &str) -> Result<(), String> {
    let steps = pool.load_steps(session_id).unwrap_or_default();
    if steps.is_empty() {
        return Err(format!(
            "CRITICAL ERROR: You called '{tool}' without a plan. \
             You MUST call 'plan_set' with a full plan before taking any actions. \
             This is your CORE MANDATE for reliability."
        ));
    }
    Ok(())
}

#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct StepListInput {
    pub steps: Vec<Step>,
}

#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct GetStepsInput {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StepUpdate {
    pub name: String,
    pub status: StepStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StepUpdateList {
    pub updates: Vec<StepUpdate>,
}

// ── Tool builders ──────────────────────────────────────────────────

fn build_plan_set(pool: Arc<DbPool>, session_id: String) -> anyhow::Result<Tool> {
    Tool::builder()
        .name("plan_set")
        .description(
            r#"CRITICAL: Planning phase. Call this FIRST to define the plan steps. Input: {"steps": [{"name": "..."}]}"#,
        )
        .input_schema(schemars::schema_for!(StepListInput))
        .execute(ToolExecute::from_sync(move |_ctx, params| {
            let input: StepListInput = serde_json::from_value(params.clone())
                .map_err(|e| format!("Invalid input: {e}"))?;

            for t in input.steps {
                let _ = pool.save_step(&session_id, &t);
            }

            Ok(json!({ "status": "ok" }).to_string())
        }))
        .build()
        .context("failed to build plan_set tool")
}

fn build_plan_step_update(pool: Arc<DbPool>, session_id: String) -> anyhow::Result<Tool> {
    Tool::builder()
        .name("plan_step_update")
        .description(
            r#"MANDATORY: Update a plan step status. Input: {"updates": [{"name": "...", "status": "completed"}]}"#,
        )
        .input_schema(schemars::schema_for!(StepUpdateList))
        .execute(ToolExecute::from_sync(move |_ctx, params| {
            let input: StepUpdateList = serde_json::from_value(params.clone())
                .map_err(|e| format!("Invalid input: {e}"))?;

            for update in input.updates {
                let _ = pool.update_step_status(&session_id, &update.name, update.status);
            }

            Ok(json!({ "status": "ok" }).to_string())
        }))
        .build()
        .context("failed to build plan_step_update tool")
}

fn build_plan_show(pool: Arc<DbPool>, session_id: String) -> anyhow::Result<Tool> {
    Tool::builder()
        .name("plan_show")
        .description("List the current plan steps and their status.")
        .input_schema(schemars::schema_for!(GetStepsInput))
        .execute(ToolExecute::from_sync(move |_ctx, _params| {
            let steps = pool.load_steps(&session_id).unwrap_or_default();
            Ok(json!({ "steps": steps }).to_string())
        }))
        .build()
        .context("failed to build plan_show tool")
}

pub fn plan_tools(pool: Arc<DbPool>, session_id: String) -> anyhow::Result<Vec<Tool>> {
    Ok(vec![
        build_plan_set(pool.clone(), session_id.clone())?,
        build_plan_step_update(pool.clone(), session_id.clone())?,
        build_plan_show(pool, session_id)?,
    ])
}
