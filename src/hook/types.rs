use anyhow::Result;
use futures::future::join_all;
use serde::{Deserialize, Serialize};
use std::process::Stdio;
use std::sync::Arc;
use strum::{AsRefStr, Display, EnumString};
use tokio::process::Command;

#[derive(
    Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, AsRefStr, Display, EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
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

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default, Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum HookType {
    #[default]
    Cmd,
    Action,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default, Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum OnFailure {
    Abort,
    #[default]
    Warn,
    Continue,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default, Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
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

/// Raw hook definition deserialized from TOML config.
/// Cannot be executed directly — build a [`Hook`] via [`From<HookDef>`].
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HookDef {
    pub name: String,
    pub event: HookEvent,
    #[serde(rename = "type", default)]
    pub kind: HookType,
    #[serde(default)]
    pub handler: String,
    pub matcher: Option<HookMatcher>,
    #[serde(default)]
    pub on_failure: OnFailure,
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub scope: HookScope,
    #[serde(skip)]
    pub plugin_dir: Option<String>,
}

/// Pre-computed environment for CLI hook execution.
#[derive(Clone)]
struct CmdEnv {
    handler: String,
    is_action: bool,
    env_vars: Vec<(String, String)>,
    path_override: Option<std::ffi::OsString>,
}

/// Runtime hook with an pre-computed environment.
/// Built from [`HookDef`] via [`From<HookDef>`].
pub struct Hook {
    pub name: String,
    pub event: HookEvent,
    pub matcher: Option<HookMatcher>,
    pub on_failure: OnFailure,
    pub timeout_ms: Option<u64>,
    pub scope: HookScope,
    pub kind: HookType,
    cmd_env: CmdEnv,
}

impl Clone for Hook {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            event: self.event,
            matcher: self.matcher.clone(),
            on_failure: self.on_failure,
            timeout_ms: self.timeout_ms,
            scope: self.scope,
            kind: self.kind,
            cmd_env: self.cmd_env.clone(),
        }
    }
}

// Forward Debug manually — closures don't impl Debug.
impl std::fmt::Debug for Hook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hook")
            .field("name", &self.name)
            .field("event", &self.event)
            .field("matcher", &self.matcher)
            .field("on_failure", &self.on_failure)
            .field("timeout_ms", &self.timeout_ms)
            .field("scope", &self.scope)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl From<HookDef> for Hook {
    fn from(def: HookDef) -> Self {
        let plugin_dir = def.plugin_dir.clone();
        let cmd_env = Self::build_cmd_env(&def, plugin_dir.as_deref());

        Self {
            name: def.name,
            event: def.event,
            matcher: def.matcher,
            on_failure: def.on_failure,
            timeout_ms: def.timeout_ms,
            scope: def.scope,
            kind: def.kind,
            cmd_env,
        }
    }
}

impl Hook {
    /// Pre-compute the command environment (PATH, env vars) once.
    fn build_cmd_env(def: &HookDef, plugin_dir: Option<&str>) -> CmdEnv {
        let mut env_vars = vec![(
            "PIE_DATABASE_PATH".to_string(),
            crate::config::pie_home()
                .join("pie.db")
                .to_string_lossy()
                .to_string(),
        )];

        if let Some(dir) = plugin_dir {
            env_vars.push(("PIE_PLUGIN_DIR".to_string(), dir.to_string()));
        }

        let path_override = plugin_dir.and_then(|dir| {
            let bin_dir = std::path::PathBuf::from(dir).join("bin");
            if bin_dir.exists() {
                if let Some(old_path) = std::env::var_os("PATH") {
                    let mut paths = std::env::split_paths(&old_path).collect::<Vec<_>>();
                    paths.insert(0, bin_dir);
                    std::env::join_paths(paths).ok()
                } else {
                    Some(bin_dir.into_os_string())
                }
            } else {
                None
            }
        });

        CmdEnv {
            handler: def.handler.clone(),
            is_action: def.kind == HookType::Action,
            env_vars,
            path_override,
        }
    }

