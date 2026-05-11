use crate::output::OutputFormat;
use clap::Args;
use p1e_sandbox::SandboxConfig;
use redact::Secret;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Default, Args)]
pub struct ProviderEndpoint {
    /// Well-known provider name (config only)
    #[arg(skip)]
    pub name: Option<String>,

    /// OpenAI-compatible base URL
    #[arg(long, alias = "openai-url", alias = "base-url")]
    pub openai: Option<String>,

    /// Anthropic API base URL
    #[arg(long, alias = "anthropic-url")]
    pub anthropic: Option<String>,
}

impl Serialize for ProviderEndpoint {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if self.openai.is_some() || self.anthropic.is_some() {
            CustomEndpoint {
                openai: self.openai.clone(),
                anthropic: self.anthropic.clone(),
            }
            .serialize(serializer)
        } else {
            self.name
                .as_deref()
                .unwrap_or("default")
                .serialize(serializer)
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ProviderEndpointDto {
    Name(String),
    Custom(CustomEndpoint),
}

#[derive(Serialize, Deserialize)]
struct CustomEndpoint {
    #[serde(alias = "base_url", alias = "endpoint", alias = "openai_url")]
    openai: Option<String>,
    #[serde(rename = "anthropic_url")]
    anthropic: Option<String>,
}

impl<'de> Deserialize<'de> for ProviderEndpoint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        match ProviderEndpointDto::deserialize(deserializer)? {
            ProviderEndpointDto::Name(n) => Ok(ProviderEndpoint {
                name: Some(n),
                ..Default::default()
            }),
            ProviderEndpointDto::Custom(CustomEndpoint { openai, anthropic }) => {
                Ok(ProviderEndpoint {
                    name: None,
                    openai,
                    anthropic,
                })
            }
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, Args)]
pub struct ProviderBaseUrl {
    /// OpenAI-compatible base URL
    #[arg(long, alias = "openai-url", alias = "base-url")]
    #[serde(rename = "openai_url", alias = "base_url")]
    pub openai: Option<String>,

    /// Anthropic API base URL
    #[arg(long, alias = "anthropic-url")]
    #[serde(rename = "anthropic_url")]
    pub anthropic: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Args)]
#[serde(default)]
pub struct ProviderConfig {
    #[arg(short, long)]
    pub model: Option<String>,

    #[command(flatten)]
    #[serde(alias = "base_url")]
    pub endpoint: ProviderEndpoint,

    /// API key for the provider
    #[arg(long)]
    pub api_key: Option<Secret<String>>,

    /// Sampling temperature (config file only)
    #[arg(skip)]
    pub temperature: Option<f32>,
}

impl ProviderConfig {
    pub fn merge(self, other: Self) -> Self {
        let model = other.model.or(self.model);

        let mut endpoint = self.endpoint;
        if other.endpoint.openai.is_some() || other.endpoint.anthropic.is_some() {
            endpoint.openai = other.endpoint.openai;
            endpoint.anthropic = other.endpoint.anthropic;
            endpoint.name = None; // Priority to custom
        } else if other.endpoint.name.is_some() {
            endpoint.name = other.endpoint.name;
            endpoint.openai = None;
            endpoint.anthropic = None;
        }

        let api_key = other.api_key.or(self.api_key);
        let temperature = other.temperature.or(self.temperature);

        Self {
            model,
            endpoint,
            api_key,
            temperature,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PieConfig {
    pub default_provider: Option<String>,
    #[serde(default)]
    pub provider: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub secrets: HashMap<String, Secret<String>>,
    #[serde(default)]
    pub model: HashMap<String, ModelTier>,
    pub agent: Option<GlobalAgentConfig>,
    pub sandbox: Option<SandboxConfig>,
    pub output_format: Option<String>,
    pub log_level: Option<String>,
    #[serde(default)]
    pub hooks: Vec<crate::hook::HookDef>,
    pub hooks_timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Hash)]
pub struct CliConfig {
    pub command: String,
    pub description: Option<String>,
    #[serde(default)]
    pub man: bool,
}

/// A named model tier in `[model.<name>]` sections.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelTier {
    pub provider: String,
    pub model: Option<String>,
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
pub struct GlobalAgentConfig {
    pub max_steps: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct LaunchConfig {
    pub sandbox: Option<SandboxConfig>,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub aliases: Vec<String>,
}
