use agentsdk::core::agent::PreToolAction;
use agentsdk::core::cwd::Cwd;
use agentsdk::core::messages::Message;
use agentsdk::core::plugin::{AgentPlugin, PluginContext};
use agentsdk::openai::api::types::ChatCompletionRequestUserMessageContent;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use strum::{Display, EnumString};

const MODE_MARKER_PREFIX: &str = "[mode:";

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    Display,
    EnumString,
    serde::Serialize,
    serde::Deserialize,
)]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
#[serde(rename_all = "snake_case")]
pub enum AgentMode {
    Plan,
    #[default]
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

    pub fn short_name(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Build => "build",
            Self::Debug => "debug",
            Self::Test => "test",
            Self::Review => "review",
            Self::Architect => "architect",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Plan => Self::Build,
            Self::Build => Self::Debug,
            Self::Debug => Self::Test,
            Self::Test => Self::Review,
            Self::Review => Self::Architect,
            Self::Architect => Self::Plan,
        }
    }

    /// Persistent marker stored in session history.
    pub fn system_marker(self) -> String {
        format!("[mode:{}]", self.short_name())
    }

    fn file_name(self) -> String {
        format!("{}.md", self.short_name())
    }

    /// Whether this mode refuses `name`. Callers that fix the mode for a
    /// whole run use it to leave blocked tools out of the advertised list.
    pub fn is_tool_blocked(self, name: &str) -> bool {
        match self {
            // Plan only observes. An allowlist, not a blocklist: a tool that
            // pie does not know about — a new plugin, anything an MCP server
            // offers — could write, so it waits until the user leaves plan.
            Self::Plan => !READ_ONLY_TOOLS.contains(&name),
            Self::Debug => matches!(name, "Write" | "Edit"),
            Self::Review | Self::Architect => matches!(name, "Write" | "Edit" | "Bash"),
            Self::Build | Self::Test => false,
        }
    }
}

/// Tools that only look: the ones plan mode allows. `switch_mode` is on the
/// list because a mode you cannot leave is a trap, not a mode — everything
/// else here reads files, searches, or loads instructions.
///
/// Deliberately absent: `Bash` (a shell can write), `RunSkillScript` (runs a
/// script), `Write`/`Edit`, and every `<server>__<tool>` an MCP server
/// contributes — pie cannot tell a docs lookup from a deployment.
const READ_ONLY_TOOLS: [&str; 9] = [
    "Read",
    "Ls",
    "Glob",
    "WebSearch",
    "FindSkills",
    "LoadSkills",
    "LoadSkillReference",
    "command__load",
    "switch_mode",
];

#[derive(Debug, Deserialize)]
struct ModeFrontmatter {
    description: String,
    #[serde(default = "default_tool_restrictions")]
    tool_restrictions: String,
}

fn default_tool_restrictions() -> String {
    "None".to_string()
}

#[derive(Debug)]
pub struct ModeFile {
    pub description: String,
    pub tool_restrictions: String,
    pub body: String,
}

/// Resolve a mode file path: local `.pie/modes/` first, fall back to global.
/// Both lookups key off the run's cwd, never the process cwd.
fn resolve_mode_path(name: &str, cwd: &Path) -> Option<PathBuf> {
    // local repo
    if let Some(root) = crate::utils::git_repo_root_from(cwd) {
        let local = PathBuf::from(root).join(".pie").join("modes").join(name);
        if local.is_file() {
            return Some(local);
        }
    }
    // global
    let global = crate::config::pie_home().join("modes").join(name);
    if global.is_file() {
        return Some(global);
    }
    None
}

pub fn load_mode_file(mode: AgentMode) -> Option<ModeFile> {
    load_mode_file_in(mode, &Cwd::fallback().0)
}

