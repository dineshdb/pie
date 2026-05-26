use agentsdk::core::agent::PreToolAction;
use agentsdk::core::messages::{self, Message};
use agentsdk::core::plugin::{AgentPlugin, PluginContext};
use agentsdk::openai::api::types::ChatCompletionRequestUserMessageContent;
use async_trait::async_trait;
use serde_json::Value;
use std::str::FromStr;

const MODE_MARKER_PREFIX: &str = "[mode:";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentMode {
    #[default]
    Plan,
    Build,
    Debug,
    Test,
    Review,
    Architect,
}

impl AgentMode {
    pub fn all() -> &'static [Self] {
        &[
            Self::Plan,
            Self::Build,
            Self::Debug,
            Self::Test,
            Self::Review,
            Self::Architect,
        ]
    }

    pub fn short_name(&self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Build => "build",
            Self::Debug => "debug",
            Self::Test => "test",
            Self::Review => "review",
            Self::Architect => "architect",
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            Self::Plan => "Analysis mode — read files only, no modifications",
            Self::Build => "Full access — all tools available",
            Self::Debug => "Debug mode — read and execute commands, no file writes",
            Self::Test => "Test mode — focus on writing and running tests",
            Self::Review => "Review mode — read-only code review",
            Self::Architect => "Architect mode — high-level design, no implementation",
        }
    }

    pub fn tool_restrictions(&self) -> &'static str {
        match self {
            Self::Plan => "Blocked: Write, Edit, Bash",
            Self::Build => "No restrictions",
            Self::Debug => "Blocked: Write, Edit",
            Self::Test => "No restrictions",
            Self::Review => "Blocked: Write, Edit, Bash",
            Self::Architect => "Blocked: Write, Edit, Bash",
        }
    }

    pub fn marker(&self) -> String {
        format!("{}{}]", MODE_MARKER_PREFIX, self.short_name())
    }

    /// Persistent marker stored in session history.
    /// Compact on purpose — the system prompt defines what each marker means.
    pub fn system_marker(&self) -> String {
        format!("[mode:{}] {}", self.short_name(), self.description())
    }

    fn is_tool_blocked(&self, name: &str) -> bool {
        match self {
            Self::Plan | Self::Review | Self::Architect => {
                matches!(name, "Write" | "Edit" | "Bash")
            }
            Self::Debug => matches!(name, "Write" | "Edit"),
            Self::Build | Self::Test => false,
        }
    }
}

impl FromStr for AgentMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "plan" => Ok(Self::Plan),
            "build" => Ok(Self::Build),
            "debug" => Ok(Self::Debug),
            "test" => Ok(Self::Test),
            "review" => Ok(Self::Review),
            "architect" => Ok(Self::Architect),
            _ => Err(format!(
                "Unknown mode '{}'. Available: {}",
                s,
                Self::all()
                    .iter()
                    .map(|m| m.short_name())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }
}

impl std::fmt::Display for AgentMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.short_name())
    }
}

pub struct ModePlugin {
    mode: AgentMode,
    pending_switch: Option<AgentMode>,
}

impl Default for ModePlugin {
    fn default() -> Self {
        Self {
            mode: AgentMode::Build,
            pending_switch: None,
        }
    }
}

impl ModePlugin {
    pub fn new(mode: AgentMode) -> Self {
        Self {
            mode,
            pending_switch: None,
        }
    }

    /// Request a mode switch. Takes effect at the start of the next iteration.
    pub fn request_switch(&mut self, mode: AgentMode) {
        self.pending_switch = Some(mode);
    }

    pub fn current_mode(&self) -> AgentMode {
        self.mode
    }
}

/// Extract the last mode marker from conversation history.
fn detect_mode_from_history(history: &[Message]) -> Option<AgentMode> {
    for msg in history.iter().rev() {
        let content: Option<&str> = match msg {
            Message::SystemMessage(s) => s.content.as_deref(),
            Message::UserMessage(u) => match &u.content {
                Some(ChatCompletionRequestUserMessageContent::String(s)) => Some(s.as_str()),
                _ => None,
            },
            _ => None,
        };
        let content = content?;
        let lower = content.to_lowercase();
        let after_prefix = lower.find(MODE_MARKER_PREFIX)?;
        let rest = &lower[after_prefix + MODE_MARKER_PREFIX.len()..];
        let mode_name = rest.split(']').next()?;
        if let Ok(mode) = AgentMode::from_str(mode_name) {
            return Some(mode);
        }
    }
    None
}

