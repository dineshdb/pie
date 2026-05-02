use crate::{Cli, output::OutputFormat, utils::git_repo_root};
use anyhow::Context;
use clap::Parser;
use figment::{
    Figment,
    providers::{Format, Toml},
};
use include_dir::{Dir, include_dir};
use p1e_sandbox::SandboxConfig;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

pub static CONFIG: OnceLock<ResolvedConfig> = OnceLock::new();

/// The embedded `.pie/` directory compiled into the binary.
pub(crate) static EMBEDDED_PIE_DIR: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/.pie");

pub fn pie_home() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".pie")
}

pub fn logs_dir() -> PathBuf {
    let dir = pie_home().join("logs");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Provider configuration shared between CLI args and TOML providers.
#[derive(Debug, Clone, Default, Deserialize, Parser)]
#[serde(default)]
pub struct ProviderConfig {
    #[arg(short, long, env = "OPENAI_MODEL")]
    pub model: Option<String>,

    /// API base URL for OpenAI-compatible providers
    #[arg(long, env = "OPENAI_BASE_URL")]
    pub base_url: Option<String>,

    /// API key for OpenAI-compatible providers
    #[arg(long, env = "OPENAI_API_KEY")]
    pub api_key: Option<String>,

    /// Sampling temperature (config file only)
    #[arg(skip)]
    pub temperature: Option<f32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PieConfig {
    pub default_provider: Option<String>,
    #[serde(default)]
    pub provider: HashMap<String, ProviderConfig>,
    pub agent: Option<AgentConfig>,
    pub sandbox: Option<SandboxConfig>,
    pub output_format: Option<String>,
    pub log_level: Option<String>,
    #[serde(default)]
    pub hooks: Vec<crate::hook::Hook>,
    pub hooks_timeout_ms: Option<u64>,
}

impl PieConfig {
    pub fn output_format(&self) -> OutputFormat {
        match self.output_format.as_deref() {
            Some("json") => OutputFormat::Json,
            Some("markdown" | "md") => OutputFormat::Markdown,
            _ => OutputFormat::Default,
        }
    }

    pub fn log_level(&self) -> &str {
        self.log_level.as_deref().unwrap_or("info")
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentConfig {
    pub max_steps: Option<u32>,
}

#[derive(Debug)]
pub struct ResolvedConfig {
    pub provider: ResolvedProvider,
    pub max_steps: u32,
    pub output_format: OutputFormat,
    pub log_level: String,
    pub debug: bool,
    pub hooks: crate::hook::HooksManager,
}

impl ProviderConfig {
    pub fn merge(self, other: Self) -> Self {
        let model = other.model.or(self.model);
        let base_url = other.base_url.or(self.base_url);
        let api_key = other.api_key.or(self.api_key);
        let temperature = other.temperature.or(self.temperature);

        Self {
            model,
            base_url,
            api_key,
            temperature,
        }
    }
}

impl TryFrom<(Cli, PieConfig)> for ResolvedConfig {
    type Error = anyhow::Error;

    fn try_from((cli, pie): (Cli, PieConfig)) -> Result<Self, Self::Error> {
        let provider_name = cli.provider.as_deref().or(pie.default_provider.as_deref());

        let provider_cfg = match provider_name {
            Some(name) => pie
                .provider
                .get(name)
                .cloned()
                .context(format!("provider '{name}' not found in config"))?,
            None => ProviderConfig::default(),
        };

        let output_format = match cli.output_format() {
            OutputFormat::Default => pie.output_format(),
            format => format,
        };

        let provider = provider_cfg.merge(cli.provider_config);
        let max_steps = pie.agent.as_ref().and_then(|a| a.max_steps).unwrap_or(25);

        let mut resolved_provider = ResolvedProvider::try_from(provider)?;
        if let Some(name) = provider_name {
            resolved_provider.name = name.to_string();
        }

        let log_level = if cli.debug { "debug" } else { pie.log_level() }.to_string();
        let hooks = crate::hook::HooksManager::new(pie.hooks, pie.hooks_timeout_ms);

        Ok(Self {
            provider: resolved_provider,
            max_steps,
            output_format,
            log_level,
            debug: cli.debug,
            hooks,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedProvider {
    pub name: String,
    pub model: String,
    pub base_url: String,
    pub api_key: String,
    #[allow(dead_code)]
    pub temperature: Option<f32>,
}

impl TryFrom<ProviderConfig> for ResolvedProvider {
    type Error = anyhow::Error;

    fn try_from(provider: ProviderConfig) -> Result<Self, Self::Error> {
        Ok(ResolvedProvider {
            name: "default".to_string(), // Will be overwritten by ResolvedConfig conversion
            model: provider
                .model
                .context("model is required (set --model, OPENAI_MODEL, or config provider)")?,
            base_url: provider.base_url.context(
                "base URL is required (set --base-url, OPENAI_BASE_URL, or config provider)",
            )?,
            api_key: provider.api_key.unwrap_or_default(),
            temperature: provider.temperature,
        })
    }
}

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
        if dir.exists()
            && let Ok(entries) = std::fs::read_dir(dir)
        {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("toml")
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

/// Build sandbox settings from config. Uses pie.toml `[sandbox]` if present,
/// otherwise falls back to defaults.
pub fn build_sandbox(pie_config: &PieConfig) -> Arc<SandboxConfig> {
    let sandbox = pie_config.sandbox.clone().unwrap_or_default();
    if let Err(warnings) = sandbox.validate() {
        for w in &warnings {
            tracing::warn!("sandbox config: {w}");
        }
    }
    Arc::new(sandbox)
}
