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
pub fn subagent_tool() -> anyhow::Result<Tool> {
    let schema = schemars::schema_for!(SubagentInput);
    Ok(Tool::builder()
        .definition(
            ToolDefinition::builder()
                .name("subagent")
                .description("Delegate a goal to an subagent with detailed instructions")
                .input_schema(schema)
                .build()?,
        )
        .execute(ToolExecute::from_async(|ctx, params| async move {
            let input: SubagentInput = serde_json::from_value(params).map_err(|e| e.to_string())?;

            let model = ctx
                .options
                .extensions
                .get::<agentsdk::OpenAI>()
                .ok_or_else(|| "Model not found in extensions".to_string())?;
            let registry = ctx
                .options
                .extensions
                .get::<Arc<Registry>>()
                .ok_or_else(|| "Registry not found in extensions".to_string())?;
            let sandbox = ctx
                .options
                .extensions
                .get::<Arc<SandboxConfig>>()
                .ok_or_else(|| "SandboxConfig not found in extensions".to_string())?;
            let pool = ctx
                .options
                .extensions
                .get::<Arc<DbPool>>()
                .ok_or_else(|| "DbPool not found in extensions".to_string())?;

            crate::tools::emit_tool_input("subagent", &json!(input));

            let tier = registry
                .agents
                .iter()
                .find(|a| a.name == input.name)
                .and_then(|a| a.model.as_deref());
            let resolved_model = CONFIG
                .get()
                .map_or((*model).clone(), |c| c.resolve_model(tier, &model));

            let mut agent = PieAgent::new(
                resolved_model,
                (*registry).clone(),
                (*sandbox).clone(),
                (*pool).clone(),
                crate::session::Session::create((*pool).clone())
                    .await
                    .map_err(|e| format!("failed to create session: {e}"))?,
                AgentConfig::subagent(
                    0,
                    if input.name.is_empty() {
                        None
                    } else {
                        Some(input.name.clone())
                    },
                ),
            );

            let res = agent.run(&input.query).await.map_err(|e| e.to_string())?;
            Ok(json!(res))
        }))
        .build()?)
}