    async fn execute_cmd(&self, context: &HookContext) -> Result<HookOutcome> {
        let hook_name = self.name.clone();
        let input_json = serde_json::to_string(context.standardized_data())?;

        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(&self.cmd_env.handler)
            .env("PIE_EVENT", context.event.to_string())
            .env("PIE_HOOK_NAME", &hook_name)
            .env("PIE_HOOK_SCOPE", format!("{:?}", self.scope))
            .env("PIE_CWD", &*context.cwd)
            .env("PIE_SESSION_ID", &*context.session_id)
            .env("PIE_INPUT", &input_json);

        for (key, val) in &self.cmd_env.env_vars {
            cmd.env(key, val);
        }

        if let Some(path) = &self.cmd_env.path_override {
            cmd.env("PATH", path);
        }

        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        if let HookContextData::Tool { tool, output, .. } = &context.data {
            cmd.env("PIE_TOOL", tool);
            if let Some(out) = output {
                cmd.env("PIE_OUTPUT", serde_json::to_string(out)?);
            }
        }

        let output = if self.cmd_env.is_action {
            cmd.stdin(Stdio::piped());
            let mut child = cmd.spawn()?;
            if let Some(mut stdin) = child.stdin.take() {
                use tokio::io::AsyncWriteExt;
                stdin.write_all(input_json.as_bytes()).await?;
                stdin.flush().await?;
            }
            child.wait_with_output().await?
        } else {
            cmd.stdin(Stdio::null());
            cmd.spawn()?.wait_with_output().await?
        };

        let exit_code = output.status.code();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

        Ok(HookOutcome::from_cmd(
            &hook_name,
            exit_code,
            stdout,
            stderr,
            context.data.is_tool(),
        ))
    }

    pub async fn execute(
        &self,
        context: &HookContext,
        global_timeout_ms: u64,
    ) -> Result<HookOutcome> {
        let start = std::time::Instant::now();
        tracing::debug!(event = %context.event, scope = ?self.scope, hook = %self.name, "HOOK STARTING");

        let timeout_ms = self.timeout_ms.unwrap_or(global_timeout_ms);
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(timeout_ms),
            self.execute_cmd(context),
        )
        .await;

        let duration = start.elapsed();

        match result {
            Ok(Ok(outcome)) => {
                tracing::debug!(
                    hook = %self.name,
                    duration = ?duration,
                    outcome = ?outcome,
                    "HOOK COMPLETED"
                );
                Ok(outcome)
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    hook = %self.name,
                    duration = ?duration,
                    error = %e,
                    "HOOK FAILED"
                );
                Ok(HookOutcome::Warning {
                    name: self.name.clone(),
                    exit_code: None,
                    message: e.to_string(),
                })
            }
            Err(_) => {
                tracing::warn!(
                    hook = %self.name,
                    duration = ?duration,
                    "HOOK TIMED OUT"
                );
                Ok(HookOutcome::Warning {
                    name: self.name.clone(),
                    exit_code: None,
                    message: "hook timed out".to_string(),
                })
            }
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

// ── Context ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct HookContext {
    pub event: HookEvent,
    pub cwd: Arc<str>,
    pub session_id: Arc<str>,
    pub data: HookContextData,
    /// Cached JSON representation to avoid repeated serialization.
    cached_json: Arc<std::sync::OnceLock<serde_json::Value>>,
}

#[derive(Debug, Clone)]
pub enum HookContextData {
    Prompt {
        system: String,
        query: String,
    },
    Tool {
        tool: String,
        input: serde_json::Value,
        output: Option<serde_json::Value>,
    },
}

impl HookContextData {
    fn is_tool(&self) -> bool {
        matches!(self, HookContextData::Tool { .. })
    }
}

impl HookContext {
    pub fn new(event: HookEvent, cwd: String, session_id: String, data: HookContextData) -> Self {
        Self {
            event,
            cwd: cwd.into(),
            session_id: session_id.into(),
            data,
            cached_json: Arc::new(std::sync::OnceLock::new()),
        }
    }

    pub fn standardized_data(&self) -> &serde_json::Value {
        self.cached_json.get_or_init(|| match &self.data {
            HookContextData::Prompt { system, query } => {
                serde_json::json!({ "system": system, "query": query })
            }
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
        })
    }

    pub fn tool_name(&self) -> Option<&str> {
        match &self.data {
            HookContextData::Tool { tool, .. } => Some(tool),
            HookContextData::Prompt { .. } => None,
        }
    }
}

