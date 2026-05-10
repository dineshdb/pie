use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

/// Sandbox configuration. Flat structure, deserialized from pie.toml `[sandbox]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxConfig {
    pub deny_read: Vec<String>,
    pub allow_read: Vec<String>,
    pub allow_write: Vec<String>,
    pub deny_write: Vec<String>,
    pub allowed_domains: Vec<String>,
    pub denied_domains: Vec<String>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            deny_read: vec!["~/.ssh".into(), "~/.gnupg".into()],
            allow_read: vec![
                "/".into(),
                "/dev".into(),
                "/proc".into(),
                "/etc/resolv.conf".into(),
                "/etc/hosts".into(),
            ],
            allow_write: vec![
                ".".into(),
                "/tmp".into(),
                "/dev/null".into(),
                "/dev/zero".into(),
                "/dev/random".into(),
                "/dev/urandom".into(),
                "/dev/tty".into(),
                "/private/tmp".into(),
                "/private/var/folders".into(),
            ],
            deny_write: vec![".env".into(), ".env.local".into()],
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

impl SandboxConfig {
    /// Validate for duplicates and conflicts.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut warnings = Vec::new();
        find_duplicates(&self.deny_read, "deny_read", &mut warnings);
        find_duplicates(&self.allow_read, "allow_read", &mut warnings);
        find_duplicates(&self.allow_write, "allow_write", &mut warnings);
        find_duplicates(&self.deny_write, "deny_write", &mut warnings);
        find_duplicates(&self.allowed_domains, "allowed_domains", &mut warnings);
        find_duplicates(&self.denied_domains, "denied_domains", &mut warnings);

        for path in &self.deny_read {
            if self.allow_read.contains(path) {
                warnings.push(format!(
                    "Path '{path}' appears in both deny_read and allow_read"
                ));
            }
        }
        for domain in &self.allowed_domains {
            if self.denied_domains.contains(domain) {
                warnings.push(format!(
                    "Domain '{domain}' appears in both allowed_domains and denied_domains"
                ));
            }
        }

        if warnings.is_empty() {
            Ok(())
        } else {
            Err(warnings)
        }
    }
}

fn find_duplicates(list: &[String], name: &str, warnings: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    for item in list {
        if !seen.insert(item) {
            warnings.push(format!("Duplicate entry '{item}' in {name}"));
        }
    }
}

/// Build a sandboxed command using native OS sandboxing.
/// Falls back to unsandboxed command if the sandbox tool is unavailable.
pub fn build_command(program: &str, args: &[String], cfg: &SandboxConfig) -> Command {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    let available = *AVAILABLE.get_or_init(is_sandbox_tool_available);

    if available {
        build_sandboxed(program, args, cfg)
    } else {
        tracing::warn!("sandbox tool not found, running unsandboxed");
        let mut c = Command::new(program);
        c.args(args);
        c
    }
}

