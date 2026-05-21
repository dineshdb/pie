use crate::agent::OutputMode;
use anyhow::Result;
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
    /// When a user sends a query, this hook can be used to
    /// - transform user query
    /// - gather extra context
    #[serde(rename = "userquery.post")]
    #[strum(serialize = "userquery.post")]
    PostUserQuery,

    /// Hook to run before creating a system prompt
    #[serde(rename = "prompt.pre")]
    #[strum(serialize = "prompt.pre")]
    PrePrompt,

    /// After the system prompt has been configured
    /// - validate the prompt
    /// - process the whole prompt in one go
    #[serde(rename = "prompt.post")]
    #[strum(serialize = "prompt.post")]
    PostPrompt,

    /// Just before tool use
    /// - validate tools
    /// - transform tools
    #[serde(rename = "tool.pre")]
    #[strum(serialize = "tool.pre")]
    PreToolUse,

    /// After tool use
    #[serde(rename = "tool.post")]
    #[strum(serialize = "tool.post")]
    PostToolUse,

    /// Just before completion
    /// - Run tests
    /// - Check if the output is satisfactory
    /// - You can ask for another loop of the agent
    #[serde(rename = "completion.pre")]
    #[strum(serialize = "completion.pre")]
    PreCompletion,

    /// After completion
    /// - send notifications
    /// - logging
    #[serde(rename = "completion.post")]
    #[strum(serialize = "completion.post")]
    PostCompletion,
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

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default, Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ExecutionStrategy {
    #[default]
    Sequential,
    Parallel,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct HookMatcher {
    pub tools: Option<Vec<String>>,
    pub file_pattern: Option<String>,
}

/// Raw hook definition deserialized from TOML config.
/// Built into a [`CommandHook`] via [`From<HookDef>`].
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
    #[serde(default)]
    pub scope: HookScope,
    #[serde(default)]
    pub strategy: ExecutionStrategy,
    #[serde(skip)]
    pub plugin_dir: Option<String>,
}

/// Pre-computed environment for CLI hook execution.
#[derive(Debug, Clone, Serialize)]
struct CmdEnv {
    handler: String,
    is_action: bool,
    #[serde(skip_serializing)]
    env_vars: Vec<(String, String)>,
    #[serde(skip_serializing)]
    path_override: Option<std::ffi::OsString>,
}

/// Runtime hook with a pre-computed environment.
#[derive(Debug, Clone, Serialize)]
pub struct CommandHook {
    pub name: String,
    pub event: HookEvent,
    pub matcher: Option<HookMatcher>,
    pub on_failure: OnFailure,
    pub scope: HookScope,
    pub strategy: ExecutionStrategy,
    cmd_env: CmdEnv,
}

impl From<HookDef> for CommandHook {
    fn from(def: HookDef) -> Self {
        let plugin_dir = def.plugin_dir.clone();
        let cmd_env = Self::build_cmd_env(&def, plugin_dir.as_deref());

        Self {
            name: def.name,
            event: def.event,
            matcher: def.matcher,
            on_failure: def.on_failure,
            scope: def.scope,
            strategy: def.strategy,
            cmd_env,
        }
    }
}

impl CommandHook {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn event(&self) -> HookEvent {
        self.event
    }

    pub fn strategy(&self) -> ExecutionStrategy {
        self.strategy
    }

    pub fn scope(&self) -> HookScope {
        self.scope
    }

