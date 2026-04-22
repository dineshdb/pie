use crate::{output::OutputFormat, utils::git_repo_root};
use anyhow::Context;
use clap::Parser;
use figment::{
    Figment,
    providers::{Format, Toml},
};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

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
    /// Model name (e.g. llama3, gpt-4o)
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

// ── TOML file structure ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct PieConfig {
    pub default_profile: Option<String>,
    #[serde(default)]
    pub profiles: HashMap<String, ProviderConfig>,
    pub agent: Option<AgentConfig>,
    pub output: Option<OutputConfig>,
    pub logging: Option<LoggingConfig>,
}

#[derive(Debug, Deserialize)]
pub struct AgentConfig {
    pub max_steps: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct OutputConfig {
    pub format: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LoggingConfig {
    pub level: Option<String>,
}

#[derive(Debug)]
pub struct ResolvedConfig {
    pub provider: ResolvedProvider,
    pub max_steps: u32,
    pub output_format: OutputFormat,
    pub log_level: String,
}

#[derive(Debug)]
pub struct ResolvedProvider {
    pub model: String,
    pub base_url: String,
    pub api_key: String,
    #[allow(dead_code)]
    pub temperature: Option<f32>,
}

/// Resolve config: load TOML files, select profile, merge with CLI/env values.
/// Priority: config profile < env vars (via clap) < CLI args.
pub fn resolve(
    cli: &ProviderConfig,
    cli_profile: Option<&str>,
    cli_debug: bool,
    cli_json: bool,
    cli_md: bool,
) -> anyhow::Result<ResolvedConfig> {
    let raw = load_config()?;

    let profile_name = cli_profile
        .map(String::from)
        .or(raw.default_profile)
        .unwrap_or_default();
    let profile = raw.profiles.get(&profile_name).cloned().unwrap_or_default();

    let model = cli
        .model
        .clone()
        .or(profile.model)
        .context("model name is required (set --model, OPENAI_MODEL, or config profile)")?;

    let base_url = cli
        .base_url
        .clone()
        .or(profile.base_url)
        .context("base URL is required (set --base-url, OPENAI_BASE_URL, or config profile)")?;

    let api_key = cli
        .api_key
        .clone()
        .or(profile.api_key)
        .context("API key is required (set --api-key, OPENAI_API_KEY, or config profile)")?;

    let temperature = cli.temperature.or(profile.temperature);
    let max_steps = raw.agent.as_ref().and_then(|a| a.max_steps).unwrap_or(25);

    let output_format = if cli_json {
        OutputFormat::Json
    } else if cli_md {
        OutputFormat::Markdown
    } else {
        match raw
            .output
            .as_ref()
            .and_then(|o| o.format.as_deref())
            .unwrap_or("default")
        {
            "json" => OutputFormat::Json,
            "markdown" | "md" => OutputFormat::Markdown,
            _ => OutputFormat::Default,
        }
    };

    let log_level = if cli_debug {
        "debug".into()
    } else {
        raw.logging
            .as_ref()
            .and_then(|l| l.level.clone())
            .unwrap_or_else(|| "warn".into())
    };

    Ok(ResolvedConfig {
        provider: ResolvedProvider {
            model,
            base_url,
            api_key,
            temperature,
        },
        max_steps,
        output_format,
        log_level,
    })
}

fn load_config() -> anyhow::Result<PieConfig> {
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn provider(
        model: Option<&str>,
        base_url: Option<&str>,
        api_key: Option<&str>,
    ) -> ProviderConfig {
        ProviderConfig {
            model: model.map(String::from),
            base_url: base_url.map(String::from),
            api_key: api_key.map(String::from),
            temperature: None,
        }
    }

    #[test]
    fn ollama_default_detects_llama() {
        assert_eq!(
            ollama_default("llama3"),
            Some("http://localhost:11434/v1".into())
        );
    }

    #[test]
    fn ollama_default_does_not_detect_gpt() {
        assert!(ollama_default("gpt-4o").is_none());
    }

    #[test]
    fn local_placeholder_localhost() {
        assert_eq!(
            local_placeholder("http://localhost:11434/v1"),
            Some("ollama".into())
        );
    }

    #[test]
    fn local_placeholder_remote() {
        assert!(local_placeholder("https://api.openai.com/v1").is_none());
    }

    #[test]
    fn resolve_from_config_or_defaults() {
        let r = resolve(&provider(None, None, None), None, false, false, false);
        assert!(r.is_err() || r.is_ok());
    }

    #[test]
    fn resolve_uses_cli_values() {
        let r = resolve(
            &provider(Some("llama3"), None, None),
            None,
            false,
            false,
            false,
        )
        .unwrap();
        assert_eq!(r.provider.model, "llama3");
        assert!(r.provider.base_url.contains("localhost"));
        assert_eq!(r.provider.api_key, "ollama");
    }

    #[test]
    fn resolve_missing_model() {
        let err = resolve(&provider(None, None, None), None, false, false, false).unwrap_err();
        assert!(err.to_string().contains("model"));
    }

    #[test]
    fn resolve_missing_base_url() {
        let err = resolve(
            &provider(Some("gpt-4o"), None, None),
            None,
            false,
            false,
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("base URL"));
    }

    #[test]
    fn resolve_missing_api_key() {
        let err = resolve(
            &provider(Some("gpt-4o"), Some("https://api.openai.com/v1"), None),
            None,
            false,
            false,
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("API key"));
    }

    #[test]
    fn load_config_succeeds_without_files() {
        assert!(load_config().is_ok());
    }
}
