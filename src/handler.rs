use crate::agent::{AgentConfig, PieAgent};
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

pub async fn handle_query(
    model: agentsdk::OpenAI,
    query: &Instructions,
    session: Session,
    format: OutputFormat,
    sandbox_settings: Arc<SandboxConfig>,
    max_steps: u32,
    registry: Arc<crate::registry::Registry>,
) -> Result<()> {
    let config = AgentConfig {
        max_steps,
        ..AgentConfig::default()
    };

    let mut agent = PieAgent::new(
        model.clone(),
        registry,
        sandbox_settings,
        session.pool.clone(),
        session,
        config,
    );

    let session_id = agent.session.id.to_string();
    let output = agent.run(&query.raw).await?;
    output_response(&output, &session_id, format, &model)?;
    Ok(())
}
