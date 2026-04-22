use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, OnceLock};

/// Sandbox configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxConfig {
    #[serde(default)]
    pub network: NetworkConfig,
    #[serde(default)]
    pub filesystem: FilesystemConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkConfig {
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    #[serde(default)]
    pub denied_domains: Vec<String>,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            allowed_domains: vec![
                "github.com".into(),
                "*.github.com".into(),
                "api.github.com".into(),
                "lfs.github.com".into(),
                "npmjs.org".into(),
                "*.npmjs.org".into(),
                "crates.io".into(),
                "*.crates.io".into(),
                "pypi.org".into(),
                "files.pythonhosted.org".into(),
            ],
            denied_domains: vec![],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilesystemConfig {
    #[serde(default)]
    pub deny_read: Vec<String>,
    #[serde(default)]
    pub allow_write: Vec<String>,
    #[serde(default)]
    pub deny_write: Vec<String>,
}

impl Default for FilesystemConfig {
    fn default() -> Self {
        Self {
            deny_read: vec!["~/.ssh".into(), "~/.gnupg".into()],
            allow_write: vec![".".into(), "/tmp".into()],
            deny_write: vec![".env".into(), ".env.local".into()],
        }
    }
}

/// Load sandbox config from `<home_dir>/sandbox.json`, falling back to defaults.
pub fn load(home_dir: &Path) -> Arc<SandboxConfig> {
    let path = home_dir.join("sandbox.json");
    let cfg = std::fs::read_to_string(&path)
        .ok()
        .and_then(|content| {
            serde_json::from_str(&content)
                .inspect_err(|e| {
                    tracing::warn!("Invalid sandbox config at {}: {e}", path.display());
                })
                .ok()
        })
        .unwrap_or_default();
    Arc::new(cfg)
}

/// Build a sandboxed command using native OS sandboxing.
/// Falls back to unsandboxed `sh -c <cmd>` if the sandbox tool is unavailable.
pub fn build_command(cmd: &str, cfg: &SandboxConfig) -> Command {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    let available = *AVAILABLE.get_or_init(is_sandbox_tool_available);

    if available {
        build_sandboxed(cmd, cfg)
    } else {
        tracing::warn!("sandbox tool not found, running unsandboxed");
        let mut c = Command::new("sh");
        c.arg("-c").arg(cmd);
        c
    }
}

/// Expand `~` to home directory.
fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        dirs::home_dir()
            .map(|h| h.join(rest).to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string())
    } else if path == "~" {
        dirs::home_dir()
            .map(|h| h.display().to_string())
            .unwrap_or_else(|| path.to_string())
    } else {
        path.to_string()
    }
}

/// Resolve a path to absolute (expand ~, prepend cwd if relative).
fn resolve_path(path: &str) -> String {
    let expanded = expand_tilde(path);
    if Path::new(&expanded).is_absolute() {
        expanded
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&expanded).to_string_lossy().to_string())
            .unwrap_or(expanded)
    }
}

// --- Platform-specific ---

#[cfg(target_os = "macos")]
pub(crate) mod platform {
    use super::*;

    const BINARY: &str = "sandbox-exec";

    pub(crate) fn is_available() -> bool {
        Command::new("which")
            .arg(BINARY)
            .output()
            .is_ok_and(|o| o.status.success())
    }

    pub(crate) fn build(cmd: &str, cfg: &SandboxConfig) -> Command {
        let profile = generate_profile(cfg);
        tracing::debug!(%cmd, "sandbox-exec:");
        let mut c = Command::new(BINARY);
        c.arg("-p").arg(&profile).arg("sh").arg("-c").arg(cmd);
        c
    }

    pub(crate) fn generate_profile(cfg: &SandboxConfig) -> String {
        let mut lines = Vec::new();
        lines.push("(version 1)".to_string());
        lines.push("(allow default)".to_string());

        // Network: SBPL cannot filter by domain — only allow/deny all outbound.
        // If allowed_domains is set, allow outbound (needed for DNS + HTTPS).
        // If denied_domains only, deny outbound entirely.
        if cfg.network.allowed_domains.is_empty() && !cfg.network.denied_domains.is_empty() {
            lines.push("(deny network-outbound)".to_string());
        }

        // Deny read paths
        for path in &cfg.filesystem.deny_read {
            let resolved = resolve_path(path);
            lines.push(format!("(deny file-read* (subpath \"{resolved}\"))"));
        }

        // Write: deny all, then allow listed paths + system dirs
        if !cfg.filesystem.allow_write.is_empty() {
            lines.push("(deny file-write*)".to_string());
            for path in &cfg.filesystem.allow_write {
                let resolved = resolve_path(path);
                lines.push(format!("(allow file-write* (subpath \"{resolved}\"))"));
            }
            // System temp dirs (needed for mktemp, pipes, subprocess tmp files)
            lines.push("(allow file-write* (subpath \"/tmp\"))".to_string());
            lines.push("(allow file-write* (subpath \"/private/tmp\"))".to_string());
            lines.push("(allow file-write* (subpath \"/private/var/folders\"))".to_string());
            // Tool-specific dirs (semgrep logs/settings, git creds, etc.)
            if let Some(home) = dirs::home_dir() {
                let h = home.to_string_lossy();
                lines.push(format!(
                    "(allow file-write* (subpath \"{h}/.config/.semgrep\"))"
                ));
            }
        }

        // Deny write patterns (e.g. .env files)
        for pattern in &cfg.filesystem.deny_write {
            let escaped = pattern.replace('.', "\\.");
            lines.push(format!("(deny file-write* (regex #\"^.*{escaped}$\"))"));
        }

        lines.join("\n")
    }
}

