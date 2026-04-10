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

/// Resolved model provider (OpenAI-compatible).
#[derive(Debug, Clone)]
pub struct Model {
    inner: OpenAICompatible<DynamicModel>,
}

// Delegate capability marker traits
macro_rules! impl_capability {
    ($($trait:ident),* $(,)?) => { $( impl $trait for Model {} )* }
}
impl_capability!(
    TextInputSupport,
    TextOutputSupport,
    ToolCallSupport,
    StructuredOutputSupport,
    ReasoningSupport,
    ImageInputSupport,
    ImageOutputSupport,
    VideoInputSupport,
    AudioInputSupport,
    AudioOutputSupport,
    VideoOutputSupport,
);

#[async_trait]
impl LanguageModel for Model {
    fn name(&self) -> String {
        self.inner.name()
    }

    async fn generate_text(
        &mut self,
        options: LanguageModelOptions,
    ) -> aisdk::error::Result<LanguageModelResponse> {
        let response = self.inner.generate_text(options).await?;
        Ok(post_process_response(response))
    }

    async fn stream_text(
        &mut self,
        options: LanguageModelOptions,
    ) -> aisdk::error::Result<ProviderStream> {
        self.inner.stream_text(options).await
    }
}

/// Build a model from CLI args + env vars.
///
/// Priority: CLI arg > env var > default.
pub fn build_model(
    model: Option<&str>,
    base_url: Option<&str>,
    api_key: Option<&str>,
) -> Result<Model> {
    let model_name = model
        .map(|s| s.to_string())
        .or_else(|| std::env::var("OPENAI_MODEL").ok())
        .context("model name is required (set --model or OPENAI_MODEL)")?;

    let base_url = base_url
        .map(|s| s.to_string())
        .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
        .or_else(|| std::env::var("OPENAI_API_BASE").ok())
        .or_else(|| ollama_default(&model_name))
        .context("base URL is required (set --base-url, OPENAI_BASE_URL, or OPENAI_API_BASE)")?;

    let api_key = api_key
        .map(|s| s.to_string())
        .or_else(|| std::env::var("OPENAI_API_KEY").ok())
        .or_else(|| local_placeholder(&base_url))
        .context("API key is required (set --api-key or OPENAI_API_KEY)")?;

    let provider = OpenAICompatible::<DynamicModel>::builder()
        .model_name(&model_name)
        .base_url(&base_url)
        .api_key(&api_key)
        .build()
        .context("failed to build OpenAI-compatible provider")?;

    tracing::debug!(model = %model_name, base_url = %base_url, "using OpenAI-compatible provider");

    Ok(Model { inner: provider })
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
