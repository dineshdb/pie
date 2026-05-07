use crate::agent::{AgentConfig, PieAgent};
use crate::config::CONFIG;
use crate::db::DbPool;
use crate::providers::Model;
use crate::registry::Registry;
use agentsdk::core::tools::{Tool, ToolExecute};
use p1e_sandbox::SandboxConfig;
use std::sync::Arc;

#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct SubagentInput {
    name: String,
    query: String,
}

#[allow(clippy::expect_used)]
pub fn subagent_tool(
    model: Model,
    registry: Arc<Registry>,
    sandbox: Arc<SandboxConfig>,
    pool: Arc<DbPool>,
) -> Tool {
    Tool::builder()
        .name("subagent")
        .description("Delegate a goal to an subagent with detailed instructions.")
        .input_schema(schemars::schema_for!(SubagentInput))
        .execute(ToolExecute::from_async(move |_ctx, params| {
            crate::tools::emit_tool_input("subagent", &params);
            let name = params
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let query = params
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();

            let model = model.clone();
            let registry = registry.clone();
            let sandbox = sandbox.clone();
            let pool = pool.clone();

            async move {
                let tier = registry
                    .agents
                    .iter()
                    .find(|a| a.name == name)
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
                        .expect("failed to create session"),
                    AgentConfig::subagent(
                        0,
                        if name.is_empty() {
                            None
                        } else {
                            Some(name.clone())
                        },
                    ),
                );

                // Subagents combine name and query into one execution.
                // If 'name' is an agent name, it's used as the role.
                let query_with_name = if name.is_empty() {
                    query
                } else {
                    format!("{name} {query}")
                };
                agent.run(&query_with_name).await.map_err(|e| e.to_string())
            }
        }))
        .build()
        .expect("subagent tool schema is valid")
}
