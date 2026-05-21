use super::loader::get_providers_data;
use super::types::{PieConfig, ProviderBaseUrl, ProviderConfig, ProviderEndpoint};
use crate::Cli;
use crate::error::{AppError, Result};
use crate::hook::CommandHook;
use crate::plugin::StaticPlugin;
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

pub struct ResolvedConfig {
    pub provider: ResolvedProvider,
    pub model_tiers: HashMap<String, ResolvedProvider>,
    pub max_steps: u32,
    pub retry: super::types::RetryConfig,
    pub output_format: OutputFormat,
    pub log_level: String,
    pub debug: bool,
    pub plugins: Vec<StaticPlugin>,
}

impl std::fmt::Debug for ResolvedConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = f.debug_struct("ResolvedConfig");
        s.field("provider", &self.provider);
        if !self.model_tiers.is_empty() {
            s.field("model_tiers", &self.model_tiers);
        }
        if self.max_steps != 25 {
            s.field("max_steps", &self.max_steps);
        }
        let default_retry = super::types::RetryConfig::default();
        if self.retry.rate_limit != default_retry.rate_limit {
            s.field("retry.rate_limit", &self.retry.rate_limit);
        }
        if self.retry.api_error != default_retry.api_error {
            s.field("retry.api_error", &self.retry.api_error);
        }
        s.field("output_format", &self.output_format);
        s.field("log_level", &self.log_level);
        if self.debug {
            s.field("debug", &self.debug);
        }
        s.field("plugins", &self.plugins);

        s.finish()
    }
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

#[derive(Clone)]
pub struct ResolvedProvider {
    pub name: String,
    pub model: String,
    pub anthropic_url: Option<Url>,
    pub openai_url: Url,
    pub api_key: Secret<String>,
    #[allow(dead_code)]
    pub temperature: Option<f32>,
}

impl serde::Serialize for ResolvedProvider {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("ResolvedProvider", 6)?;
        state.serialize_field("name", &self.name)?;
        state.serialize_field("model", &self.model)?;
        state.serialize_field("anthropic_url", &self.anthropic_url)?;
        state.serialize_field("openai_url", &self.openai_url)?;
        state.serialize_field("api_key", "[REDACTED]")?;
        state.serialize_field("temperature", &self.temperature)?;
        state.end()
    }
}

impl std::fmt::Debug for ResolvedProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = f.debug_struct("ResolvedProvider");
        s.field("name", &self.name);
        s.field("model", &self.model);
        if let Some(ref url) = self.anthropic_url {
            s.field("anthropic_url", &url.to_string());
        }
        s.field("openai_url", &self.openai_url.to_string());
        s.field("api_key", &"[REDACTED]".to_string());
        if let Some(t) = self.temperature {
            s.field("temperature", &t);
        }
        s.finish()
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

        let openai_url = custom_openai
            .as_ref()
            .and_then(|s| Url::parse(s).ok())
            .or_else(|| known_data.and_then(|d| d.openai.as_ref().and_then(|s| Url::parse(s).ok())))
            .or_else(|| {
                if custom_openai.is_none() && known_data.is_none() {
                    "http://localhost:11434/v1".parse().ok()
                } else {
                    None
                }
            })
            .ok_or_else(|| {
                AppError::Config(format!(
                    "provider '{name}' not found or has no valid base URL"
                ))
            })?;

        let anthropic_url = custom_anthropic
            .as_ref()
            .and_then(|s| Url::parse(s).ok())
            .or_else(|| {
                known_data.and_then(|d| d.anthropic.as_ref().and_then(|s| Url::parse(s).ok()))
            })
            .or_else(|| {
                if custom_anthropic.is_none() && known_data.is_none() {
                    "http://localhost:11434".parse().ok()
                } else {
                    None
                }
            });

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

    fn try_from((cli, pie): (Cli, PieConfig)) -> Result<Self, Self::Error> {
        let providers_data = get_providers_data()?;
        let provider_name = cli.provider.as_deref().or(pie.default_provider.as_deref());

        let provider_cfg = if let Some(name) = provider_name {
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

        let output_format = match cli.output_format() {
            OutputFormat::Default => pie.output_format(),
            format => format,
        };

        let provider = provider_cfg.merge(cli.provider_config);
        let mut resolved_provider = ResolvedProvider::resolve(provider, providers_data)?;
        if let Some(name) = provider_name {
            resolved_provider.name = name.to_string();
        }

        let model_tiers = resolve_model_tiers(&pie, providers_data);

        let log_level = if cli.debug { "debug" } else { pie.log_level() }.to_string();

        let (mut plugins, _) = crate::plugin::scan_plugins();

        // Wrap pie.toml hooks in a static plugin
        if !pie.hooks.is_empty() {
            let hooks: Vec<CommandHook> = pie.hooks.into_iter().map(CommandHook::from).collect();
            plugins.push(StaticPlugin {
                name: "config".to_string(),
                hooks,
            });
        }

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
            plugins,
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
