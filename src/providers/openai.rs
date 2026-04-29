use super::tool_compat::post_process_response;
use crate::config::ResolvedProvider;
use crate::utils::execute_with_retry;
use agentsdk::core::DynamicModel;
use agentsdk::core::capabilities::{
    AudioInputSupport, AudioOutputSupport, ImageInputSupport, ImageOutputSupport, ReasoningSupport,
    StructuredOutputSupport, TextInputSupport, TextOutputSupport, ToolCallSupport,
    VideoInputSupport, VideoOutputSupport,
};
use agentsdk::core::language_model::{
    LanguageModel, LanguageModelOptions, LanguageModelResponse, LanguageModelResponseContentType,
    ProviderStream,
};
use agentsdk::providers::OpenAICompatible;
use anyhow::{Context, Result};
use async_trait::async_trait;
use itertools::Itertools;

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
    ) -> agentsdk::error::Result<LanguageModelResponse> {
        let response = self.inner.generate_text(options).await?;
        for content in &response.contents {
            match content {
                LanguageModelResponseContentType::Text(t) => {
                    tracing::trace!(text = %t, "raw model response text");
                }
                LanguageModelResponseContentType::ToolCall(info) => {
                    tracing::trace!(tool = %info.tool.name, input = ?info.input, "raw model tool call");
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
    ) -> agentsdk::error::Result<ProviderStream> {
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
    Ok(Model { inner })
}

/// Fetch available models from the provider.
pub async fn fetch_models(provider: &ResolvedProvider) -> Result<Vec<String>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .context("failed to build reqwest client")?;

    let base_url = provider.base_url.trim_end_matches('/');
    let url = format!("{base_url}/models");
    let api_key_owned = provider.api_key.clone();

    tracing::info!(url = %url, "fetching models from API");
    let response = execute_with_retry("fetch_models", move || {
        let client = client.clone();
        let url = url.clone();
        let api_key = api_key_owned.clone();

        async move {
            let mut request = client.get(&url);
            if !api_key.is_empty() {
                request = request.header("Authorization", format!("Bearer {api_key}"));
            }

            let res = request.send().await;
            match res {
                Ok(r) if r.status().is_success() => Ok(r),
                Ok(r) if crate::utils::is_retriable_status(r.status().as_u16()) => {
                    Err(anyhow::anyhow!("API error {}", r.status()))
                }
                Ok(r) => {
                    let status = r.status();
                    let error_text = r
                        .text()
                        .await
                        .unwrap_or_else(|_| "no error body".to_string());
                    Err(anyhow::anyhow!("API error {status}: {error_text}"))
                }
                Err(e) => Err(anyhow::anyhow!(e).context("failed to fetch models")),
            }
        }
    })
    .await?;

    let data: serde_json::Value = response
        .json()
        .await
        .context("failed to parse models JSON")?;

    let models = data
        .get("data")
        .and_then(|d| d.as_array())
        .map(|data_array| {
            data_array
                .iter()
                .filter_map(|m| m.get("id").and_then(|id| id.as_str()))
                .map(String::from)
                .sorted()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if models.is_empty() {
        anyhow::bail!("No models found in provider response");
    }

    Ok(models)
}
