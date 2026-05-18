mod filesystem;
mod hooks;
pub mod plan;
mod shell;
mod skills;
pub(crate) mod subagent;
mod websearch;

pub use filesystem::{
    glob_tool, list_directory_tool, read_file_tool, replace_tool, write_file_tool,
};
pub use hooks::wrap_tools_with_hooks;
pub use shell::shell;
pub use skills::{execute_skill_script_tool, load_references_tool, load_skills_tool};
pub use subagent::subagent_tool;
pub use websearch::websearch;

/// Typed tool names for stringly-typed comparisons across the codebase.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    strum::AsRefStr,
    strum::Display,
    strum::EnumString,
    strum::EnumIter,
)]
pub enum ToolName {
    #[strum(serialize = "shell")]
    Shell,
    #[strum(serialize = "read_file")]
    ReadFile,
    #[strum(serialize = "write_file")]
    WriteFile,
    #[strum(serialize = "replace")]
    Replace,
    #[strum(serialize = "list_directory")]
    ListDirectory,
    #[strum(serialize = "glob")]
    Glob,
    #[strum(serialize = "load_skills")]
    LoadSkills,
    #[strum(serialize = "load_references")]
    LoadReferences,
    #[strum(serialize = "execute_skill_script")]
    ExecuteSkillScript,
    #[strum(serialize = "websearch")]
    Websearch,
    #[strum(serialize = "subagent")]
    Subagent,
    #[strum(serialize = "plan_set")]
    PlanSet,
    #[strum(serialize = "plan_step_update")]
    PlanStepUpdate,
    #[strum(serialize = "plan_show")]
    PlanShow,
}

/// Lock a mutex, recovering from poison instead of panicking.
pub(crate) fn safe_lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Emit a `TOOL:` line with tool name and input parameters for test observability.
pub(crate) fn emit_tool_input(name: &str, params: &serde_json::Value) {
    let params_str = params.to_string();
    let anonymized = crate::utils::anonymize_path(&params_str);
    tracing::debug!("TOOL: {name} {anonymized}");
}

// ── Sandbox execution helpers ──────────────────────────────────────────

/// Captured output from a sandboxed command execution.
pub(crate) struct SandboxOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Execute a command inside the sandbox and capture its output.
pub(crate) fn run_sandboxed_command(cmd: &str, cfg: &p1e_sandbox::SandboxConfig) -> SandboxOutput {
    let result = p1e_sandbox::build_shell_command(cmd, cfg)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("PAGER", "cat")
        .output();
    match result {
        Ok(out) => SandboxOutput {
            stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            exit_code: out.status.code().unwrap_or(-1),
        },
        Err(e) => SandboxOutput {
            stdout: String::new(),
            stderr: e.to_string(),
            exit_code: -1,
        },
    }
}
