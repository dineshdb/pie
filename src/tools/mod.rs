pub use filesystem::{read_file_tool, replace_tool, write_file_tool};
pub use shell::shell;
pub use skills::{execute_skill_script_tool, load_references_tool, load_skills_tool};
pub use subagent::subagent_tool;

mod filesystem;
mod shell;
mod skills;
mod subagent;

/// Lock a mutex, recovering from poison instead of panicking.
pub(crate) fn safe_lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