    pub fn matches(&self, context: &HookContext) -> bool {
        let Some(matcher) = &self.matcher else {
            return true;
        };

        if let Some(tools) = &matcher.tools {
            let Some(tool) = context.tool_name() else {
                return false;
            };
            if !tools.iter().any(|t| t == tool) {
                return false;
            }
        }

        if let Some(pattern) = &matcher.file_pattern {
            let data = context.data_json();
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

    pub async fn on(&self, context: &HookContext) -> Result<HookOutcome> {
        self.execute_cmd(context).await
    }

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
        let input_json = serde_json::to_string(&context.data_json())?;

        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(&self.cmd_env.handler)
            .env("PIE_EVENT", context.event.to_string())
            .env("PIE_HOOK_NAME", &hook_name)
            .env("PIE_HOOK_SCOPE", format!("{:?}", self.scope))
            .env("PIE_CWD", &*context.cwd)
            .env("PIE_SESSION_ID", &*context.session_id)
            .env("PIE_OUTPUT_MODE", context.output_mode.to_string())
            .env("PIE_INPUT", &input_json);

        for (key, val) in &self.cmd_env.env_vars {
            cmd.env(key, val);
        }

        if let Some(path) = &self.cmd_env.path_override {
            cmd.env("PATH", path);
        }

        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        if let HookContextData::Tool(d) = &context.data {
            if let Some(t) = &d.tool {
                cmd.env("PIE_TOOL", t);
            }
            if let Some(out) = &d.output {
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
            &stdout,
            &stderr,
            context,
            self.on_failure,
        ))
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
    pub output_mode: OutputMode,
    pub data: HookContextData,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PromptData {
    pub system: Option<String>,
    pub query: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolData {
    pub tool: Option<String>,
    pub input: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<serde_json::Value>,
}

impl PromptData {
    pub fn merge(&mut self, delta: Self) {
        if let Some(delta_sys) = delta.system {
            let mut current_sys = self.system.take().unwrap_or_default();
            if current_sys.is_empty() {
                self.system = Some(delta_sys);
            } else {
                if !current_sys.ends_with('\n') {
                    current_sys.push('\n');
                }
                current_sys.push_str(&delta_sys);
                self.system = Some(current_sys);
            }
        }
        if let Some(delta_query) = delta.query {
            self.query = Some(delta_query);
        }
    }
}

impl ToolData {
    pub fn merge(&mut self, delta: Self) {
        if let Some(t) = delta.tool {
            self.tool = Some(t);
        }
        if let Some(i) = delta.input {
            self.input = Some(i);
        }
        if let Some(o) = delta.output {
            self.output = Some(o);
        }
    }
}

#[derive(Debug, Clone)]
pub enum HookContextData {
    Prompt(PromptData),
    Tool(ToolData),
}

impl HookContextData {
    fn is_tool(&self) -> bool {
        matches!(self, HookContextData::Tool(_))
    }

    pub(crate) fn merge(&mut self, delta_val: serde_json::Value) {
        match self {
            HookContextData::Prompt(p) => {
                if let Ok(delta) = serde_json::from_value::<PromptData>(delta_val) {
                    p.merge(delta);
                }
            }
            HookContextData::Tool(t) => {
                if let Ok(delta) = serde_json::from_value::<ToolData>(delta_val) {
                    t.merge(delta);
                }
            }
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        match self {
            HookContextData::Prompt(d) => serde_json::to_value(d).unwrap_or_default(),
            HookContextData::Tool(d) => serde_json::to_value(d).unwrap_or_default(),
        }
    }
}

impl HookContext {
    pub fn new(
        event: HookEvent,
        cwd: String,
        session_id: String,
        output_mode: OutputMode,
        data: HookContextData,
    ) -> Self {
        Self {
            event,
            cwd: cwd.into(),
            session_id: session_id.into(),
            output_mode,
            data,
        }
    }

    pub fn data_json(&self) -> serde_json::Value {
        self.data.to_json()
    }

    pub fn tool_name(&self) -> Option<&str> {
        match &self.data {
            HookContextData::Tool(d) => d.tool.as_deref(),
            HookContextData::Prompt(_) => None,
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
    fn from_cmd(
        name: &str,
        exit_code: Option<i32>,
        stdout: &str,
        stderr: &str,
        context: &HookContext,
        on_failure: OnFailure,
    ) -> Self {
        if let Some(outcome) = Self::parse_action_response(name, exit_code, stdout, context) {
            return outcome;
        }

        if exit_code == Some(0) {
            return HookOutcome::Success;
        }

        let combined = match (stdout, stderr) {
            ("", s) | (s, "") => s.to_string(),
            (s, e) => format!("{s}\n{e}"),
        };

        if matches!(exit_code, Some(2 | 64 | 65 | 77)) || on_failure == OnFailure::Abort {
            HookOutcome::Error {
                name: name.to_string(),
                exit_code,
                message: format!("Operation blocked:\n{combined}"),
            }
        } else {
            HookOutcome::Warning {
                name: name.to_string(),
                exit_code,
                message: combined,
            }
        }
    }

    /// Try to interpret stdout as a structured action response.
    fn parse_action_response(
        name: &str,
        exit_code: Option<i32>,
        stdout: &str,
        context: &HookContext,
    ) -> Option<Self> {
        if stdout.is_empty() {
            return None;
        }
        let json_val: serde_json::Value = serde_json::from_str(stdout).ok()?;
        let action: ActionOutput = serde_json::from_value(json_val.clone()).ok()?;

        if let Some(ref decision) = action.decision {
            match decision {
                ActionDecision::Block | ActionDecision::Deny => {
                    return Some(HookOutcome::Error {
                        name: name.to_string(),
                        exit_code,
                        message: format!(
                            "Operation blocked by decision:\n{}",
                            action.message.as_deref().unwrap_or(stdout)
                        ),
                    });
                }
                ActionDecision::Allow if action.updated_input.is_none() => {
                    return Some(HookOutcome::Success);
                }
                ActionDecision::Allow => {
                    return action.updated_input.map(|data| HookOutcome::Transformed {
                        name: name.to_string(),
                        data,
                    });
                }
                ActionDecision::Ask => {}
            }
        }

        if let Some(data) = action.updated_input {
            return Some(HookOutcome::Transformed {
                name: name.to_string(),
                data,
            });
        }

        if context.data.is_tool() || action.decision.is_none() {
            return Some(HookOutcome::Transformed {
                name: name.to_string(),
                data: json_val,
            });
        }

        None
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
