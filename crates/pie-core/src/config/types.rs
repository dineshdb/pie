use crate::utils::output::OutputFormat;
use clap::Args;
use p1e_sandbox::SandboxConfig;
use redact::Secret;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use url::Url;

#[derive(Debug, Clone, Default, Args)]
pub struct ProviderEndpoint {
    /// Well-known provider name (config only)
    #[arg(skip)]
    pub name: Option<String>,

    /// OpenAI-compatible base URL
    #[arg(long, alias = "openai-url", alias = "base-url", global = true)]
    pub openai: Option<String>,

    /// Anthropic API base URL
    #[arg(long, alias = "anthropic-url", global = true)]
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
    #[arg(short, long, global = true)]
    pub model: Option<String>,

    #[command(flatten)]
    #[serde(alias = "base_url")]
    pub endpoint: ProviderEndpoint,

    /// API key for the provider
    #[arg(long, global = true)]
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
    #[serde(default)]
    pub mcp: HashMap<String, McpServerConfig>,
    /// Per-model token pricing in USD per million tokens, keyed by the
    /// exact model id: `[pricing."claude-sonnet-4"]`. A model without an
    /// entry gets token stats but no cost.
    #[serde(default)]
    pub pricing: HashMap<String, ModelPricing>,
    pub agent: Option<GlobalAgentConfig>,
    pub sandbox: Option<SandboxConfig>,
    pub output_format: Option<String>,
    pub log_level: Option<String>,
}

/// Token pricing for a model, in USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ModelPricing {
    /// Uncached prompt (input) tokens.
    pub input: f64,
    /// Prompt tokens served from the provider's prompt cache. Defaults to
    /// `input` when unset (no discount assumed).
    #[serde(default)]
    pub cached_input: Option<f64>,
    /// Completion (output) tokens.
    pub output: f64,
}

impl ModelPricing {
    pub fn cached_input(&self) -> f64 {
        self.cached_input.unwrap_or(self.input)
    }
}

/// An HTTP-based MCP server under `[mcp.<name>]`. Its tools show up to
/// agents as `{name}__{tool}` once the agent lists `mcp` (or `mcp:<name>`)
/// in its `plugins`.
#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    /// Streamable-HTTP MCP endpoint (e.g. `https://mcp.example.com/mcp`).
    pub url: Url,
    /// Headers sent with every request. A value that exactly matches a
    /// `[secrets]` key is replaced by that secret's value at load time.
    #[serde(default)]
    pub headers: HashMap<String, Secret<String>>,
}

impl Serialize for McpServerConfig {
    /// Header values are secrets by construction; only their names survive
    /// serialization.
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("McpServerConfig", 2)?;
        s.serialize_field("url", self.url.as_str())?;
        s.serialize_field("headers", &self.headers.keys().collect::<Vec<_>>())?;
        s.end()
    }
}

impl McpServerConfig {
    /// Replace header values that exactly match a `[secrets]` key with the
    /// secret's value; literal values pass through untouched.
    pub fn resolve_secrets(&mut self, secrets: &HashMap<String, Secret<String>>) {
        for header in self.headers.values_mut() {
            if let Some(val) = secrets.get(header.expose_secret()) {
                *header = val.clone();
            }
        }
    }
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
            Some("json") => OutputFormat::Json(None),
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
    #[serde(default)]
    pub retry: RetryConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct RetryConfig {
    #[serde(default)]
    pub rate_limit: RateLimitConfig,
    #[serde(default)]
    pub api_error: ApiErrorConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct RateLimitConfig {
    pub max_errors: u32,
    pub retry_delay_secs: u64,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            max_errors: 5,
            retry_delay_secs: 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct ApiErrorConfig {
    pub max_errors: u32,
    pub retry_delay_secs: u64,
}

impl Default for ApiErrorConfig {
    fn default() -> Self {
        Self {
            max_errors: 10,
            retry_delay_secs: 10,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct LaunchConfig {
    pub sandbox: Option<SandboxConfig>,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub aliases: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use figment::Figment;
    use figment::providers::{Format, Toml};

    fn parse(toml: &str) -> PieConfig {
        Figment::new().merge(Toml::string(toml)).extract().unwrap()
    }

    #[test]
    fn parse_mcp_server_config() {
        let pie = parse(
            r#"
[mcp.deepwiki]
url = "https://mcp.deepwiki.com/mcp"

[mcp.context7]
url = "https://mcp.context7.com/mcp"
[mcp.context7.headers]
CONTEXT7_API_KEY = "context7_key"
"#,
        );

        assert_eq!(pie.mcp.len(), 2);
        let deepwiki = &pie.mcp["deepwiki"];
        assert_eq!(deepwiki.url.as_str(), "https://mcp.deepwiki.com/mcp");
        assert!(deepwiki.headers.is_empty());

        let context7 = &pie.mcp["context7"];
        assert_eq!(
            context7.headers["CONTEXT7_API_KEY"].expose_secret(),
            "context7_key"
        );
    }

    #[test]
    fn mcp_section_is_optional() {
        let pie = parse("log_level = \"info\"");
        assert!(pie.mcp.is_empty());
    }

    #[test]
    fn parse_pricing_defaults_cached_to_input_rate() {
        let pie = parse(
            r#"
[pricing."some-model"]
input = 1.5
output = 6.0

[pricing."other-model"]
input = 1.0
cached_input = 0.1
output = 2.0
"#,
        );
        assert_eq!(pie.pricing.len(), 2);
        let some = &pie.pricing["some-model"];
        assert!((some.input - 1.5).abs() < 1e-9);
        assert!(some.cached_input.is_none());
        assert!(
            (some.cached_input() - 1.5).abs() < 1e-9,
            "unset cached_input bills at input rate"
        );
        assert!((pie.pricing["other-model"].cached_input() - 0.1).abs() < 1e-9);
    }

    #[test]
    fn resolve_secrets_replaces_exact_matches_only() {
        let mut server = McpServerConfig {
            url: "https://mcp.example.com/mcp".parse().unwrap(),
            headers: HashMap::from([
                ("AUTH".to_string(), Secret::new("ctx_key".to_string())),
                (
                    "X-LITERAL".to_string(),
                    Secret::new("Bearer literal".to_string()),
                ),
            ]),
        };

        let mut secrets = HashMap::new();
        secrets.insert("ctx_key".to_string(), Secret::new("real-key".to_string()));
        server.resolve_secrets(&secrets);

        assert_eq!(server.headers["AUTH"].expose_secret(), "real-key");
        assert_eq!(
            server.headers["X-LITERAL"].expose_secret(),
            "Bearer literal"
        );
    }
}
