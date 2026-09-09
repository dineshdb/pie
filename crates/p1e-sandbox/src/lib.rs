use agentsdk::core::sandbox::{FSProvider, SandboxError, SandboxOutput};
use async_trait::async_trait;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

/// A capability that a skill declares it needs, and an agent/schedule must grant.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Permission {
    Unsandboxed(String),
    FilesystemRead(String),
    FilesystemWrite(String),
    Network(String),
    AppleEvents,
}

impl Permission {
    pub fn apply_to(&self, cfg: &mut SandboxConfig) {
        if !cfg.permissions.contains(self) {
            cfg.permissions.push(self.clone());
        }

        match self {
            Self::FilesystemRead(path) if !cfg.allow_read.contains(path) => {
                cfg.allow_read.push(path.clone());
            }
            Self::FilesystemWrite(path) if !cfg.allow_write.contains(path) => {
                cfg.allow_write.push(path.clone());
            }
            Self::Network(domain) if !cfg.allowed_domains.contains(domain) => {
                cfg.allowed_domains.push(domain.clone());
            }
            _ => {}
        }
    }
}

impl std::str::FromStr for Permission {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Handle schemes (both URI and colon format)
        if let Some(value) = s
            .strip_prefix("unsandboxed://")
            .or_else(|| s.strip_prefix("unsandboxed:"))
        {
            Ok(Self::Unsandboxed(value.to_string()))
        } else if let Some(value) = s
            .strip_prefix("fs-read://")
            .or_else(|| s.strip_prefix("fs-read:"))
        {
            Ok(Self::FilesystemRead(value.to_string()))
        } else if let Some(value) = s
            .strip_prefix("fs-write://")
            .or_else(|| s.strip_prefix("fs-write:"))
        {
            Ok(Self::FilesystemWrite(value.to_string()))
        } else if let Some(value) = s
            .strip_prefix("network://")
            .or_else(|| s.strip_prefix("network:"))
        {
            Ok(Self::Network(value.to_string()))
        } else if s == "apple-events" || s == "apple-events://" {
            Ok(Self::AppleEvents)
        } else {
            Err(format!("unknown permission: {s}"))
        }
    }
}

impl std::fmt::Display for Permission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsandboxed(bin) => write!(f, "unsandboxed:{bin}"),
            Self::FilesystemRead(path) => write!(f, "fs-read:{path}"),
            Self::FilesystemWrite(path) => write!(f, "fs-write:{path}"),
            Self::Network(domain) => write!(f, "network:{domain}"),
            Self::AppleEvents => write!(f, "apple-events"),
        }
    }
}

impl Serialize for Permission {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Permission {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

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
    pub allowed_bins: Vec<String>,
    pub disallowed_bins: Vec<String>,
    #[serde(default)]
    pub permissions: Vec<Permission>,
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
            allowed_bins: vec![],
            disallowed_bins: vec!["sudo".into(), "su".into()],
            permissions: vec![],
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SecurityMode {
    #[default]
    Guard,
    Isolation,
}

impl std::fmt::Display for SecurityMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SecurityMode::Guard => write!(f, "Guard"),
            SecurityMode::Isolation => write!(f, "Isolation"),
        }
    }
}

#[derive(Debug, Default)]
pub struct SecurityReport {
    pub is_safe: bool,
    pub errors: Vec<String>,
    pub mode: SecurityMode,
}

