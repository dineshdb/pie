pub mod plan;

// ── Sandbox execution helpers ──────────────────────────────────────────

fn default_bin_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = vec![crate::config::pie_home().join("bin")];
    if let Some(git_root) = crate::utils::git_repo_root() {
        dirs.push(std::path::PathBuf::from(git_root).join(".pie").join("bin"));
    }
    dirs
}

pub(crate) fn run_sandboxed_command_streaming(
    cmd: &str,
    cfg: &p1e_sandbox::SandboxConfig,
    extra_bin_dirs: &[std::path::PathBuf],
) -> Result<i32, String> {
    let mut bin_dirs = default_bin_dirs();
    bin_dirs.extend_from_slice(extra_bin_dirs);

    let mut command = cfg.build_safe_command(cmd, &bin_dirs)?;
    command.stdin(std::process::Stdio::inherit());
    command.stdout(std::process::Stdio::inherit());
    command.stderr(std::process::Stdio::inherit());

    let mut child = command.spawn().map_err(|e| e.to_string())?;
    let status = child.wait().map_err(|e| e.to_string())?;
    Ok(status.code().unwrap_or(-1))
}
