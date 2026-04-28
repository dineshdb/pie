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
    pub title: String,
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
            .map(|t| t.title.clone())
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
        let current = self.current_task();
        match current {
            Some(task) => format!("{}/{total}: {}", done.saturating_add(1), task.title),
            None => format!("{done}/{total}"),
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct GetTasksInput {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskUpdate {
    pub title: String,
    pub status: TaskStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskUpdateList {
    pub updates: Vec<TaskUpdate>,
}

pub fn task_tools(state: &SharedTaskList) -> anyhow::Result<Vec<Tool>> {
    let task_add = {
        let state = state.clone();
        Tool::builder()
            .name("task_add")
            .description(
                "Create tasks for your execution plan. Call this FIRST before acting. \
                 Break the request into discrete, verifiable steps. \
                 Re-calling with an existing title updates its status.",
            )
            .input_schema(schemars::schema_for!(TaskList))
            .execute(ToolExecute::from_sync(move |_ctx, params| {
                let input: TaskList = serde_json::from_value(params.clone())
                    .map_err(|e| format!("Invalid task_add input: {e}"))?;

                let mut guard = crate::tools::safe_lock(&state);

                for t in input.tasks {
                    let title = t.title.trim();
                    if title.is_empty() {
                        continue;
                    }

                    // Find existing task by title (case-insensitive)
                    let existing = guard
                        .tasks
                        .iter_mut()
                        .find(|existing| existing.title.trim().eq_ignore_ascii_case(title));

                    if let Some(task) = existing {
                        task.status = t.status;
                    } else {
                        guard.tasks.push(Task {
                            title: title.to_string(),
                            status: t.status,
                        });
                    }
                }

                Ok(json!({
                    "status": "ok",
                    "remaining": guard.tasks.iter().filter(|t| !matches!(t.status, TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Skipped)).count()
                })
                .to_string())
            }))
            .build()
            .context("failed to build task_add tool")?
    };

    let update_task = {
        let state = state.clone();
        Tool::builder()
            .name("task_update")
            .description(
                "Mark task status as you progress. \
                 Always mark the completed task as 'completed' and the next as 'in_progress' in the same call.",
            )
            .input_schema(schemars::schema_for!(TaskUpdateList))
            .execute(ToolExecute::from_sync(move |_ctx, params| {
                let input: TaskUpdateList = serde_json::from_value(params.clone())
                    .map_err(|e| format!("Invalid task_update input: {e}"))?;

                let mut guard = crate::tools::safe_lock(&state);
                for update in input.updates {
                    let target_title = update.title.trim();
                    if let Some(task) = guard.tasks.iter_mut().find(|t| {
                        t.title.trim().eq_ignore_ascii_case(target_title)
                    }) {
                        task.status = update.status;
                    }
                }

                let remaining = guard
                    .tasks
                    .iter()
                    .filter(|t| !matches!(t.status, TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Skipped))
                    .count();

                Ok(json!({
                    "status": "ok",
                    "remaining": remaining
                })
                .to_string())
            }))
            .build()
            .context("failed to build task_update tool")?
    };

    let get_tasks = {
        let state = state.clone();
        Tool::builder()
            .name("task_list")
            .description("List all tasks and their current status.")
            .input_schema(schemars::schema_for!(GetTasksInput))
            .execute(ToolExecute::from_sync(move |_ctx, _params| {
                let guard = crate::tools::safe_lock(&state);
                Ok(json!({ "tasks": guard.tasks }).to_string())
            }))
            .build()
            .context("failed to build task_list tool")?
    };

    Ok(vec![task_add, update_task, get_tasks])
}