impl SandboxConfig {
    /// Comprehensive security check for a command string. Relative paths in
    /// the command resolve against `base` (the run's working directory).
    pub fn check_command_safety(&self, cmd: &str, base: &Path) -> SecurityReport {
        let mode = if self.allowed_bins.is_empty() {
            SecurityMode::Guard
        } else {
            SecurityMode::Isolation
        };

        let mut report = SecurityReport {
            is_safe: true,
            errors: Vec::new(),
            mode,
        };

        let words = match shell_words::split(cmd) {
            Ok(w) => w,
            Err(e) => {
                report.is_safe = false;
                report.errors.push(format!("Failed to parse command: {e}"));
                return report;
            }
        };

        let indirection_operators = ["|", ">", ">>", "<", "&>", "2>", "1>"];
        let mut next_is_cmd = true;

        for word in words {
            if indirection_operators.contains(&word.as_str()) {
                next_is_cmd = true;
                continue;
            }

            if next_is_cmd {
                let is_disallowed = self.disallowed_bins.iter().any(|disallowed| {
                    &word == disallowed || word.ends_with(&format!("/{}", disallowed))
                });
                if is_disallowed {
                    report.is_safe = false;
                    report
                        .errors
                        .push(format!("Binary is explicitly disallowed: {word}"));
                }

                match report.mode {
                    SecurityMode::Isolation => {
                        // Isolation Mode: Must be in allowed_bins
                        let is_allowed = self.allowed_bins.iter().any(|allowed| {
                            &word == allowed || word.ends_with(&format!("/{}", allowed))
                        });
                        if !is_allowed {
                            report.is_safe = false;
                            report.errors.push(format!(
                                "Binary is not in the allowed list (Isolation Mode): {word}"
                            ));
                        }
                    }
                    SecurityMode::Guard => {
                        // Guard Mode: already checked disallowed_bins
                    }
                }
                next_is_cmd = false;
            }

            if word.contains('/') || word.contains('.') {
                match self.is_within_allowed_paths(&word, base) {
                    Ok(allowed) => {
                        if !allowed {
                            report.is_safe = false;
                            report
                                .errors
                                .push(format!("Path is outside allowed sandbox paths: {word}"));
                        }
                    }
                    Err(e) => {
                        tracing::trace!(path = %word, error = %e, "Could not validate path safety");
                    }
                }
            }
        }

        report
    }

    /// Build a sandboxed shell command with standard agent environment defaults.
    /// The command runs in `base` (the run's working directory).
    /// Returns the command.
    pub fn build_safe_command(
        &self,
        cmd: &str,
        bin_dirs: &[std::path::PathBuf],
        base: &Path,
    ) -> Result<Command, String> {
        let report = self.check_command_safety(cmd, base);
        if !report.is_safe {
            return Err(report.errors.join("; "));
        }

        let mut c = build_shell_command(cmd, self, base);
        c.env("GIT_TERMINAL_PROMPT", "0");
        c.env("PAGER", "cat");

        if !bin_dirs.is_empty()
            && let Some(old_path) = std::env::var_os("PATH")
        {
            let mut paths = std::env::split_paths(&old_path).collect::<Vec<_>>();
            // Insert in reverse order so the most specific (repo local) comes first if we insert at 0
            for bin_dir in bin_dirs.iter().rev() {
                if !paths.contains(bin_dir) {
                    paths.insert(0, bin_dir.clone());
                }
            }
            if let Ok(new_path) = std::env::join_paths(paths) {
                c.env("PATH", new_path);
            }
        }

        Ok(c)
    }

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

    /// Merge another `SandboxConfig` on top of this one.
    /// Fields from `other` override or extend this config's fields.
    pub fn merge(&mut self, other: &SandboxConfig) {
        self.deny_read.extend_from_slice(&other.deny_read);
        self.allow_read.extend_from_slice(&other.allow_read);
        self.allow_write.extend_from_slice(&other.allow_write);
        self.deny_write.extend_from_slice(&other.deny_write);
        self.allowed_domains
            .extend_from_slice(&other.allowed_domains);
        self.denied_domains.extend_from_slice(&other.denied_domains);
        self.allowed_bins.extend_from_slice(&other.allowed_bins);
        self.disallowed_bins
            .extend_from_slice(&other.disallowed_bins);
        for perm in &other.permissions {
            if !self.permissions.contains(perm) {
                self.permissions.push(perm.clone());
            }
        }
    }

