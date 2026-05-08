use crate::db::DbPool;
use agentsdk::core::tools::{Tool, ToolExecute};
use anyhow::Context;
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
    pub name: String,
    #[serde(default)]
    pub status: StepStatus,
}

pub trait PlanRepo {
    fn load_steps(
        &self,
        session_id: &str,
    ) -> impl Future<Output = anyhow::Result<Vec<Step>>> + Send;
    fn save_step(
        &self,
        session_id: &str,
        step: &Step,
    ) -> impl Future<Output = anyhow::Result<i64>> + Send;
    fn update_step_status(
        &self,
        session_id: &str,
        name: &str,
        status: StepStatus,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
    fn delete_steps(&self, session_id: &str) -> impl Future<Output = anyhow::Result<()>> + Send;
}

impl PlanRepo for DbPool {
    async fn load_steps(&self, session_id: &str) -> anyhow::Result<Vec<Step>> {
        let rows = sqlx::query_as!(
            StepRow,
            "SELECT name, status FROM steps WHERE session_id = ? ORDER BY created_at ASC",
            session_id,
        )
        .fetch_all(self)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| Step {
                name: r.name,
                status: r.status.parse().unwrap_or(StepStatus::Pending),
            })
            .collect())
    }

    async fn save_step(&self, session_id: &str, step: &Step) -> anyhow::Result<i64> {
        let status_ref: &str = step.status.as_ref();
        let result = sqlx::query!(
            r#"INSERT INTO steps (session_id, name, status, updated_at)
             VALUES (?1, ?2, ?3, unixepoch('subsec') * 1000)
             ON CONFLICT(session_id, name) DO UPDATE SET
             status = excluded.status,
             updated_at = excluded.updated_at"#,
            session_id,
            step.name,
            status_ref,
        )
        .execute(self)
        .await?;
        Ok(result.last_insert_rowid())
    }

    async fn update_step_status(
        &self,
        session_id: &str,
        name: &str,
        status: StepStatus,
    ) -> anyhow::Result<()> {
        let status_ref: &str = status.as_ref();
        sqlx::query!(
            "UPDATE steps SET status = ?, updated_at = unixepoch('subsec') * 1000
             WHERE session_id = ? AND name = ?",
            status_ref,
            session_id,
            name,
        )
        .execute(self)
        .await?;
        Ok(())
    }

    async fn delete_steps(&self, session_id: &str) -> anyhow::Result<()> {
        sqlx::query!("DELETE FROM steps WHERE session_id = ?", session_id)
            .execute(self)
            .await?;
        Ok(())
    }
}

#[derive(sqlx::FromRow)]
struct StepRow {
    name: String,
    status: String,
}

pub async fn enforce_planning(pool: &DbPool, session_id: &str, tool: &str) -> Result<(), String> {
    let steps = pool.load_steps(session_id).await.unwrap_or_default();
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
        .execute(ToolExecute::from_async(move |_ctx, params| {
            let pool = pool.clone();
            let session_id = session_id.clone();
            async move {
                let input: StepListInput = serde_json::from_value(params.clone())
                    .map_err(|e| format!("Invalid input: {e}"))?;

                for t in input.steps {
                    let _ = pool.save_step(&session_id, &t).await;
                }

                Ok(json!({ "status": "ok" }).to_string())
            }
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
        .execute(ToolExecute::from_async(move |_ctx, params| {
            let pool = pool.clone();
            let session_id = session_id.clone();
            async move {
                let input: StepUpdateList = serde_json::from_value(params.clone())
                    .map_err(|e| format!("Invalid input: {e}"))?;

                for update in input.updates {
                    let _ = pool.update_step_status(&session_id, &update.name, update.status).await;
                }

                Ok(json!({ "status": "ok" }).to_string())
            }
        }))
        .build()
        .context("failed to build plan_step_update tool")
}

fn build_plan_show(pool: Arc<DbPool>, session_id: String) -> anyhow::Result<Tool> {
    Tool::builder()
        .name("plan_show")
        .description("List the current plan steps and their status.")
        .input_schema(schemars::schema_for!(GetStepsInput))
        .execute(ToolExecute::from_async(move |_ctx, _params| {
            let pool = pool.clone();
            let session_id = session_id.clone();
            async move {
                let steps = pool.load_steps(&session_id).await.unwrap_or_default();
                Ok(json!({ "steps": steps }).to_string())
            }
        }))
        .build()
        .context("failed to build plan_show tool")
}

/// Build the plan-related tools.
///
/// # Errors
///
/// Returns an error if any of the tools cannot be built.
pub fn plan_tools(pool: Arc<DbPool>, session_id: String) -> anyhow::Result<Vec<Tool>> {
    Ok(vec![
        build_plan_set(pool.clone(), session_id.clone())?,
        build_plan_step_update(pool.clone(), session_id.clone())?,
        build_plan_show(pool, session_id)?,
    ])
}
