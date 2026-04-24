use super::tool_compat::post_process_response;
use crate::config::ResolvedProvider;
use aisdk::core::DynamicModel;
use aisdk::core::capabilities::{
    AudioInputSupport, AudioOutputSupport, ImageInputSupport, ImageOutputSupport, ReasoningSupport,
    StructuredOutputSupport, TextInputSupport, TextOutputSupport, ToolCallSupport,
    VideoInputSupport, VideoOutputSupport,
};
use aisdk::core::language_model::{
    LanguageModel, LanguageModelOptions, LanguageModelResponse, LanguageModelResponseContentType,
    ProviderStream,
};
use aisdk::providers::OpenAICompatible;
use anyhow::{Context, Result};
use async_trait::async_trait;

/// Resolved model provider (OpenAI-compatible).
#[derive(Debug, Clone)]
pub struct Model {
    inner: OpenAICompatible<DynamicModel>,
}

#[cfg(test)]
impl Model {
    pub fn test_dummy() -> Result<Self> {
        Ok(Self {
            inner: OpenAICompatible::<DynamicModel>::builder()
                .model_name("test")
                .base_url("http://localhost:1")
                .api_key("test")
                .build()
                .map_err(|e| anyhow::anyhow!("failed to build test model: {e}"))?,
        })
    }
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
        for content in &response.contents {
            match content {
                LanguageModelResponseContentType::Text(t) => {
                    tracing::debug!(text = %t, "raw model response text");
                }
                LanguageModelResponseContentType::ToolCall(info) => {
                    tracing::debug!(tool = %info.tool.name, input = ?info.input, "raw model tool call");
                }
                other => {
                    tracing::debug!(?other, "raw model response other");
                }
            }
        }
        Ok(post_process_response(response))
    }

    async fn stream_text(
        &mut self,
        options: LanguageModelOptions,
    ) -> aisdk::error::Result<ProviderStream> {
        self.inner.stream_text(options).await
    }
}

/// Build a model from a fully resolved provider config.
pub fn build_from_resolved(provider: &ResolvedProvider) -> Result<Model> {
    let inner = OpenAICompatible::<DynamicModel>::builder()
        .model_name(&provider.model)
        .base_url(&provider.base_url)
        .api_key(&provider.api_key)
        .build()
        .context("failed to build OpenAI-compatible provider")?;

    tracing::debug!(
        model = %provider.model,
        base_url = %provider.base_url,
        "provider"
    );

    Ok(Model { inner })
}

/// Fetch available models from the provider.
pub async fn fetch_models(provider: &ResolvedProvider) -> Result<Vec<String>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .context("failed to build reqwest client")?;

    let base_url = provider.base_url.trim_end_matches('/');
    let url = if base_url.ends_with("/v1") {
        format!("{base_url}/models")
    } else {
        format!("{base_url}/v1/models")
    };

    let mut request = client.get(&url);
    if !provider.api_key.is_empty() {
        request = request.header("Authorization", format!("Bearer {}", provider.api_key));
    }

    let response = request.send().await.context("failed to fetch models")?;
    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "no error body".to_string());
        anyhow::bail!("API error {status}: {error_text}");
    }

    let data: serde_json::Value = response
        .json()
        .await
        .context("failed to parse models JSON")?;

    let mut models = Vec::new();
    if let Some(data_array) = data.get("data").and_then(|d| d.as_array()) {
        for m in data_array {
            if let Some(id) = m.get("id").and_then(|id| id.as_str()) {
                models.push(id.to_string());
            }
        }
    }

    if models.is_empty() {
        anyhow::bail!("No models found in provider response");
    }

    models.sort();
    Ok(models)
}