pub fn load_mode_file_in(mode: AgentMode, cwd: &Path) -> Option<ModeFile> {
    let path = resolve_mode_path(&mode.file_name(), cwd)?;
    let raw = std::fs::read_to_string(path).ok()?;
    let (yaml, body) = split_frontmatter(&raw);
    let fm: ModeFrontmatter = serde_yaml::from_str(&yaml).ok()?;
    let body = body.trim().to_string();
    Some(ModeFile {
        description: fm.description,
        tool_restrictions: fm.tool_restrictions,
        body,
    })
}

fn split_frontmatter(raw: &str) -> (String, String) {
    let raw = raw.trim();
    if let Some(rest) = raw.strip_prefix("---")
        && let Some(end) = rest.find("\n---")
    {
        let yaml = rest[..end].trim().to_string();
        let body = rest[end + 4..].trim().to_string();
        return (yaml, body);
    }
    (String::new(), raw.to_string())
}

// ── ModePlugin ───────────────────────────────────────────────────

pub struct ModePlugin {
    pub mode: AgentMode,
    /// Whether the agent may retune its own mode. Frontends that own a mode
    /// selector (ACP) turn this off: the agent switching itself would desync
    /// their UI, and a read-only mode the agent can leave is not read-only.
    switching: bool,
}

impl Default for ModePlugin {
    fn default() -> Self {
        Self {
            mode: AgentMode::Build,
            switching: true,
        }
    }
}

impl ModePlugin {
    pub fn new(mode: AgentMode) -> Self {
        Self {
            mode,
            switching: true,
        }
    }

    /// Drop the `switch_mode` tool: the mode is the caller's to set, for the
    /// whole run. History markers stop moving it too, so the mode the caller
    /// passed is the mode that runs.
    pub fn without_switching(mut self) -> Self {
        self.switching = false;
        self
    }

    #[allow(dead_code)]
    pub fn current_mode(&self) -> AgentMode {
        self.mode
    }
}

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
        if !self.switching {
            return; // the caller's choice outranks whatever the history says
        }
        if iteration == 0
            && let Some(history) = ctx.get::<agentsdk::core::history::History>()
            && let Some(mode) = detect_mode_from_history(&history.0)
        {
            self.mode = mode;
        }
    }

    async fn prepare_system_prompt(
        &mut self,
        ctx: &mut PluginContext,
    ) -> Option<Cow<'static, str>> {
        let mode_file = load_mode_file_in(self.mode, &Cwd::from_ctx(ctx).0)?;
        let fixed = if self.switching {
            ""
        } else {
            "\n\nThe user sets this mode in their editor and only they can change \
             it — you have no switch_mode tool, and the tools this mode forbids \
             are not available to you at all. If the work needs one, say which \
             and why, and let them decide."
        };
        Some(Cow::Owned(format!(
            "# Mode: {}\n\n{}\n\n## Tool Restrictions\n{}{}",
            self.mode.short_name(),
            mode_file.body,
            mode_file.tool_restrictions,
            fixed,
        )))
    }

    async fn on_tool_pre_execute(
        &mut self,
        ctx: &mut PluginContext,
        _id: &str,
        name: &str,
        _args: &Value,
    ) -> PreToolAction {
        if self.mode.is_tool_blocked(name) {
            let restrictions = load_mode_file_in(self.mode, &Cwd::from_ctx(ctx).0)
                .map(|f| f.tool_restrictions)
                .unwrap_or_default();
            return PreToolAction::Abort(format!(
                "{} is not allowed in {} mode. Tool restrictions: {}",
                name, self.mode, restrictions
            ));
        }
        PreToolAction::Proceed(None)
    }

    fn tools(&self) -> Vec<agentsdk::core::tools::ToolDefinition> {
        use agentsdk::core::tools::ToolDefinition;
        if !self.switching {
            return Vec::new();
        }
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
        if !self.switching {
            // The tool is not advertised, but a replayed history can still
            // carry a call for it.
            return Err(format!(
                "The mode is set by the user's editor and is currently {}. \
                 Ask them to switch it; you cannot.",
                self.mode.short_name()
            ));
        }
        let input: SwitchModeInput = serde_json::from_value(call.arguments.clone())
            .map_err(|e| format!("Invalid input: {e}"))?;
        let new_mode: AgentMode = input
            .mode
            .parse()
            .map_err(|e: strum::ParseError| e.to_string())?;
        if new_mode == self.mode {
            return Ok(Value::Null);
        }
        self.mode = new_mode;
        Ok(serde_json::json!({
            "status": "switched",
            "mode": self.mode.short_name(),
            "message": format!("Switched to {} mode", self.mode),
        }))
    }
}

