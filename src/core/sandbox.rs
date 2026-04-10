use crate::core::config::pie_home;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// Sandbox configuration stored at ~/.pie/sandbox.json
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

/// Configuration file written for srt to consume (~/.pie/srt-settings.json)
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SrtSettings {
    network: SrtNetwork,
    filesystem: SrtFilesystem,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SrtNetwork {
    #[serde(default)]
    allowed_domains: Vec<String>,
    #[serde(default)]
    denied_domains: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SrtFilesystem {
    #[serde(default)]
    deny_read: Vec<String>,
    #[serde(default)]
    allow_write: Vec<String>,
    #[serde(default)]
    deny_write: Vec<String>,
}

impl From<&SandboxConfig> for SrtSettings {
    fn from(cfg: &SandboxConfig) -> Self {
        Self {
            network: SrtNetwork {
                allowed_domains: cfg.network.allowed_domains.clone(),
                denied_domains: cfg.network.denied_domains.clone(),
            },
            filesystem: SrtFilesystem {
                deny_read: cfg.filesystem.deny_read.clone(),
                allow_write: cfg.filesystem.allow_write.clone(),
                deny_write: cfg.filesystem.deny_write.clone(),
            },
        }
    }
}

fn config_path() -> PathBuf {
    pie_home().join("sandbox.json")
}

fn srt_settings_path() -> PathBuf {
    pie_home().join("srt-settings.json")
}

/// Load sandbox config from ~/.pie/sandbox.json, falling back to defaults.
pub fn load_config() -> SandboxConfig {
    let path = config_path();
    match fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_else(|e| {
            tracing::warn!("Invalid sandbox config at {}: {e}", path.display());
            SandboxConfig::default()
        }),
        Err(_) => SandboxConfig::default(),
    }
}

/// Write the srt-compatible settings file and return the settings path.
/// Returns an error if srt is not installed.
pub fn prepare(cfg: &SandboxConfig) -> anyhow::Result<PathBuf> {
    if !is_srt_available() {
        anyhow::bail!(
            "srt not found on PATH. Install it: npm install -g @anthropic-ai/sandbox-runtime"
        );
    }
    let path = srt_settings_path();
    let settings = SrtSettings::from(cfg);
    let json = serde_json::to_string_pretty(&settings)
        .map_err(|e| anyhow::anyhow!("Failed to serialize srt settings: {e}"))?;
    fs::write(&path, &json)
        .map_err(|e| anyhow::anyhow!("Failed to write {}: {e}", path.display()))?;
    Ok(path)
}

/// Check if `srt` binary is available on PATH.
pub fn is_srt_available() -> bool {
    Command::new("which")
        .arg("srt")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Build a sandboxed command using `srt`.
pub fn build_command(cmd: &str, settings_path: &PathBuf) -> Command {
    tracing::debug!(settings = %settings_path.display(), cmd, "sandbox:");
    let mut c = Command::new("srt");
    c.arg("--settings")
        .arg(settings_path)
        .arg("sh")
        .arg("-c")
        .arg(cmd);
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_serializes_to_srt_format() {
        let cfg = SandboxConfig::default();
        let settings = SrtSettings::from(&cfg);
        let json = serde_json::to_string_pretty(&settings).unwrap();
        assert!(json.contains("allowedDomains"));
        assert!(json.contains("denyRead"));
        assert!(json.contains("allowWrite"));
    }

    #[test]
    fn default_config_has_sensible_defaults() {
        let cfg = SandboxConfig::default();
        assert!(!cfg.network.allowed_domains.is_empty());
        assert!(cfg.network.denied_domains.is_empty());
        assert!(!cfg.filesystem.deny_read.is_empty());
        assert!(!cfg.filesystem.allow_write.is_empty());
        assert!(!cfg.filesystem.deny_write.is_empty());
    }

    #[test]
    fn build_command_wraps_with_srt() {
        let path = PathBuf::from("/tmp/test-srt-settings.json");
        let cmd = build_command("echo hello", &path);
        assert_eq!(cmd.get_program().to_string_lossy(), "srt");
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(args.contains(&"--settings".to_string()));
        assert!(args.contains(&"/tmp/test-srt-settings.json".to_string()));
    }

    #[test]
    fn load_config_missing_file_returns_default() {
        let _cfg = load_config();
    }
}