#[async_trait]
impl AgentPlugin for ModePlugin {
    fn name(&self) -> &'static str {
        "modes"
    }

    async fn on_iteration_start(&mut self, ctx: &mut PluginContext, iteration: usize) {
        // Handle pending switch from a tool call
        if let Some(new_mode) = self.pending_switch.take() {
            self.mode = new_mode;
            if let Some(mut history) = ctx.get_mut::<agentsdk::core::history::History>() {
                history.0.push(messages::user(format!(
                    "[Mode: {}]\n\n{}",
                    new_mode,
                    new_mode.description()
                )));
            }
            return;
        }

        // On first iteration, detect mode from session history markers
        if iteration == 0 {
            if let Some(history) = ctx.get::<agentsdk::core::history::History>() {
                if let Some(mode) = detect_mode_from_history(&history.0) {
                    self.mode = mode;
                }
            }
        }
    }

    async fn on_tool_pre_execute(
        &mut self,
        _ctx: &mut PluginContext,
        _id: &str,
        name: &str,
        _args: &Value,
    ) -> PreToolAction {
        if self.mode.is_tool_blocked(name) {
            return PreToolAction::Abort(format!(
                "{} is not allowed in {} mode. Tool restrictions: {}",
                name,
                self.mode,
                self.mode.tool_restrictions()
            ));
        }
        PreToolAction::Proceed(None)
    }

    /// Provide tool which LLM can call to switch modes.
    fn tools(&self) -> Vec<agentsdk::core::tools::ToolDefinition> {
        use agentsdk::core::tools::ToolDefinition;
        vec![ToolDefinition {
            name: "switch_mode".into(),
            description: "Switch the agent's operating mode. Available modes: \
                          plan (read-only analysis), build (full access), \
                          debug (root cause, read+exec), test (testing focus), \
                          review (code review), architect (high-level design)."
                .into(),
            input_schema: agentsdk::__private::schemars::schema_for!(SwitchModeInput),
        }]
    }

    async fn run_tool(
        &mut self,
        _ctx: &mut PluginContext,
        call: &agentsdk::core::plugin::PluginToolCall,
    ) -> Result<Value, String> {
        let input: SwitchModeInput = serde_json::from_value(call.arguments.clone())
            .map_err(|e| format!("Invalid input: {e}"))?;
        let new_mode: AgentMode = input.mode.parse().map_err(|e: String| e)?;
        if new_mode == self.mode {
            return Ok(serde_json::json!({
                "status": "already_active",
                "mode": self.mode.short_name(),
                "message": format!("Already in {} mode", self.mode),
            }));
        }
        self.pending_switch = Some(new_mode);
        Ok(serde_json::json!({
            "status": "switching",
            "mode": new_mode.short_name(),
            "message": format!("Switching to {} mode on next iteration", new_mode),
        }))
    }
}

#[derive(serde::Deserialize, agentsdk::__private::schemars::JsonSchema)]
struct SwitchModeInput {
    /// Mode to switch to: plan, build, debug, test, review, architect
    mode: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mode_from_str() {
        assert_eq!("plan".parse::<AgentMode>().unwrap(), AgentMode::Plan);
        assert_eq!("build".parse::<AgentMode>().unwrap(), AgentMode::Build);
        assert_eq!("debug".parse::<AgentMode>().unwrap(), AgentMode::Debug);
        assert_eq!("test".parse::<AgentMode>().unwrap(), AgentMode::Test);
        assert_eq!("review".parse::<AgentMode>().unwrap(), AgentMode::Review);
        assert_eq!(
            "architect".parse::<AgentMode>().unwrap(),
            AgentMode::Architect
        );
        assert!("unknown".parse::<AgentMode>().is_err());
    }

    #[test]
    fn test_mode_marker() {
        let sys = messages::system(&AgentMode::Plan.system_marker());
        let detected = detect_mode_from_history(&[sys]);
        assert_eq!(detected, Some(AgentMode::Plan));
    }

    #[test]
    fn test_tool_blocking() {
        assert!(AgentMode::Plan.is_tool_blocked("Write"));
        assert!(AgentMode::Plan.is_tool_blocked("Edit"));
        assert!(AgentMode::Plan.is_tool_blocked("Bash"));
        assert!(!AgentMode::Plan.is_tool_blocked("Read"));
        assert!(!AgentMode::Plan.is_tool_blocked("Glob"));

        assert!(!AgentMode::Build.is_tool_blocked("Write"));
        assert!(!AgentMode::Build.is_tool_blocked("Bash"));

        assert!(AgentMode::Debug.is_tool_blocked("Write"));
        assert!(!AgentMode::Debug.is_tool_blocked("Bash"));
    }
}
