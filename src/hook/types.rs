use anyhow::Result;
use futures::future::join_all;
use serde::{Deserialize, Serialize};
use std::process::Stdio;
use strum::{AsRefStr, Display, EnumString};
use tokio::process::Command;

#[derive(
    Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, AsRefStr, Display, EnumString,
)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    #[serde(rename = "prompt.pre")]
    #[strum(serialize = "prompt.pre")]
    PrePrompt,
    #[serde(rename = "tool.pre")]
    #[strum(serialize = "tool.pre")]
    PreToolUse,
    #[serde(rename = "tool.post")]
    #[strum(serialize = "tool.post")]
    PostToolUse,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HookType {
    Cmd,
    Action,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum OnFailure {
    Abort,
    #[default]
    Warn,
    Continue,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum HookScope {
    #[default]
    Validation,
    Transform,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct HookMatcher {
    pub tools: Option<Vec<String>>,
    pub file_pattern: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Hook {
    pub name: String,
    pub event: HookEvent,
    #[serde(rename = "type")]
    pub kind: HookType,
    pub handler: String,
    pub matcher: Option<HookMatcher>,
    #[serde(default)]
    pub on_failure: OnFailure,
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub scope: HookScope,
}

#[derive(Debug, Clone)]
pub enum HookContextData {
    Prompt {
        system: String,
    },
    Tool {
        tool: String,
        input: serde_json::Value,
        output: Option<serde_json::Value>,
    },
}

impl HookContext {
    /// Returns a standardized JSON object for this context.
    pub fn standardized_data(&self) -> serde_json::Value {
        match &self.data {
            HookContextData::Prompt { system } => serde_json::json!({ "system": system }),
            HookContextData::Tool {
                tool,
                input,
                output,
            } => {
                if let Some(out) = output {
                    serde_json::json!({ "tool": tool, "input": input, "output": out })
                } else {
                    serde_json::json!({ "tool": tool, "input": input })
                }
            }
        }
    }

    pub fn tool_name(&self) -> Option<&str> {
        match &self.data {
            HookContextData::Tool { tool, .. } => Some(tool),
            HookContextData::Prompt { .. } => None,
        }
    }
}

impl Hook {
    pub async fn execute(
        &self,
        context: &HookContext,
        global_timeout_ms: u64,
    ) -> Result<HookOutcome> {
        tracing::debug!(hook = %self.name, event = %context.event, scope = ?self.scope, "executing hook");

        let timeout_ms = self.timeout_ms.unwrap_or(global_timeout_ms);

        let result = match self.kind {
            HookType::Cmd | HookType::Action => {
                tokio::time::timeout(
                    std::time::Duration::from_millis(timeout_ms),
                    self.execute_cmd(context),
                )
                .await
            }
        };

        match result {
            Ok(Ok(output)) => {
                let exit_code = output.status.code();
                let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

                Ok(HookOutcome::new(self, exit_code, stdout, stderr))
            }
            Ok(Err(e)) => Ok(HookOutcome::new(self, None, String::new(), e.to_string())),
            Err(_) => Ok(HookOutcome::new(
                self,
                None,
                String::new(),
                "hook timed out".to_string(),
            )),
        }
    }

    async fn execute_cmd(&self, context: &HookContext) -> Result<std::process::Output> {
        let input_json = serde_json::to_string(&context.standardized_data())?;

        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(&self.handler)
            .env("PIE_EVENT", context.event.to_string())
            .env("PIE_HOOK_NAME", &self.name)
            .env("PIE_HOOK_SCOPE", format!("{:?}", self.scope).to_lowercase())
            .env("PIE_CWD", &context.cwd)
            .env("PIE_SESSION_ID", &context.session_id)
            .env("PIE_INPUT", &input_json)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if let HookContextData::Tool { tool, output, .. } = &context.data {
            cmd.env("PIE_TOOL", tool);
            if let Some(out) = output {
                cmd.env("PIE_OUTPUT", serde_json::to_string(out)?);
            }
        }

        // Only pipe stdin for Action hooks
        if self.kind == HookType::Action {
            cmd.stdin(Stdio::piped());
            let mut child = cmd.spawn()?;
            if let Some(mut stdin) = child.stdin.take() {
                use tokio::io::AsyncWriteExt;
                stdin.write_all(input_json.as_bytes()).await?;
                stdin.flush().await?;
            }
            let output = child.wait_with_output().await?;
            Ok(output)
        } else {
            cmd.stdin(Stdio::null());
            let output = cmd.spawn()?.wait_with_output().await?;
            Ok(output)
        }
    }

    pub fn matches(&self, context: &HookContext) -> bool {
        let Some(matcher) = &self.matcher else {
            return true;
        };

        if let Some(tools) = &matcher.tools {
            let Some(tool) = context.tool_name() else {
                return false;
            };
            if !tools.contains(&tool.to_string()) {
                return false;
            }
        }

        if let Some(pattern) = &matcher.file_pattern {
            let data = context.standardized_data();
            let path = data
                .get("path")
                .or_else(|| data.get("input").and_then(|i| i.get("path")))
                .and_then(|v| v.as_str());

            if !path.is_some_and(|p| Self::glob_match(pattern, p)) {
                return false;
            }
        }

        true
    }

    fn glob_match(pattern: &str, path: &str) -> bool {
        glob::Pattern::new(pattern).is_ok_and(|g| g.matches(path))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HookOutcome {
    Success,
    Warning {
        name: String,
        exit_code: Option<i32>,
        message: String,
    },
    Error {
        name: String,
        exit_code: Option<i32>,
        message: String,
    },
    Transformed {
        name: String,
        data: serde_json::Value,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ActionDecision {
    Allow,
    Block,
    Deny,
    Ask,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionOutput {
    pub decision: Option<ActionDecision>,
    pub message: Option<String>,
    pub updated_input: Option<serde_json::Value>,
}

impl HookOutcome {
    pub fn new(hook: &Hook, exit_code: Option<i32>, stdout: String, stderr: String) -> Self {
        // 1. Try to parse structured JSON from stdout (Claude-compatible format)
        if !stdout.is_empty()
            && let Ok(json_val) = serde_json::from_str::<serde_json::Value>(&stdout)
        {
            // Try to parse as ActionOutput to see if it has control fields
            if let Ok(action) = serde_json::from_value::<ActionOutput>(json_val.clone()) {
                // Handle Explicit Decision
                if let Some(decision) = &action.decision {
                    match decision {
                        ActionDecision::Block | ActionDecision::Deny => {
                            return HookOutcome::Error {
                                name: hook.name.clone(),
                                exit_code,
                                message: format!(
                                    "Operation blocked by decision:\n{}",
                                    action.message.unwrap_or(stdout)
                                ),
                            };
                        }
                        ActionDecision::Allow if action.updated_input.is_none() => {
                            return HookOutcome::Success;
                        }
                        _ => {} // Fall through to transformation or other logic
                    }
                }

                // Handle Transformation (via updatedInput or direct object)
                if hook.scope == HookScope::Transform {
                    let data = action.updated_input.unwrap_or(json_val);
                    return HookOutcome::Transformed {
                        name: hook.name.clone(),
                        data,
                    };
                }

                // If it was just an "allow" (with or without decision field)
                // and we didn't transform anything, it's a success
                if matches!(action.decision, Some(ActionDecision::Allow)) {
                    return HookOutcome::Success;
                }
            }
        }

        // 2. Fallback to exit-code based logic
        if exit_code == Some(0) {
            if hook.scope == HookScope::Transform && !stdout.is_empty() {
                // If we reach here, stdout wasn't valid JSON but the hook is a transform.
                return HookOutcome::Error {
                    name: hook.name.clone(),
                    exit_code,
                    message: format!(
                        "Transform hook failed to return valid JSON. Captured output:\n{stdout}"
                    ),
                };
            }
            return HookOutcome::Success;
        }

        let combined_output = if stderr.is_empty() {
            stdout
        } else if stdout.is_empty() {
            stderr
        } else {
            format!("{stdout}\n{stderr}")
        };

        let is_rejection = matches!(exit_code, Some(2 | 64 | 65 | 77));
        let on_failure =
            if hook.scope == HookScope::Validation && hook.on_failure == OnFailure::default() {
                OnFailure::Abort
            } else {
                hook.on_failure
            };

        let should_abort = on_failure == OnFailure::Abort || is_rejection;

        if should_abort {
            HookOutcome::Error {
                name: hook.name.clone(),
                exit_code,
                message: if is_rejection {
                    format!("Operation blocked:\n{combined_output}")
                } else {
                    combined_output
                },
            }
        } else if on_failure == OnFailure::Warn {
            HookOutcome::Warning {
                name: hook.name.clone(),
                exit_code,
                message: combined_output,
            }
        } else {
            HookOutcome::Success
        }
    }

    pub fn format(&self) -> String {
        match self {
            HookOutcome::Success => String::new(),
            HookOutcome::Transformed { name, .. } => {
                format!("[Hook: {name}] Data transformed successfully")
            }
            HookOutcome::Warning {
                name,
                exit_code,
                message,
            } => format!(
                "[Hook: {}] Warning (exit code {}):\n{}",
                name,
                exit_code.unwrap_or(-1),
                message.trim()
            ),
            HookOutcome::Error {
                name,
                exit_code,
                message,
            } => format!(
                "[Hook: {}] Error (exit code {}):\n{}",
                name,
                exit_code.unwrap_or(-1),
                message.trim()
            ),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct HooksManager {
    pub hooks: Vec<Hook>,
    pub timeout_ms: u64,
}

impl HooksManager {
    pub fn new(hooks: Vec<Hook>, timeout_ms: Option<u64>) -> Self {
        Self {
            hooks,
            timeout_ms: timeout_ms.unwrap_or(1000),
        }
    }

    pub async fn run(
        &self,
        event: HookEvent,
        context: &HookContext,
    ) -> Result<(Vec<HookOutcome>, serde_json::Value)> {
        let applicable_hooks: Vec<&Hook> = self
            .hooks
            .iter()
            .filter(|h| h.event == event && h.matches(context))
            .collect();

        if applicable_hooks.is_empty() {
            return Ok((Vec::new(), context.standardized_data()));
        }

        let mut all_outcomes = Vec::new();
        let mut current_data = context.standardized_data();

        // Group hooks by scope
        let validations: Vec<&Hook> = applicable_hooks
            .iter()
            .filter(|h| h.scope == HookScope::Validation)
            .copied()
            .collect();
        let transforms: Vec<&Hook> = applicable_hooks
            .iter()
            .filter(|h| h.scope == HookScope::Transform)
            .copied()
            .collect();

        // 1. Run Validation hooks in parallel
        if !validations.is_empty() {
            let mut futures = Vec::new();
            for hook in validations {
                futures.push(hook.execute(context, self.timeout_ms));
            }

            let results = join_all(futures).await;
            for result in results {
                let outcome = result?;
                all_outcomes.push(outcome);
            }

            // If any validation failed with Error, return early
            if all_outcomes
                .iter()
                .any(|o| matches!(o, HookOutcome::Error { .. }))
            {
                return Ok((all_outcomes, current_data));
            }
        }

        // 2. Run Transform hooks sequentially
        for hook in transforms {
            // Create a new context with the current (potentially transformed) data
            let transform_context = HookContext {
                event: context.event,
                cwd: context.cwd.clone(),
                session_id: context.session_id.clone(),
                data: match &context.data {
                    HookContextData::Prompt { .. } => HookContextData::Prompt {
                        system: current_data
                            .get("system")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                    },
                    HookContextData::Tool { tool, .. } => HookContextData::Tool {
                        tool: current_data
                            .get("tool")
                            .and_then(|v| v.as_str())
                            .unwrap_or(tool)
                            .to_string(),
                        input: current_data
                            .get("input")
                            .cloned()
                            .unwrap_or_else(|| current_data.clone()),
                        output: current_data.get("output").cloned(),
                    },
                },
            };

            let outcome = hook.execute(&transform_context, self.timeout_ms).await?;
            if let HookOutcome::Transformed { data, .. } = &outcome {
                current_data = data.clone();
            }
            all_outcomes.push(outcome);

            // If a transformation failed with Error, stop processing
            if all_outcomes
                .iter()
                .any(|o| matches!(o, HookOutcome::Error { .. }))
            {
                break;
            }
        }

        Ok((all_outcomes, current_data))
    }
}

#[derive(Debug, Clone)]
pub struct HookContext {
    pub event: HookEvent,
    pub cwd: String,
    pub session_id: String,
    pub data: HookContextData,
}
