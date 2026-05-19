use crate::db::DbPool;
use agentsdk::core::tools::{Tool, ToolDefinition, ToolExecute};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::future::Future;
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

#[derive(JsonSchema, Deserialize, Serialize)]
struct PlanSetInput {
    /// List of steps for the plan.
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StepUpdate {
    pub name: String,
    pub status: StepStatus,
}

#[derive(JsonSchema, Deserialize, Serialize)]
struct PlanStepUpdateInput {
    /// List of updates to apply to the plan steps.
    pub updates: Vec<StepUpdate>,
}

pub fn plan_set_tool(pool: Arc<DbPool>, session_id: String) -> anyhow::Result<Tool> {
    Ok(Tool::builder()
        .definition(
            ToolDefinition::builder()
                .name("plan_set")
                .description("CRITICAL: Planning phase. Call this FIRST to define the plan steps")
                .input_schema(schema_for!(PlanSetInput))
                .build()?,
        )
        .execute(ToolExecute::from_async(move |_ctx, params| {
            let pool = pool.clone();
            let session_id = session_id.clone();
            async move {
                let input: PlanSetInput =
                    serde_json::from_value(params).map_err(|e| e.to_string())?;

                for t in input.steps {
                    let _ = pool.save_step(&session_id, &t).await;
                }

                Ok(json!({ "status": "ok" }))
            }
        }))
        .build()?)
}

/// MANDATORY: Update a plan step status.
pub fn plan_step_update_tool(pool: Arc<DbPool>, session_id: String) -> anyhow::Result<Tool> {
    Ok(Tool::builder()
        .definition(
            ToolDefinition::builder()
                .name("plan_step_update")
                .description("MANDATORY: Update a plan step status")
                .input_schema(schema_for!(PlanStepUpdateInput))
                .build()?,
        )
        .execute(ToolExecute::from_async(move |_ctx, params| {
            let pool = pool.clone();
            let session_id = session_id.clone();
            async move {
                let input: PlanStepUpdateInput =
                    serde_json::from_value(params).map_err(|e| e.to_string())?;

                for update in input.updates {
                    let _ = pool
                        .update_step_status(&session_id, &update.name, update.status)
                        .await;
                }

                Ok(json!({ "status": "ok" }))
            }
        }))
        .build()?)
}

/// Build the plan-related tools.
pub fn plan_tools(pool: Arc<DbPool>, session_id: &str) -> anyhow::Result<Vec<Tool>> {
    let sid = session_id.to_string();
    Ok(vec![
        plan_set_tool(pool.clone(), sid.clone())?,
        plan_step_update_tool(pool, sid)?,
    ])
}
