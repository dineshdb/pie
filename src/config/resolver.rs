use super::loader::get_providers_data;
use super::types::{PieConfig, ProviderBaseUrl, ProviderConfig, ProviderEndpoint};
use crate::Cli;
use crate::error::{AppError, Result};
use crate::utils::output::OutputFormat;
use agentsdk::{ModelConfig, OpenAI};
use itertools::Itertools;
use p1e_sandbox::SandboxConfig;
use redact::Secret;
use std::collections::HashMap;
use std::sync::Arc;
use url::Url;

const ENV_OPENAI_MODEL: &str = "OPENAI_MODEL";
const ENV_OPENAI_BASE_URL: &str = "OPENAI_BASE_URL";
const ENV_OPENAI_API_KEY: &str = "OPENAI_API_KEY";
const ENV_ANTHROPIC_AUTH_TOKEN: &str = "ANTHROPIC_AUTH_TOKEN";
const ENV_ANTHROPIC_BASE_URL: &str = "ANTHROPIC_BASE_URL";
const ENV_ANTHROPIC_MODEL: &str = "ANTHROPIC_MODEL";

#[derive(Debug, Clone, serde::Serialize)]
pub struct ResolvedProvider {
    pub name: String,
    pub model: String,
    pub anthropic_url: Option<Url>,
    pub openai_url: Url,
    #[serde(serialize_with = "redact_api_key")]
    pub api_key: Secret<String>,
    #[allow(dead_code)]
    pub temperature: Option<f32>,
}

fn redact_api_key<S>(_: &Secret<String>, s: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    s.serialize_str("[REDACTED]")
}

fn resolve_url(custom: Option<&String>, known: Option<&String>, fallback: &str) -> Option<Url> {
    if let Some(url) = custom.and_then(|s| Url::parse(s).ok()) {
        return Some(url);
    }
    if let Some(url) = known.and_then(|s| Url::parse(s).ok()) {
        return Some(url);
    }
    if custom.is_none() && known.is_none() {
        return fallback.parse().ok();
    }
    None
}

#[derive(Debug, serde::Serialize)]
pub struct ResolvedConfig {
    pub provider: ResolvedProvider,
    pub model_tiers: HashMap<String, ResolvedProvider>,
    pub max_steps: u32,
    pub retry: super::types::RetryConfig,
    pub output_format: OutputFormat,
    pub log_level: String,
    pub debug: bool,
}

impl ResolvedConfig {
    /// Resolve a model tier name (from agent frontmatter) to a concrete `OpenAI` client.
    /// Falls back to the provided `fallback` if the tier is unset or unresolvable.
    pub fn resolve_model(&self, tier: Option<&str>, fallback: &OpenAI) -> OpenAI {
        let Some(tier_name) = tier else {
            return fallback.clone();
        };
        let Some(provider) = self.model_tiers.get(tier_name) else {
            tracing::warn!("model tier '{tier_name}' not found in config, using default");
            return fallback.clone();
        };
        provider.build_client()
    }
}

impl ResolvedProvider {
    pub fn env_vars(&self) -> HashMap<&'static str, String> {
        let mut env = HashMap::new();
        env.insert(ENV_OPENAI_MODEL, self.model.clone());
        env.insert(ENV_OPENAI_BASE_URL, self.openai_url.to_string());
        env.insert(ENV_OPENAI_API_KEY, self.api_key.expose_secret().clone());

        if let Some(ref url) = self.anthropic_url {
            env.insert(
                ENV_ANTHROPIC_AUTH_TOKEN,
                self.api_key.expose_secret().clone(),
            );
            env.insert(ENV_ANTHROPIC_BASE_URL, url.to_string());
            env.insert(ENV_ANTHROPIC_MODEL, self.model.clone());
        }

