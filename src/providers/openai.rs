use super::apple::AppleClient;
use super::tool_compat::post_process_response;
use aisdk::core::DynamicModel;
use aisdk::core::capabilities::{
    AudioInputSupport, AudioOutputSupport, ImageInputSupport, ImageOutputSupport, ReasoningSupport,
    StructuredOutputSupport, TextInputSupport, TextOutputSupport, ToolCallSupport,
    VideoInputSupport, VideoOutputSupport,
};
use aisdk::core::language_model::{
    LanguageModel, LanguageModelOptions, LanguageModelResponse, ProviderStream,
};
use aisdk::providers::OpenAICompatible;
use anyhow::{Context, Result};
use async_trait::async_trait;

/// Resolved model provider — both variants implement `LanguageModel`.
#[derive(Debug, Clone)]
pub enum Model {
    Apple(AppleClient),
    OpenAI(OpenAICompatible<DynamicModel>),
}

// Delegate all capability marker traits
impl TextInputSupport for Model {}
impl TextOutputSupport for Model {}
impl ToolCallSupport for Model {}
impl StructuredOutputSupport for Model {}
impl ReasoningSupport for Model {}
impl ImageInputSupport for Model {}
impl ImageOutputSupport for Model {}
impl VideoInputSupport for Model {}
impl AudioInputSupport for Model {}
impl AudioOutputSupport for Model {}
impl VideoOutputSupport for Model {}

#[async_trait]
impl LanguageModel for Model {
    fn name(&self) -> String {
        match self {
            Model::Apple(c) => c.name(),
            Model::OpenAI(p) => <OpenAICompatible<DynamicModel> as LanguageModel>::name(p),
        }
    }

    async fn generate_text(
        &mut self,
        options: LanguageModelOptions,
    ) -> aisdk::error::Result<LanguageModelResponse> {
        let response = match self {
            Model::Apple(c) => c.generate_text(options).await?,
            Model::OpenAI(p) => p.generate_text(options).await?,
        };
        Ok(post_process_response(response))
    }

    async fn stream_text(
        &mut self,
        options: LanguageModelOptions,
    ) -> aisdk::error::Result<ProviderStream> {
        match self {
            Model::Apple(c) => c.stream_text(options).await,
            Model::OpenAI(p) => p.stream_text(options).await,
        }
    }
}

/// Build a model from CLI args + env vars.
///
/// Priority: CLI arg > `PIE_*` env > provider-specific env > default.
pub fn build_model(
    model: Option<&str>,
    base_url: Option<&str>,
    api_key: Option<&str>,
) -> Result<Model> {
    let model_name = model
        .map(|s| s.to_string())
        .or_else(|| std::env::var("PIE_MODEL").ok())
        .or_else(|| std::env::var("OPENAI_MODEL").ok());

    // If no model specified, try Apple
    if model_name.is_none() {
        match AppleClient::new() {
            Ok(client) => {
                tracing::debug!("using Apple Foundation Models");
                return Ok(Model::Apple(client));
            }
            Err(e) => {
                anyhow::bail!(
                    "No model specified and Apple Intelligence unavailable: {e}\n\
                     Set --model or PIE_MODEL to use an OpenAI-compatible provider."
                );
            }
        }
    }

    let model_name = model_name.unwrap();

    // Resolve base URL
    let base_url = base_url
        .map(|s| s.to_string())
        .or_else(|| std::env::var("PIE_BASE_URL").ok())
        .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
        .or_else(|| std::env::var("OPENAI_API_BASE").ok())
        .or_else(|| ollama_default(&model_name))
        .context("base URL is required (set --base-url, PIE_BASE_URL, or OPENAI_BASE_URL)")?;

    // Resolve API key
    let api_key = api_key
        .map(|s| s.to_string())
        .or_else(|| std::env::var("PIE_API_KEY").ok())
        .or_else(|| std::env::var("OPENAI_API_KEY").ok())
        .or_else(|| local_placeholder(&base_url))
        .context("API key is required (set --api-key, PIE_API_KEY, or OPENAI_API_KEY)")?;

    let provider = OpenAICompatible::<DynamicModel>::builder()
        .model_name(&model_name)
        .base_url(&base_url)
        .api_key(&api_key)
        .build()
        .context("failed to build OpenAI-compatible provider")?;

    tracing::debug!(model = %model_name, base_url = %base_url, "using OpenAI-compatible provider");

    Ok(Model::OpenAI(provider))
}

/// Well-known local model prefixes that default to Ollama.
fn ollama_default(model: &str) -> Option<String> {
    const LOCAL_PREFIXES: &[&str] = &[
        "llama",
        "mistral",
        "phi",
        "codellama",
        "qwen",
        "deepseek",
        "gemma",
    ];
    if LOCAL_PREFIXES.iter().any(|p| model.starts_with(p)) {
        Some("http://localhost:11434/v1".to_string())
    } else {
        None
    }
}

/// Localhost servers don't need a real key — use a placeholder.
fn local_placeholder(base_url: &str) -> Option<String> {
    if base_url.contains("localhost") || base_url.contains("127.0.0.1") {
        Some("ollama".to_string())
    } else {
        None
    }
}