// ── Outcomes ──────────────────────────────────────────────────────────

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
pub struct ActionOutput {
    pub decision: Option<ActionDecision>,
    pub message: Option<String>,
    pub updated_input: Option<serde_json::Value>,
}

impl HookOutcome {
    /// Parse CLI command output into a hook outcome.
    fn from_cmd(
        name: &str,
        exit_code: Option<i32>,
        stdout: String,
        stderr: String,
        is_tool: bool,
    ) -> Self {
        if !stdout.is_empty()
            && let Ok(json_val) = serde_json::from_str::<serde_json::Value>(&stdout)
            && let Ok(action) = serde_json::from_value::<ActionOutput>(json_val.clone())
        {
            if let Some(decision) = &action.decision {
                match decision {
                    ActionDecision::Block | ActionDecision::Deny => {
                        return HookOutcome::Error {
                            name: name.to_string(),
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
                    _ => {}
                }
            }

            if let Some(data) = action.updated_input {
                return HookOutcome::Transformed {
                    name: name.to_string(),
                    data,
                };
            }

            // Direct JSON object as transform
            if is_tool {
                return HookOutcome::Transformed {
                    name: name.to_string(),
                    data: json_val,
                };
            }

            if matches!(action.decision, Some(ActionDecision::Allow)) {
                return HookOutcome::Success;
            }
        }

        // Exit-code based fallback
        if exit_code == Some(0) {
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

        if is_rejection {
            HookOutcome::Error {
                name: name.to_string(),
                exit_code,
                message: format!("Operation blocked:\n{combined_output}"),
            }
        } else {
            HookOutcome::Warning {
                name: name.to_string(),
                exit_code,
                message: combined_output,
            }
        }
    }

    pub fn format(&self) -> String {
        match self {
            HookOutcome::Success => String::new(),
            HookOutcome::Transformed { name, .. } => {
                format!("[Hook: {name}] Transformed data")
            }
            HookOutcome::Warning {
                name,
                exit_code,
                message,
            } => {
                format!(
                    "[Hook: {}] Warning (exit code {}):\n{}",
                    name,
                    exit_code.unwrap_or(-1),
                    message.trim()
                )
            }
            HookOutcome::Error {
                name,
                exit_code,
                message,
            } => {
                format!(
                    "[Hook: {}] Error (exit code {}):\n{}",
                    name,
                    exit_code.unwrap_or(-1),
                    message.trim()
                )
            }
        }
    }
}

// ── Manager ───────────────────────────────────────────────────────────

#[derive(Debug, Default)]
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
        let applicable: Vec<&Hook> = self
            .hooks
            .iter()
            .filter(|h| h.event == event && h.matches(context))
            .collect();

        if applicable.is_empty() {
            return Ok((Vec::new(), context.standardized_data().clone()));
        }

        let mut all_outcomes = Vec::new();
        let mut current_data = context.standardized_data().clone();

        let validations: Vec<&Hook> = applicable
            .iter()
            .filter(|h| h.scope == HookScope::Validation)
            .copied()
            .collect();
        let transforms: Vec<&Hook> = applicable
            .iter()
            .filter(|h| h.scope == HookScope::Transform)
            .copied()
            .collect();

        // 1. Validation hooks in parallel
        if !validations.is_empty() {
            let futures: Vec<_> = validations
                .iter()
                .map(|h| h.execute(context, self.timeout_ms))
                .collect();
            let results = join_all(futures).await;
            for result in results {
                all_outcomes.push(result?);
            }

            if all_outcomes
                .iter()
                .any(|o| matches!(o, HookOutcome::Error { .. }))
            {
                return Ok((all_outcomes, current_data));
            }
        }

        // 2. Transform hooks sequentially
        for hook in transforms {
            let transform_context = HookContext {
                event: context.event,
                cwd: context.cwd.clone(),
                session_id: context.session_id.clone(),
                data: match &context.data {
                    HookContextData::Prompt { query, .. } => HookContextData::Prompt {
                        system: current_data
                            .get("system")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        query: query.clone(),
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
                cached_json: Arc::new(std::sync::OnceLock::new()),
            };

            let outcome = hook.execute(&transform_context, self.timeout_ms).await?;
            if let HookOutcome::Transformed { data, .. } = &outcome {
                current_data = data.clone();
            }
            all_outcomes.push(outcome);

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