#[derive(Deserialize, agentsdk::__private::schemars::JsonSchema)]
struct SwitchModeInput {
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
        assert_eq!("PLAN".parse::<AgentMode>().unwrap(), AgentMode::Plan);
        assert_eq!("Build".parse::<AgentMode>().unwrap(), AgentMode::Build);
    }

    #[test]
    fn test_mode_marker() {
        let sys = agentsdk::core::messages::system(AgentMode::Plan.system_marker());
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

        // Plan denies by default: a script runner and anything an MCP server
        // contributes stay out, but the way back to another mode is open.
        assert!(AgentMode::Plan.is_tool_blocked("RunSkillScript"));
        assert!(AgentMode::Plan.is_tool_blocked("context7__query-docs"));
        assert!(AgentMode::Plan.is_tool_blocked("SomeToolAddedNextYear"));
        assert!(!AgentMode::Plan.is_tool_blocked("switch_mode"));
        assert!(!AgentMode::Plan.is_tool_blocked("LoadSkills"));

        assert!(!AgentMode::Build.is_tool_blocked("Write"));
        assert!(!AgentMode::Build.is_tool_blocked("Bash"));

        assert!(AgentMode::Debug.is_tool_blocked("Write"));
        assert!(!AgentMode::Debug.is_tool_blocked("Bash"));
    }

    #[test]
    fn a_fixed_mode_offers_no_way_for_the_agent_to_change_it() {
        use agentsdk::core::plugin::AgentPlugin;

        let free = ModePlugin::new(AgentMode::Plan);
        assert_eq!(
            free.tools().len(),
            1,
            "without a client owning the mode, switch_mode stays available"
        );

        let fixed = ModePlugin::new(AgentMode::Plan).without_switching();
        assert!(
            fixed.tools().is_empty(),
            "a mode the agent cannot change must not advertise switch_mode"
        );
    }

    #[tokio::test]
    async fn a_fixed_mode_refuses_a_replayed_switch_call() {
        use agentsdk::core::plugin::{AgentPlugin, PluginToolCall};

        let mut world = agentsdk::hecs::World::new();
        let entity = world.spawn(());
        let mut ctx = PluginContext::new(world, entity);
        let mut fixed = ModePlugin::new(AgentMode::Plan).without_switching();

        let call = PluginToolCall {
            id: "1".into(),
            name: "switch_mode".into(),
            arguments: serde_json::json!({"mode": "build"}),
        };
        let err = fixed
            .run_tool(&mut ctx, &call)
            .await
            .expect_err("the model cannot switch a mode the caller fixed");
        assert!(err.contains("plan"), "{err}");
        assert_eq!(fixed.mode, AgentMode::Plan, "the mode must not move");
    }

    #[test]
    fn test_mode_next_cycle() {
        assert_eq!(AgentMode::Plan.next(), AgentMode::Build);
        assert_eq!(AgentMode::Architect.next(), AgentMode::Plan);
    }

    #[test]
    fn test_split_frontmatter() {
        let raw = "---\ndescription: test\ntool_restrictions: none\n---\nbody text";
        let (yaml, body) = split_frontmatter(raw);
        assert!(yaml.contains("description: test"));
        assert_eq!(body, "body text");
    }

    #[test]
    fn test_split_frontmatter_no_frontmatter() {
        let (yaml, body) = split_frontmatter("just body text");
        assert!(yaml.is_empty());
        assert_eq!(body, "just body text");
    }
}
