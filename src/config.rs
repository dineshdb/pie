use crate::{Cli, output::OutputFormat, utils::git_repo_root};
use anyhow::Context;
use clap::Parser;
use figment::{
    Figment,
    providers::{Format, Toml},
};
use include_dir::{Dir, include_dir};
use p1e_srt::SandboxConfig;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

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

/// Provider configuration shared between CLI args and TOML profiles.
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
    pub default_profile: Option<String>,
    #[serde(default)]
    pub profiles: HashMap<String, ProviderConfig>,
    pub agent: Option<AgentConfig>,
    pub sandbox: Option<SandboxConfig>,
    pub output_format: Option<String>,
    pub log_level: Option<String>,
}

impl PieConfig {
    pub fn output_format(&self) -> OutputFormat {
        match self.output_format.as_deref().unwrap_or("default") {
            "json" => OutputFormat::Json,
            "markdown" | "md" => OutputFormat::Markdown,
            _ => OutputFormat::Default,
        }
    }

    pub fn log_level(&self) -> &str {
        self.log_level.as_deref().unwrap_or("warn")
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
        let profile_name = cli
            .profile
            .as_deref()
            .or(pie.default_profile.as_deref())
            .unwrap_or_default();

        let profile = pie
            .profiles
            .get(profile_name)
            .cloned()
            .context("profile not found")?;

        let output_format = match (cli.output_format(), pie.output_format()) {
            (OutputFormat::Default, _) => pie.output_format(),
            _ => cli.output_format(),
        };

        let provider = cli.provider.merge(profile);
        let max_steps = pie.agent.as_ref().and_then(|a| a.max_steps).unwrap_or(25);

        let log_level = if cli.debug { "debug" } else { pie.log_level() };
        let mut resolved_provider = ResolvedProvider::try_from(provider.clone())?;
        resolved_provider.name = profile_name.to_string();

        Ok(Self {
            provider: resolved_provider,
            max_steps,
            output_format,
            log_level: log_level.to_string(),
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
            model: provider.model.context(
                "base URL is required (set --base-url, OPENAI_BASE_URL, or config profile)",
            )?,
            base_url: provider.base_url.context(
                "base URL is required (set --base-url, OPENAI_BASE_URL, or config profile)",
            )?,
            api_key: provider.api_key.unwrap_or_default(),
            temperature: provider.temperature,
        })
    }
}

pub fn load_config() -> anyhow::Result<PieConfig> {
    let global = pie_home().join("pie.toml");
    let project = git_repo_root().map(|root| PathBuf::from(root).join(".pie").join("pie.toml"));

    let mut figment = Figment::new().merge(Toml::file_exact(global));
    if let Some(p) = project.filter(|p| p.exists()) {
        figment = figment.merge(Toml::file_exact(p));
    }

    figment
        .extract()
        .map_err(|e| anyhow::anyhow!("config parse error: {e}"))
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
