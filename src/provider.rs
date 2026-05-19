use crate::config::ResolvedProvider;
use agentsdk::{ModelConfig, OpenAI};
use anyhow::{Context, Result};
use itertools::Itertools;

/// Build a model from a fully resolved provider config.
pub fn build_from_resolved(provider: &ResolvedProvider) -> OpenAI {
    let config = ModelConfig {
        base_url: provider.openai_url.as_str().to_string(),
        api_key: provider.api_key.expose_secret().clone(),
        model: provider.model.clone(),
    };
    OpenAI::new(config)
}

/// Fetch available models from the provider.
pub async fn fetch_models(provider: &ResolvedProvider) -> Result<Vec<String>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .context("failed to build reqwest client")?;

    let base_url = provider.openai_url.as_str().trim_end_matches('/');
    let url = format!("{base_url}/models");
    let api_key_owned = provider.api_key.clone();

    tracing::info!(url = %url, "fetching models from API");

    let mut request = client.get(&url);
    let secret = api_key_owned.expose_secret();
    if !secret.is_empty() {
        request = request.header("Authorization", format!("Bearer {secret}"));
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
