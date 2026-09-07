use crate::agent::{AgentConfig, PieAgent, RunOutcome};
use crate::config::RetryConfig;
use crate::instructions::Instructions;
use crate::session::Session;
use crate::utils::output::{JsonResponse, OutputFormat};
use anyhow::Result;
use p1e_sandbox::SandboxConfig;
use std::sync::Arc;

/// Print response to stdout and persist to session.
fn output_response(
    outcome: &RunOutcome,
    session_id: &str,
    format: &OutputFormat,
    model: &agentsdk::OpenAI,
) -> Result<()> {
    let output = &outcome.text;
    if output.is_empty() {
        return Ok(());
    }
    if format.is_json() {
        let val = serde_json::from_str(output)
            .unwrap_or_else(|_| serde_json::Value::String(output.clone()));
        let json_resp = JsonResponse::new(
            val,
            Some(session_id.to_string()),
            Some(model.config.model.clone()),
            Some(outcome.usage.report(outcome.cost_usd)),
        );
        println!("{}", serde_json::to_string(&json_resp)?);
    } else {
        println!("{output}");
    }
    Ok(())
}

fn parse_schema(spec: Option<&str>) -> Result<serde_json::Value> {
    // TODO: convert terse format to json schema while sending upstream
    let Some(spec) = spec else {
        anyhow::bail!("JSON schema is required for JSON output format");
    };

    if spec.trim().is_empty() {
        anyhow::bail!("JSON schema cannot be empty");
    }

    if let Ok(parsed) = serde_json::from_str(spec) {
        return Ok(parsed);
    }

    // Try reading as a file
    if let Ok(content) = std::fs::read_to_string(spec)
        && let Ok(parsed) = serde_json::from_str(&content)
    {
        return Ok(parsed);
    }

    anyhow::bail!("Failed to parse JSON schema from string or file: {spec}")
}

pub struct HandleParams {
    pub model: agentsdk::OpenAI,
    pub query: Instructions,
    pub session: Session,
    pub format: OutputFormat,
    pub sandbox_settings: Arc<SandboxConfig>,
    pub retry: RetryConfig,
    pub registry: Arc<crate::registry::Registry>,
    pub agent_name: Option<String>,
}

pub async fn handle_query(params: HandleParams) -> Result<()> {
    let config = AgentConfig {
        retry: params.retry,
        agent_name: params.agent_name,
        ..AgentConfig::default()
    };

    let mut agent = PieAgent::new(
        params.model.clone(),
        params.registry,
        params.sandbox_settings,
        params.session,
        config,
    );

    let session_id = agent.session.id.to_string();

    let outcome: RunOutcome = match &params.format {
        OutputFormat::Json(spec) => {
            let schema = parse_schema(spec.as_deref())?;
            agent.run_json(&params.query.raw, schema).await?
        }
        _ => agent.run(&params.query.raw).await?,
    };

    output_response(&outcome, &session_id, &params.format, &params.model)?;
    Ok(())
}
