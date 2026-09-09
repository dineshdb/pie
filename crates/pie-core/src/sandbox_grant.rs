//! Sandbox assembly shared by the frontends that run sessions in
//! client-chosen workspaces (ACP, the MCP tasks server).

use p1e_sandbox::SandboxConfig;
use std::path::PathBuf;

/// Canonicalize the session root and grant it read+write in a per-session
/// sandbox copy. This is where remote clients get their workspace access:
/// the CLI's `allow_write = ["."]` only ever covers the directory pie was
/// *started* in, while a remote client points pie at any project. Cloning
/// is idempotent — re-granting the same roots never duplicates entries.
///
/// `deny_read`/`deny_write` from the base config survive the grant, so
/// `~/.ssh` and `.env` stay off limits even under a broad root.
pub fn granted_sandbox(base: &SandboxConfig, roots: &[PathBuf]) -> SandboxConfig {
    let mut sandbox = base.clone();
    for root in roots {
        let as_str = root.to_string_lossy().to_string();
        for list in [&mut sandbox.allow_read, &mut sandbox.allow_write] {
            if !list.contains(&as_str) {
                list.push(as_str.clone());
            }
        }
    }
    sandbox
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn granted_sandbox_adds_roots_to_read_and_write_once() {
        let base = SandboxConfig {
            allow_write: vec![".".into(), "/tmp".into()],
            allow_read: vec!["/".into()],
            ..SandboxConfig::default()
        };
        let roots = vec![PathBuf::from("/Users/me/proj")];

        let granted = granted_sandbox(&base, &roots);
        assert!(granted.allow_write.contains(&"/Users/me/proj".to_string()));
        assert!(granted.allow_read.contains(&"/Users/me/proj".to_string()));
        assert!(granted.allow_write.contains(&".".to_string()));

        // Re-granting the same root must not duplicate entries (the config
        // validator warns on duplicates).
        let again = granted_sandbox(&granted, &roots);
        assert_eq!(
            again
                .allow_write
                .iter()
                .filter(|p| **p == "/Users/me/proj")
                .count(),
            1
        );

        // deny_read survives the grant: ~/.ssh stays protected.
        assert!(granted.deny_read.contains(&"~/.ssh".to_string()));
    }
}
