use crate::agent::{AgentConfig, PieAgent};
use crate::config::RetryConfig;
use crate::instructions::Instructions;
use crate::session::Session;
use crate::utils::output::{JsonResponse, OutputFormat};
use anyhow::Result;
use p1e_sandbox::SandboxConfig;
use std::sync::Arc;

/// Print response to stdout and persist to session.
fn output_response(
    output: &str,
    session_id: &str,
    format: OutputFormat,
    model: &agentsdk::OpenAI,
) -> Result<()> {
    if output.is_empty() {
        return Ok(());
    }
    if format.is_json() {
        let json_resp = JsonResponse::new(
            output.to_string(),
            Some(session_id.to_string()),
            Some(model.config.model.clone()),
        );
        println!("{}", serde_json::to_string(&json_resp)?);
    } else {
        println!("{output}");
    }
    Ok(())
}

pub struct HandleParams {
    pub model: agentsdk::OpenAI,
    pub query: Instructions,
    pub session: Session,
    pub format: OutputFormat,
    pub sandbox_settings: Arc<SandboxConfig>,
    pub max_steps: u32,
    pub retry: RetryConfig,
    pub registry: Arc<crate::registry::Registry>,
}

pub async fn handle_query(params: HandleParams) -> Result<()> {
    let config = AgentConfig {
        max_steps: params.max_steps,
        retry: params.retry,
        ..AgentConfig::default()
    };

    let mut agent = PieAgent::new(
        params.model.clone(),
        params.registry,
        params.sandbox_settings,
        params.session.pool.clone(),
        params.session,
        config,
    );

    let session_id = agent.session.id.to_string();
    let output = agent.run(&params.query.raw).await?;
    output_response(&output, &session_id, params.format, &params.model)?;
    Ok(())
}