/// Build a sandboxed shell command (via `bash -c`).
pub fn build_shell_command(cmd: &str, cfg: &SandboxConfig) -> Command {
    build_command("bash", &["-c".into(), cmd.into()], cfg)
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

    pub(crate) fn build(program: &str, args: &[String], cfg: &SandboxConfig) -> Command {
        let profile = generate_profile(cfg);
        tracing::trace!(%program, ?args, BINARY);
        let mut c = Command::new(BINARY);
        c.arg("-p").arg(&profile).arg(program).args(args);
        c
    }

    pub(crate) fn generate_profile(cfg: &SandboxConfig) -> String {
        let mut lines = vec![
            "(version 1)".to_string(),
            "(allow default)".to_string(),
            "(allow file-read-metadata)".to_string(),
            "(allow mach-lookup)".to_string(),
            "(allow sysctl-read)".to_string(),
            "(allow ipc-posix-shm)".to_string(),
        ];

        // Network: SBPL cannot filter by domain — only allow/deny all outbound.
        if cfg.allowed_domains.is_empty() && !cfg.denied_domains.is_empty() {
            lines.push("(deny network-outbound)".to_string());
        }

        for path in &cfg.deny_read {
            let resolved = resolve_path(path);
            lines.push(format!("(deny file-read* (subpath \"{resolved}\"))"));
        }

        for path in &cfg.allow_read {
            let resolved = resolve_path(path);
            lines.push(format!("(allow file-read* (subpath \"{resolved}\"))"));
        }

        if !cfg.allow_write.is_empty() {
            lines.push("(deny file-write*)".to_string());

            // Always allow essential interactive/system write paths if we are restricting writes
            let essentials = [
                "/dev/null",
                "/dev/zero",
                "/dev/random",
                "/dev/urandom",
                "/dev/tty",
                "/tmp",
                "/private/tmp",
                "/private/var/folders",
            ];

            for path in essentials {
                lines.push(format!("(allow file-write* (subpath \"{path}\"))"));
            }

            for path in &cfg.allow_write {
                let resolved = resolve_path(path);
                lines.push(format!("(allow file-write* (subpath \"{resolved}\"))"));
            }
        }

        for pattern in &cfg.deny_write {
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

    pub(crate) fn build(program: &str, args: &[String], cfg: &SandboxConfig) -> Command {
        let mut c = Command::new(BINARY);
        c.arg("--die-with-parent");

        for path in &cfg.allow_read {
            let resolved = resolve_path(path);
            if resolved == "/dev" {
                c.arg("--dev").arg("/dev");
            } else if resolved == "/proc" {
                c.arg("--proc").arg("/proc");
            } else {
                c.arg("--ro-bind").arg(&resolved).arg(&resolved);
            }
        }

        for path in &cfg.deny_read {
            let resolved = resolve_path(path);
            c.arg("--tmpfs").arg(&resolved);
        }

        for path in &cfg.allow_write {
            let resolved = resolve_path(path);
            c.arg("--bind").arg(&resolved).arg(&resolved);
        }

        // Always allow essential TTY/tmp for interactive use on Linux if restricting
        if !cfg.allow_write.is_empty() {
            // bwrap --dev /dev handles most of this, but we ensure /tmp is bound if not already
            if !cfg
                .allow_write
                .iter()
                .any(|p| p == "/tmp" || p == "/private/tmp")
            {
                c.arg("--bind").arg("/tmp").arg("/tmp");
            }
        }

        if !cfg.allowed_domains.is_empty() {
            tracing::debug!(
                "bwrap cannot filter network per-domain; network restrictions not enforced"
            );
        }

        c.arg(program).args(args);
        tracing::debug!(%program, ?args, "bwrap:");
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

fn build_sandboxed(program: &str, args: &[String], cfg: &SandboxConfig) -> Command {
    plat::build(program, args, cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_sensible_defaults() {
        let cfg = SandboxConfig::default();
        assert!(!cfg.allowed_domains.is_empty());
        assert!(cfg.denied_domains.is_empty());
        assert!(!cfg.deny_read.is_empty());
        assert!(!cfg.allow_write.is_empty());
        assert!(!cfg.deny_write.is_empty());
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

    #[test]
    fn validate_detects_duplicates() {
        let mut cfg = SandboxConfig::default();
        cfg.deny_read.push("~/.ssh".into()); // duplicate
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_detects_conflicts() {
        let mut cfg = SandboxConfig::default();
        cfg.allow_read.push("~/.ssh".into()); // also in deny_read
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_passes_for_defaults() {
        let cfg = SandboxConfig::default();
        assert!(cfg.validate().is_ok());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_profile_has_required_sections() {
        let cfg = SandboxConfig::default();
        let profile = generate_macos_profile(&cfg);
        assert!(profile.contains("(version 1)"));
        assert!(profile.contains("(allow default)"));
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
        cfg.allowed_domains.clear();
        cfg.denied_domains.push("evil.com".into());
        let profile = generate_macos_profile(&cfg);
        assert!(profile.contains("(deny network-outbound)"));
    }

    #[cfg(target_os = "macos")]
    fn generate_macos_profile(cfg: &SandboxConfig) -> String {
        platform::generate_profile(cfg)
    }
}
