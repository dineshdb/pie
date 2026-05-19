use crate::agent::{AgentConfig, PieAgent};
use crate::config::CONFIG;
use crate::db::DbPool;
use crate::registry::Registry;
use agentsdk::core::tools::{Tool, ToolDefinition, ToolExecute};
use p1e_sandbox::SandboxConfig;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;

#[derive(JsonSchema, Deserialize, Serialize)]
struct SubagentInput {
    /// The name of the subagent to invoke (e.g., `howto`, `review`).
    pub name: String,
    /// The detailed instructions or goal for the subagent.
    pub query: String,
}

/// Delegate a goal to an subagent with detailed instructions.
pub fn subagent_tool(
    model: agentsdk::OpenAI,
    registry: Arc<Registry>,
    sandbox: Arc<SandboxConfig>,
    pool: Arc<DbPool>,
) -> anyhow::Result<Tool> {
    let schema = schemars::schema_for!(SubagentInput);
    Ok(Tool::builder()
        .definition(
            ToolDefinition::builder()
                .name("subagent")
                .description("Delegate a goal to an subagent with detailed instructions")
                .input_schema(schema)
                .build()?,
        )
        .execute(ToolExecute::from_async(move |_ctx, params| {
            let model = model.clone();
            let registry = registry.clone();
            let sandbox = sandbox.clone();
            let pool = pool.clone();
            async move {
                let input: SubagentInput =
                    serde_json::from_value(params).map_err(|e| e.to_string())?;

                crate::tools::emit_tool_input("subagent", &json!(input));

                let tier = registry
                    .agents
                    .iter()
                    .find(|a| a.name == input.name)
                    .and_then(|a| a.model.as_deref());
                let resolved_model = CONFIG
                    .get()
                    .map_or(model.clone(), |c| c.resolve_model(tier, &model));

                let mut agent = PieAgent::new(
                    resolved_model,
                    registry,
                    sandbox,
                    pool.clone(),
                    crate::session::Session::create(pool)
                        .await
                        .map_err(|e| format!("failed to create session: {e}"))?,
                    AgentConfig::subagent(
                        0,
                        (!input.name.is_empty()).then_some(input.name.clone()),
                    ),
                );

                let res = agent.run(&input.query).await.map_err(|e| e.to_string())?;
                Ok(json!(res))
            }
        }))
        .build()?)
}