#[cfg(target_os = "linux")]
pub(crate) mod platform {
    use super::*;

    const BINARY: &str = "bwrap";

    pub(crate) fn is_available() -> bool {
        Command::new("which")
            .arg(BINARY)
            .output()
            .is_ok_and(|o| o.status.success())
    }

    pub(crate) fn build(cmd: &str, cfg: &SandboxConfig) -> Command {
        let mut c = Command::new(BINARY);

        // Read-only root filesystem
        c.arg("--ro-bind").arg("/").arg("/");
        c.arg("--dev").arg("/dev");
        c.arg("--proc").arg("/proc");
        c.arg("--die-with-parent");

        // Deny read paths: mount empty tmpfs over them
        for path in &cfg.filesystem.deny_read {
            let resolved = resolve_path(path);
            c.arg("--tmpfs").arg(&resolved);
        }

        // Allow write paths: bind-mount read-write
        for path in &cfg.filesystem.allow_write {
            let resolved = resolve_path(path);
            c.arg("--bind").arg(&resolved).arg(&resolved);
        }

        // Network: bwrap cannot do per-domain filtering.
        // If there are allowed domains, we need DNS → can't unshare-net.
        // If only denied domains (no allowed), also can't selectively block.
        // Only unshare net when there are no allowed domains and no denied domains
        // (i.e. default config with allowed domains = no unshare).
        if !cfg.network.allowed_domains.is_empty() {
            tracing::debug!(
                "bwrap cannot filter network per-domain; network restrictions not enforced"
            );
        }

        c.arg("sh").arg("-c").arg(cmd);
        tracing::debug!(%cmd, "bwrap:");
        c
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!("pie only supports macOS and Linux");

#[cfg(target_os = "macos")]
use platform as plat;
#[cfg(target_os = "linux")]
use platform as plat;

fn is_sandbox_tool_available() -> bool {
    plat::is_available()
}

fn build_sandboxed(cmd: &str, cfg: &SandboxConfig) -> Command {
    plat::build(cmd, cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_sensible_defaults() {
        let cfg = SandboxConfig::default();
        assert!(!cfg.network.allowed_domains.is_empty());
        assert!(cfg.network.denied_domains.is_empty());
        assert!(!cfg.filesystem.deny_read.is_empty());
        assert!(!cfg.filesystem.allow_write.is_empty());
    }

    #[test]
    fn resolve_path_expands_tilde() {
        let resolved = resolve_path("~/test");
        assert!(!resolved.starts_with('~'));
        assert!(resolved.ends_with("/test"));
    }

    #[test]
    fn resolve_path_makes_relative_absolute() {
        let resolved = resolve_path(".");
        assert!(Path::new(&resolved).is_absolute());
    }

    #[test]
    fn resolve_path_leaves_absolute_unchanged() {
        let resolved = resolve_path("/tmp");
        assert_eq!(resolved, "/tmp");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_profile_has_required_sections() {
        let cfg = SandboxConfig::default();
        let profile = generate_macos_profile(&cfg);
        assert!(profile.contains("(version 1)"));
        assert!(profile.contains("(allow default)"));
        // default config has allowed_domains, so network is NOT denied
        assert!(!profile.contains("(deny network"));
        assert!(profile.contains("(deny file-read*"));
        assert!(profile.contains("(deny file-write*)"));
        assert!(profile.contains("(allow file-write*"));
        assert!(profile.contains("/private/var/folders"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_profile_denies_network_when_no_allowed_domains() {
        let mut cfg = SandboxConfig::default();
        cfg.network.allowed_domains.clear();
        cfg.network.denied_domains.push("evil.com".into());
        let profile = generate_macos_profile(&cfg);
        assert!(profile.contains("(deny network-outbound)"));
    }

    #[cfg(target_os = "macos")]
    fn generate_macos_profile(cfg: &SandboxConfig) -> String {
        platform::generate_profile(cfg)
    }
}
