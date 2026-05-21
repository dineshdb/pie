pub mod plan;
mod shell;
mod websearch;

pub use shell::shell;
pub use websearch::websearch;

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
    let redacted = jewels::redact(&anonymized);
    tracing::debug!("TOOL: {name} {redacted}");
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
    let mut bin_dirs = vec![crate::config::pie_home().join("bin")];
    if let Some(git_root) = crate::utils::git_repo_root() {
        bin_dirs.push(std::path::PathBuf::from(git_root).join(".pie").join("bin"));
    }

    let result = cfg.build_safe_command(cmd, &bin_dirs);
    match result {
        Ok(mut c) => match c.output() {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                SandboxOutput {
                    stdout: jewels::redact(&crate::utils::anonymize_path(&stdout)),
                    stderr: jewels::redact(&crate::utils::anonymize_path(&stderr)),
                    exit_code: out.status.code().unwrap_or(-1),
                }
            }
            Err(e) => SandboxOutput {
                stdout: String::new(),
                stderr: e.to_string(),
                exit_code: -1,
            },
        },
        Err(e) => SandboxOutput {
            stdout: String::new(),
            stderr: e,
            exit_code: -1,
        },
    }
}
