use aisdk::core::tools::{Tool, ToolExecute};
use anyhow::Context;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::{Arc, Mutex};

/// Shared task list handle used across handler, subagent, and TUI.
pub type SharedTaskList = Arc<Mutex<TaskList>>;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Serialize, JsonSchema, Deserialize)]
pub struct Task {
    pub name: String,
    #[serde(default = "default_status")]
    pub status: TaskStatus,
}

fn default_status() -> TaskStatus {
    TaskStatus::Pending
}

#[derive(Debug, Clone, JsonSchema, Default, Serialize, Deserialize)]
pub struct TaskList {
    pub tasks: Vec<Task>,
}

impl TaskList {
    pub fn current_task(&self) -> Option<&Task> {
        self.tasks
            .iter()
            .find(|t| t.status == TaskStatus::InProgress)
    }

    pub fn active_tasks(&self) -> Vec<String> {
        self.tasks
            .iter()
            .filter(|t| t.status == TaskStatus::InProgress)
            .map(|t| t.name.clone())
            .collect()
    }

    pub fn progress_summary(&self) -> String {
        let total = self.tasks.len();
        if total == 0 {
            return String::new();
        }
        let done = self
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Completed)
            .count();
        match self.current_task() {
            Some(task) => format!("{}/{total}: {}", done.saturating_add(1), task.name),
            None => format!("{done}/{total}"),
        }
    }

    fn remaining(&self) -> usize {
        self.tasks
            .iter()
            .filter(|t| {
                !matches!(
                    t.status,
                    TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Skipped
                )
            })
            .count()
    }

    /// Upsert tasks by name (case-insensitive). Returns names that were processed.
    fn upsert(&mut self, incoming: Vec<Task>) -> Vec<String> {
        let added: Vec<String> = incoming.iter().map(|t| t.name.clone()).collect();

        for t in incoming {
            let title = t.name.trim();
            if title.is_empty() {
                continue;
            }
            if let Some(existing) = self
                .tasks
                .iter_mut()
                .find(|e| e.name.trim().eq_ignore_ascii_case(title))
            {
                existing.status = t.status;
            } else {
                self.tasks.push(Task {
                    name: title.to_string(),
                    status: t.status,
                });
            }
        }

        added
    }

    /// Update task statuses by name (case-insensitive).
    fn update_statuses(&mut self, updates: &[TaskUpdate]) {
        for update in updates {
            let target = update.name.trim();
            if let Some(task) = self
                .tasks
                .iter_mut()
                .find(|t| t.name.trim().eq_ignore_ascii_case(target))
            {
                task.status = update.status.clone();
            }
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct GetTasksInput {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskUpdate {
    #[serde(alias = "title")]
    pub name: String,
    pub status: TaskStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskUpdateList {
    pub updates: Vec<TaskUpdate>,
}

// ── Tool builders ──────────────────────────────────────────────────

fn build_task_add(state: SharedTaskList) -> anyhow::Result<Tool> {
    Tool::builder()
        .name("task_add")
        .description(
            "REQUIRED FIRST CALL. Create ALL tasks before acting. \
             Input: {\"tasks\": [{\"name\": \"step description\", \"status\": \"in_progress\"}, ...]} \
             First task: in_progress, rest: pending.",
        )
        .input_schema(schemars::schema_for!(TaskList))
        .execute(ToolExecute::from_sync(move |_ctx, params| {
            crate::tools::emit_tool_input("task_add", &params);
            let input: TaskList = serde_json::from_value(params.clone())
                .map_err(|e| format!("Invalid task_add input: {e}"))?;

            let mut guard = crate::tools::safe_lock(&state);
            let added = guard.upsert(input.tasks);

            Ok(json!({
                "status": "ok",
                "added": added,
                "remaining": guard.remaining()
            })
            .to_string())
        }))
        .build()
        .context("failed to build task_add tool")
}

fn build_task_update(state: SharedTaskList) -> anyhow::Result<Tool> {
    Tool::builder()
        .name("task_update")
        .description(
            "REQUIRED after each step. Mark completed + next in_progress in one call. \
             Input: {\"updates\": [{\"name\": \"done task\", \"status\": \"completed\"}, {\"name\": \"next task\", \"status\": \"in_progress\"}]}",
        )
        .input_schema(schemars::schema_for!(TaskUpdateList))
        .execute(ToolExecute::from_sync(move |_ctx, params| {
            crate::tools::emit_tool_input("task_update", &params);
            let input: TaskUpdateList = serde_json::from_value(params.clone())
                .map_err(|e| format!("Invalid task_update input: {e}"))?;

            let updated: Vec<serde_json::Value> = input
                .updates
                .iter()
                .map(|u| json!({"name": u.name, "status": u.status}))
                .collect();

            let mut guard = crate::tools::safe_lock(&state);
            guard.update_statuses(&input.updates);

            Ok(json!({
                "status": "ok",
                "updated": updated,
                "remaining": guard.remaining()
            })
            .to_string())
        }))
        .build()
        .context("failed to build task_update tool")
}

fn build_task_list(state: SharedTaskList) -> anyhow::Result<Tool> {
    Tool::builder()
        .name("task_list")
        .description("List all tasks and their current status.")
        .input_schema(schemars::schema_for!(GetTasksInput))
        .execute(ToolExecute::from_sync(move |_ctx, _params| {
            crate::tools::emit_tool_input("task_list", &serde_json::json!({}));
            let guard = crate::tools::safe_lock(&state);
            Ok(json!({ "tasks": guard.tasks }).to_string())
        }))
        .build()
        .context("failed to build task_list tool")
}

pub fn task_tools(state: &SharedTaskList) -> anyhow::Result<Vec<Tool>> {
    let s = state.clone();
    Ok(vec![
        build_task_add(s.clone())?,
        build_task_update(s.clone())?,
        build_task_list(s)?,
    ])
}
