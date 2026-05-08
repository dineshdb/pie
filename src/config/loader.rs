use super::types::{LaunchConfig, PieConfig, ProviderBaseUrl};
use crate::utils::git_repo_root;
use anyhow::Context;
use figment::{
    Figment,
    providers::{Format, Toml},
};
use include_dir::{Dir, include_dir};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

/// The embedded `.pie/` directory compiled into the binary.
pub static EMBEDDED_PIE_DIR: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/.pie");

pub fn pie_home() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".pie")
}

pub fn logs_dir() -> PathBuf {
    let dir = pie_home().join("logs");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

static PROVIDERS_DATA: OnceLock<HashMap<String, ProviderBaseUrl>> = OnceLock::new();

pub fn get_providers_data() -> anyhow::Result<&'static HashMap<String, ProviderBaseUrl>> {
    if let Some(data) = PROVIDERS_DATA.get() {
        return Ok(data);
    }
    let data = load_providers_data()?;
    Ok(PROVIDERS_DATA.get_or_init(|| data))
}

fn load_providers_data() -> anyhow::Result<HashMap<String, ProviderBaseUrl>> {
    let file = EMBEDDED_PIE_DIR
        .get_file("providers.toml")
        .context("critical asset 'providers.toml' must exist in embedded dir")?;
    let content = file
        .contents_utf8()
        .context("critical asset 'providers.toml' must be valid UTF-8")?;
    let data: HashMap<String, ProviderBaseUrl> = Figment::new()
        .merge(Toml::string(content))
        .extract()
        .context("failed to parse embedded 'providers.toml'")?;
    Ok(data)
}

/// Load the PIE configuration from global and project-specific paths.
pub fn load_config() -> anyhow::Result<PieConfig> {
    let global_home = pie_home();
    let global = global_home.join("pie.toml");
    let project_root = git_repo_root().map(PathBuf::from);
    let project_pie = project_root
        .as_ref()
        .map(|root| root.join(".pie").join("pie.toml"));

    let mut figment = Figment::new().merge(Toml::file_exact(global));
    if let Some(p) = project_pie.filter(|p| p.exists()) {
        figment = figment.merge(Toml::file_exact(p));
    }

    let mut pie_config: PieConfig = figment
        .extract()
        .map_err(|e| anyhow::anyhow!("config parse error: {e}"))?;

    // Modular Hook Loading from plugins directory
    let mut scan_dirs = Vec::new();
    scan_dirs.push(global_home.join("plugins"));
    if let Some(root) = &project_root {
        scan_dirs.push(root.join(".pie").join("plugins"));
    }

    for dir in scan_dirs {
        if !dir.exists() {
            continue;
        }

        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();

                if path.is_dir() {
                    let plugin_toml = path.join("plugin.toml");
                    if plugin_toml.exists()
                        && let Ok(content) = std::fs::read_to_string(&plugin_toml)
                        && let Ok(mut plugin_config) = Figment::new()
                            .merge(Toml::string(&content))
                            .extract::<PieConfig>()
                    {
                        let plugin_dir_str = path.to_string_lossy().to_string();
                        for hook in &mut plugin_config.hooks {
                            hook.plugin_dir = Some(plugin_dir_str.clone());
                            if hook.handler.starts_with("./") {
                                let abs_handler = path
                                    .join(&hook.handler)
                                    .canonicalize()
                                    .unwrap_or_else(|_| path.join(&hook.handler));
                                hook.handler = abs_handler.to_string_lossy().to_string();
                            }
                        }
                        pie_config.hooks.extend(plugin_config.hooks);
                        if let Some(to) = plugin_config.hooks_timeout_ms {
                            pie_config.hooks_timeout_ms = Some(to);
                        }
                    }
                } else if path.extension().and_then(|s| s.to_str()) == Some("toml")
                    && let Ok(content) = std::fs::read_to_string(&path)
                    && let Ok(plugin_config) = Figment::new()
                        .merge(Toml::string(&content))
                        .extract::<PieConfig>()
                {
                    pie_config.hooks.extend(plugin_config.hooks);
                    if let Some(to) = plugin_config.hooks_timeout_ms {
                        pie_config.hooks_timeout_ms = Some(to);
                    }
                }
            }
        }
    }

    Ok(pie_config)
}

/// Load launch configurations from embedded and global paths.
pub fn load_launch_config() -> anyhow::Result<HashMap<String, LaunchConfig>> {
    let mut figment = Figment::new();

    // 1. Embedded defaults
    if let Some(file) = EMBEDDED_PIE_DIR.get_file("launch.toml") {
        let content = file.contents_utf8().context("launch.toml is not UTF-8")?;
        figment = figment.merge(Toml::string(content));
    }

    // 2. Global overrides
    let global = pie_home().join("launch.toml");
    if global.exists() {
        figment = figment.merge(Toml::file_exact(global));
    }

    let config: HashMap<String, LaunchConfig> = figment
        .extract()
        .map_err(|e| anyhow::anyhow!("launch config parse error: {e}"))?;

    Ok(config)
}