        env
    }

    /// Build an `OpenAI` client from this resolved provider.
    pub fn build_client(&self) -> OpenAI {
        let config = ModelConfig {
            base_url: self.openai_url.as_str().to_string(),
            api_key: self.api_key.expose_secret().clone(),
            model: self.model.clone(),
        };
        OpenAI::new(config)
    }

    /// Fetch available models from this provider.
    pub async fn fetch_models(&self) -> Result<Vec<String>> {
        let client = self.build_client();
        let models = client.list_models().await?;

        let models = models.into_iter().sorted().collect::<Vec<_>>();

        if models.is_empty() {
            return Err(AppError::Config(
                "No models found in provider response".into(),
            ));
        }

        Ok(models)
    }

    pub fn resolve(
        provider: ProviderConfig,
        providers_data: &HashMap<String, ProviderBaseUrl>,
    ) -> Result<Self> {
        let (name, known_data, custom_openai, custom_anthropic) =
            if provider.endpoint.openai.is_some() || provider.endpoint.anthropic.is_some() {
                (
                    "custom".to_string(),
                    None,
                    provider.endpoint.openai,
                    provider.endpoint.anthropic,
                )
            } else if let Some(ref n) = provider.endpoint.name {
                if Url::parse(n).is_ok() {
                    ("custom".to_string(), None, Some(n.clone()), None)
                } else {
                    (n.clone(), providers_data.get(n), None, None)
                }
            } else {
                ("default".to_string(), None, None, None)
            };

        let openai_url = resolve_url(
            custom_openai.as_ref(),
            known_data.and_then(|d| d.openai.as_ref()),
            "http://localhost:11434/v1",
        )
        .ok_or_else(|| {
            AppError::Config(format!(
                "provider '{name}' not found or has no valid base URL"
            ))
        })?;

        let anthropic_url = resolve_url(
            custom_anthropic.as_ref(),
            known_data.and_then(|d| d.anthropic.as_ref()),
            "http://localhost:11434",
        );

        Ok(Self {
            name,
            model: provider.model.ok_or_else(|| {
                AppError::Config(
                    "model is required (set --model, OPENAI_MODEL, or config provider)".into(),
                )
            })?,
            openai_url,
            anthropic_url,
            api_key: provider
                .api_key
                .unwrap_or_else(|| Secret::new(String::new())),
            temperature: provider.temperature,
        })
    }
}

impl TryFrom<(Cli, PieConfig)> for ResolvedConfig {
    type Error = AppError;

    fn try_from((mut cli, pie): (Cli, PieConfig)) -> Result<Self, Self::Error> {
        let providers_data = get_providers_data()?;

        let mut smart_provider_name = None;
        let mut tier_model_override = None;

        // Smart selection: if --provider is not explicitly set,
        // check if -m matches a provider or tier name.
        if cli.provider.is_none()
            && let Some(model_name) = cli.provider_config.model.as_deref()
        {
            if let Some(tier) = pie.model.get(model_name) {
                smart_provider_name = Some(tier.provider.clone());
                tier_model_override.clone_from(&tier.model);
                cli.provider_config.model = None;
            } else if pie.provider.contains_key(model_name) {
                smart_provider_name = Some(model_name.to_string());
                cli.provider_config.model = None;
            }
        }

        let provider_name = cli
            .provider
            .clone()
            .or(smart_provider_name)
            .or(pie.default_provider.clone());

        let mut provider_cfg = if let Some(ref name) = provider_name {
            pie.provider
                .get(name)
                .cloned()
                .ok_or_else(|| AppError::Config(format!("provider '{name}' not found in config")))?
        } else {
            let openai_env = std::env::var(ENV_OPENAI_BASE_URL).ok();
            let anthropic_env = std::env::var(ENV_ANTHROPIC_BASE_URL).ok();

            let mut endpoint = ProviderEndpoint::default();
            if let Some(val) = openai_env {
                if Url::parse(&val).is_ok() {
                    endpoint.openai = Some(val);
                    endpoint.anthropic = anthropic_env;
                } else {
                    endpoint.name = Some(val);
                }
            } else if let Some(val) = anthropic_env {
                endpoint.anthropic = Some(val);
            }

            ProviderConfig {
                model: std::env::var(ENV_OPENAI_MODEL).ok(),
                endpoint,
                api_key: std::env::var(ENV_OPENAI_API_KEY).ok().map(Secret::new),
                temperature: None,
            }
        };

        if let Some(m) = tier_model_override {
            provider_cfg.model = Some(m);
        }

        let output_format = match cli.output_format() {
            OutputFormat::Default => pie.output_format(),
            format => format,
        };

        let provider = provider_cfg.merge(cli.provider_config);
        let mut resolved_provider = ResolvedProvider::resolve(provider, providers_data)?;
        if let Some(name) = provider_name {
            resolved_provider.name.clone_from(&name);
        }

        let model_tiers = resolve_model_tiers(&pie, providers_data);

        let log_level = if cli.debug { "debug" } else { pie.log_level() }.to_string();

        let retry = pie
            .agent
            .as_ref()
            .map(|a| a.retry.clone())
            .unwrap_or_default();
        Ok(Self {
            provider: resolved_provider,
            model_tiers,
            max_steps: pie.agent.as_ref().and_then(|a| a.max_steps).unwrap_or(25),
            retry,
            output_format,
            log_level,
            debug: cli.debug,
        })
    }
}