    /// Check if a candidate path falls within any of the allowed read or write paths.
    ///
    /// `base` is the working directory the check applies to: relative
    /// candidates and relative allow/deny entries resolve against it, so a
    /// run's sandbox is fully determined by `(config, base)` and never by
    /// the process cwd.
    pub fn is_within_allowed_paths(&self, candidate: &str, base: &Path) -> std::io::Result<bool> {
        let candidate_path = Path::new(candidate);

        // If it's just a filename without path components, it's relative to the
        // working directory, which is usually allowed.
        if !candidate.contains('/') && !candidate.contains("..") {
            return Ok(true);
        }

        let full_path = if candidate_path.is_absolute() {
            candidate_path.to_path_buf()
        } else {
            base.join(candidate_path)
        };

        // Note: fs::canonicalize only works for existing paths.
        let resolved = match std::fs::canonicalize(&full_path) {
            Ok(p) => p,
            Err(_) => {
                // Fallback to logical normalization for non-existent paths
                let mut depth: i32 = 0;
                for component in candidate.split('/') {
                    match component {
                        "" | "." => continue,
                        ".." => {
                            depth -= 1;
                            if depth < 0 {
                                return Ok(false);
                            }
                        }
                        _ => depth += 1,
                    }
                }
                return Ok(true);
            }
        };

        let mut allowed_paths = self.allow_read.clone();
        allowed_paths.extend(self.allow_write.clone());

        for allowed in allowed_paths {
            let Ok(allowed_canonical) = std::fs::canonicalize(resolve_entry(&allowed, base)) else {
                continue;
            };
            if resolved.starts_with(&allowed_canonical) {
                return Ok(true);
            }
        }

        Ok(false)
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
/// The child runs in `base` (the run's working directory).
pub fn build_command(program: &str, args: &[String], cfg: &SandboxConfig, base: &Path) -> Command {
    let should_sandbox = !cfg.permissions.iter().any(|p| {
        if let Permission::Unsandboxed(b) = p {
            b == program || program.ends_with(&format!("/{b}"))
        } else {
            false
        }
    });

    if !should_sandbox {
        let mut c = Command::new(program);
        c.args(args);
        c.current_dir(base);
        return c;
    }
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    let available = *AVAILABLE.get_or_init(is_sandbox_tool_available);

    if available {
        build_sandboxed(program, args, cfg, base)
    } else {
        tracing::warn!("sandbox tool not found, running unsandboxed");
        let mut c = Command::new(program);
        c.args(args);
        c.current_dir(base);
        c
    }
}

/// Build a sandboxed shell command (via `bash -c`).
/// The child runs in `base` (the run's working directory).
pub fn build_shell_command(cmd: &str, cfg: &SandboxConfig, base: &Path) -> Command {
    let mut should_sandbox_bash = true;
    if let Ok(words) = shell_words::split(cmd)
        && let Some(first) = words.first()
        && cfg.permissions.iter().any(|p| {
            if let Permission::Unsandboxed(b) = p {
                b == first || first.ends_with(&format!("/{b}"))
            } else {
                false
            }
        })
    {
        should_sandbox_bash = false;
    }

    if !should_sandbox_bash {
        let mut c = Command::new("bash");
        c.args(["-c", cmd]);
        c.current_dir(base);
        return c;
    }

    build_command("bash", &["-c".into(), cmd.into()], cfg, base)
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

/// Resolve a config entry to absolute: expand `~`, join `base` if relative.
fn resolve_entry(entry: &str, base: &Path) -> std::path::PathBuf {
    let expanded = expand_tilde(entry);
    let path = Path::new(&expanded);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
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

    pub(crate) fn build(
        program: &str,
        args: &[String],
        cfg: &SandboxConfig,
        base: &Path,
    ) -> Command {
        let profile = generate_profile(cfg, base);
        tracing::trace!(%program, ?args, BINARY);
        let mut c = Command::new(BINARY);
        c.arg("-p").arg(&profile).arg(program).args(args);
        c.current_dir(base);
        c
    }

    pub(crate) fn generate_profile(cfg: &SandboxConfig, base: &Path) -> String {
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
            let resolved = resolve_entry(path, base).to_string_lossy().to_string();
            lines.push(format!("(deny file-read* (subpath \"{resolved}\"))"));
        }

        for path in &cfg.allow_read {
            let resolved = resolve_entry(path, base).to_string_lossy().to_string();
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
                let resolved = resolve_entry(path, base).to_string_lossy().to_string();
                lines.push(format!("(allow file-write* (subpath \"{resolved}\"))"));
            }
        }

        let allow_apple_events = cfg
            .permissions
            .iter()
            .any(|p| matches!(p, Permission::AppleEvents));
        if allow_apple_events {
            lines.push("(allow appleevent-send)".to_string());
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

    pub(crate) fn build(
        program: &str,
        args: &[String],
        cfg: &SandboxConfig,
        base: &Path,
    ) -> Command {
        let mut c = Command::new(BINARY);
        c.arg("--die-with-parent");

        for path in &cfg.allow_read {
            let resolved = resolve_entry(path, base);
            if resolved == Path::new("/dev") {
                c.arg("--dev").arg("/dev");
            } else if resolved == Path::new("/proc") {
                c.arg("--proc").arg("/proc");
            } else {
                c.arg("--ro-bind").arg(&resolved).arg(&resolved);
            }
        }

        for path in &cfg.deny_read {
            let resolved = resolve_entry(path, base);
            c.arg("--tmpfs").arg(&resolved);
        }

        for path in &cfg.allow_write {
            let resolved = resolve_entry(path, base);
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
        c.current_dir(base);
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

fn build_sandboxed(program: &str, args: &[String], cfg: &SandboxConfig, base: &Path) -> Command {
    plat::build(program, args, cfg, base)
}

pub struct PlatformSandbox {
    config: SandboxConfig,
    bin_dirs: Vec<std::path::PathBuf>,
    /// The working directory this sandbox applies to: relative config
    /// entries, relative tool paths, and spawned commands all resolve
    /// against it instead of the process cwd.
    cwd: std::path::PathBuf,
}

impl PlatformSandbox {
    pub fn new(config: SandboxConfig, cwd: impl Into<std::path::PathBuf>) -> Self {
        Self {
            config,
            bin_dirs: Vec::new(),
            cwd: cwd.into(),
        }
    }

    #[must_use]
    pub fn with_bin_dirs(mut self, dirs: Vec<std::path::PathBuf>) -> Self {
        self.bin_dirs = dirs;
        self
    }

    pub fn check_read_access(&self, path: &Path) -> Result<(), SandboxError> {
        let path_str = path.to_string_lossy();
        if self
            .config
            .is_within_allowed_paths(&path_str, &self.cwd)
            .unwrap_or(false)
        {
            Ok(())
        } else {
            Err(SandboxError::ReadDenied(path_str.into_owned()))
        }
    }

    pub fn check_write_access(&self, path: &Path) -> Result<(), SandboxError> {
        let path_str = path.to_string_lossy();
        if self
            .config
            .is_within_allowed_paths(&path_str, &self.cwd)
            .unwrap_or(false)
        {
            Ok(())
        } else {
            Err(SandboxError::WriteDenied(path_str.into_owned()))
        }
    }
}

#[async_trait]
impl FSProvider for PlatformSandbox {
    fn read(&self, path: &Path) -> Result<String, SandboxError> {
        self.check_read_access(path)?;
        Ok(std::fs::read_to_string(path)?)
    }

    fn write(&self, path: &Path, content: &str) -> Result<(), SandboxError> {
        self.check_write_access(path)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(std::fs::write(path, content)?)
    }

    fn list(&self, path: &Path) -> Result<Vec<(String, bool)>, SandboxError> {
        self.check_read_access(path)?;
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = entry.file_type()?.is_dir();
            entries.push((name, is_dir));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(entries)
    }

    fn glob(&self, pattern: &str) -> Result<Vec<String>, SandboxError> {
        // glob crates resolve relative patterns against the process cwd;
        // anchor them to this sandbox's working directory instead.
        let pattern_path = Path::new(pattern);
        let absolute = if pattern_path.is_absolute() {
            pattern.to_string()
        } else {
            self.cwd.join(pattern_path).to_string_lossy().to_string()
        };
        let entries = glob::glob(&absolute).map_err(std::io::Error::other)?;
        let mut paths = Vec::new();
        for entry in entries {
            let path = entry.map_err(std::io::Error::other)?;
            self.check_read_access(&path)?;
            paths.push(path.to_string_lossy().into_owned());
        }
        Ok(paths)
    }

    async fn exec(&self, cmd: &str) -> Result<SandboxOutput, SandboxError> {
        let command = self
            .config
            .build_safe_command(cmd, &self.bin_dirs, &self.cwd)
            .map_err(SandboxError::CommandDenied)?;

        let output = tokio::process::Command::from(command).output().await?;

        Ok(SandboxOutput {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            exit_code: output.status.code().unwrap_or(-1),
        })
    }
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
    fn resolve_entry_expands_tilde() {
        let resolved = resolve_entry("~/test", Path::new("/"));
        let home = dirs::home_dir().expect("home dir exists");
        assert_eq!(resolved, home.join("test"));
    }

    #[test]
    fn resolve_entry_joins_base_for_relative_paths() {
        let resolved = resolve_entry(".", Path::new("/workspaces/proj"));
        assert_eq!(resolved, Path::new("/workspaces/proj"));

        let nested = resolve_entry("src/lib.rs", Path::new("/workspaces/proj"));
        assert_eq!(nested, Path::new("/workspaces/proj/src/lib.rs"));
    }

    #[test]
    fn resolve_entry_leaves_absolute_unchanged() {
        let resolved = resolve_entry("/tmp", Path::new("/somewhere/else"));
        assert_eq!(resolved, Path::new("/tmp"));
    }

    #[test]
    fn path_checks_use_explicit_base_not_process_cwd() {
        // Both candidates exist on disk, so the canonicalize path (not the
        // logical fallback) decides. The process cwd is elsewhere entirely,
        // so any process-cwd leak flips these assertions.
        let root = std::env::temp_dir().join("p1e-sandbox-base-test");
        let dir = root.join("proj");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::create_dir_all(root.join("outside")).unwrap();
        std::fs::write(dir.join("sub").join("file.txt"), "x").unwrap();
        std::fs::write(root.join("outside").join("file.txt"), "x").unwrap();

        let cfg = SandboxConfig {
            // Defaults allow reading `/`, which would make every path pass;
            // an empty read list isolates the write grant under test.
            allow_read: vec![],
            allow_write: vec![".".into()],
            ..SandboxConfig::default()
        };

        assert!(cfg.is_within_allowed_paths("sub/file.txt", &dir).unwrap());
        assert!(
            !cfg.is_within_allowed_paths("../outside/file.txt", &dir)
                .unwrap()
        );
        std::fs::remove_dir_all(&root).ok();
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
        platform::generate_profile(cfg, Path::new("/base/unused/by/absolute/entries"))
    }

    #[test]
    fn permission_roundtrip_display_from_str() {
        let perms = vec![
            Permission::Unsandboxed("osascript".into()),
            Permission::FilesystemRead("~/Library/Mail".into()),
            Permission::FilesystemWrite("./output".into()),
            Permission::Network("imap.gmail.com".into()),
            Permission::AppleEvents,
        ];
        for perm in &perms {
            let s = perm.to_string();
            let parsed: Permission = s.parse().unwrap_or_else(|_| panic!("parse: {s}"));
            assert_eq!(*perm, parsed, "roundtrip failed for {s}");
        }
    }

    #[test]
    fn permission_from_str_backwards_compatibility() {
        let cases = [
            (
                "unsandboxed:osascript",
                Permission::Unsandboxed("osascript".into()),
            ),
            ("fs-read:/tmp", Permission::FilesystemRead("/tmp".into())),
            ("fs-write:/var", Permission::FilesystemWrite("/var".into())),
            (
                "network:google.com",
                Permission::Network("google.com".into()),
            ),
            ("apple-events", Permission::AppleEvents),
        ];

        for (input, expected) in cases {
            let parsed: Permission = input.parse().unwrap();
            assert_eq!(parsed, expected);
        }
    }

    #[test]
    fn permission_from_str_rejects_unknown() {
        assert!("bad-permission".parse::<Permission>().is_err());
        assert!("".parse::<Permission>().is_err());
    }

    #[test]
    fn permission_serialize_deserialize() {
        let perms = vec![
            Permission::Unsandboxed("osascript".into()),
            Permission::AppleEvents,
        ];
        let json = serde_json::to_string(&perms).unwrap();
        let parsed: Vec<Permission> = serde_json::from_str(&json).unwrap();
        assert_eq!(perms, parsed);
    }

    #[test]
    fn permission_apply_apple_events() {
        let mut cfg = SandboxConfig::default();
        Permission::AppleEvents.apply_to(&mut cfg);
        assert!(cfg.permissions.contains(&Permission::AppleEvents));
    }

    #[test]
    fn permission_apply_unsandboxed() {
        let mut cfg = SandboxConfig::default();
        Permission::Unsandboxed("osascript".into()).apply_to(&mut cfg);
        assert!(
            cfg.permissions
                .contains(&Permission::Unsandboxed("osascript".into()))
        );
    }

    #[test]
    fn permission_apply_fs_read() {
        let mut cfg = SandboxConfig::default();
        Permission::FilesystemRead("~/Library/Mail".into()).apply_to(&mut cfg);
        assert!(cfg.allow_read.contains(&"~/Library/Mail".to_string()));
    }

    #[test]
    fn permission_apply_fs_write() {
        let mut cfg = SandboxConfig::default();
        Permission::FilesystemWrite("./output".into()).apply_to(&mut cfg);
        assert!(cfg.allow_write.contains(&"./output".to_string()));
    }

    #[test]
    fn permission_apply_network() {
        let mut cfg = SandboxConfig::default();
        Permission::Network("imap.gmail.com".into()).apply_to(&mut cfg);
        assert!(cfg.allowed_domains.contains(&"imap.gmail.com".to_string()));
    }

    #[test]
    fn permission_equality_and_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        assert!(set.insert(Permission::AppleEvents));
        assert!(!set.insert(Permission::AppleEvents));
        assert!(set.insert(Permission::Unsandboxed("osascript".into())));
        assert_eq!(set.len(), 2);
    }
}
