pub use filesystem::{
    glob_tool, list_directory_tool, read_file_tool, replace_tool, write_file_tool,
};
pub use hooks::wrap_tools_with_hooks;
pub use shell::shell;
pub use skills::{execute_skill_script_tool, load_references_tool, load_skills_tool};
pub use subagent::subagent_tool;
pub use websearch::websearch;

mod filesystem;
mod hooks;
pub mod plan;
mod shell;
mod skills;
pub(crate) mod subagent;
mod websearch;

/// Lock a mutex, recovering from poison instead of panicking.
pub(crate) fn safe_lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Emit a `TOOL:` line with tool name and input parameters for test observability.
pub(crate) fn emit_tool_input(name: &str, params: &serde_json::Value) {
    tracing::debug!("TOOL: {name} {params}");
}