/// Resolve `[model.<name>]` tiers into `ResolvedProvider`s by looking up
/// each tier's `provider` field in the `[provider.*]` map.
fn resolve_model_tiers(
    pie: &PieConfig,
    data: &HashMap<String, ProviderBaseUrl>,
) -> HashMap<String, ResolvedProvider> {
    let mut tiers = HashMap::new();
    for (name, tier) in &pie.model {
        let Some(provider_cfg) = pie.provider.get(&tier.provider) else {
            tracing::warn!(
                "model tier '{name}' references unknown provider '{}'",
                tier.provider
            );
            continue;
        };
        let mut cfg = provider_cfg.clone();
        if let Some(ref model_override) = tier.model {
            cfg.model = Some(model_override.clone());
        }
        match ResolvedProvider::resolve(cfg, data) {
            Ok(mut resolved) => {
                resolved.name.clone_from(name);
                tiers.insert(name.clone(), resolved);
            }
            Err(e) => {
                tracing::warn!("model tier '{name}' has invalid config: {e}");
            }
        }
    }
    tiers
}

pub fn build_sandbox(pie_config: &PieConfig) -> Arc<SandboxConfig> {
    let sandbox = pie_config.sandbox.clone().unwrap_or_default();
    if let Err(warnings) = sandbox.validate() {
        for w in &warnings {
            tracing::warn!("sandbox config: {w}");
        }
    }
    Arc::new(sandbox)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{ModelTier, ProviderEndpoint};
    use clap::Parser;

    #[test]
    fn test_resolve_config_smart_model_selection() {
        let mut pie = PieConfig {
            default_provider: Some("openai".to_string()),
            provider: HashMap::new(),
            secrets: HashMap::new(),
            model: HashMap::new(),
            agent: None,
            sandbox: None,
            output_format: None,
            log_level: None,
        };

        pie.provider.insert(
            "openai".to_string(),
            ProviderConfig {
                model: Some("gpt-4o".to_string()),
                endpoint: ProviderEndpoint {
                    name: Some("openai".to_string()),
                    ..Default::default()
                },
                ..Default::default()
            },
        );

        pie.provider.insert(
            "codestral".to_string(),
            ProviderConfig {
                model: Some("codestral-latest".to_string()),
                endpoint: ProviderEndpoint {
                    openai: Some("https://api.mistral.ai/v1".to_string()),
                    ..Default::default()
                },
                ..Default::default()
            },
        );

        // Case 1: No -p, no -m -> should use default_provider (openai)
        let cli = Cli::parse_from(["pie"]);
        let config = ResolvedConfig::try_from((cli, pie.clone())).unwrap();
        assert_eq!(config.provider.name, "openai");
        assert_eq!(config.provider.model, "gpt-4o");

        // Case 2: No -p, -m matches a provider name
        let cli = Cli::parse_from(["pie", "-m", "codestral"]);
        let config = ResolvedConfig::try_from((cli, pie.clone())).unwrap();
        assert_eq!(config.provider.name, "codestral");
        assert_eq!(config.provider.model, "codestral-latest");

        // Case 3: No -p, -m matches a tier name
        pie.model.insert(
            "fast".to_string(),
            ModelTier {
                provider: "openai".to_string(),
                model: Some("gpt-4o-mini".to_string()),
            },
        );
        let cli = Cli::parse_from(["pie", "-m", "fast"]);
        let config = ResolvedConfig::try_from((cli, pie.clone())).unwrap();
        assert_eq!(config.provider.name, "openai");
        assert_eq!(config.provider.model, "gpt-4o-mini");

        // Case 4: No -p, -m matches a tier name with no model override
        pie.model.insert(
            "slow".to_string(),
            ModelTier {
                provider: "openai".to_string(),
                model: None,
            },
        );
        let cli = Cli::parse_from(["pie", "-m", "slow"]);
        let config = ResolvedConfig::try_from((cli, pie.clone())).unwrap();
        assert_eq!(config.provider.name, "openai");
        assert_eq!(config.provider.model, "gpt-4o"); // Uses provider's default

        // Case 5: -p explicitly set, -m matches another provider name -> should NOT switch provider
        let cli = Cli::parse_from(["pie", "-p", "openai", "-m", "codestral"]);
        let config = ResolvedConfig::try_from((cli, pie.clone())).unwrap();
        assert_eq!(config.provider.name, "openai");
        assert_eq!(config.provider.model, "codestral");
    }
}
