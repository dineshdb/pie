use crate::agent::{AgentConfig, PieAgent};
use crate::config::CONFIG;
use crate::db::DbPool;
use crate::registry::{CompletionKind, Registry};
use crate::session::Session;
use agentsdk::core::plugin::PluginToolCall;
use agentsdk::core::tools::ToolDefinition;
use agentsdk::{AgentPlugin, Messages, PluginContext};
use async_trait::async_trait;
use p1e_sandbox::SandboxConfig;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::borrow::Cow;
use std::sync::Arc;

#[derive(Debug)]
pub struct SubAgentPlugin {
    model: agentsdk::OpenAI,
    registry: Arc<Registry>,
    sandbox: Arc<SandboxConfig>,
    pool: Arc<DbPool>,
}

impl SubAgentPlugin {
    pub fn new(
        model: agentsdk::OpenAI,
        registry: Arc<Registry>,
        sandbox: Arc<SandboxConfig>,
        pool: Arc<DbPool>,
    ) -> Self {
        Self {
            model,
            registry,
            sandbox,
            pool,
        }
    }
}

#[async_trait]
impl AgentPlugin for SubAgentPlugin {
    fn name(&self) -> &'static str {
        "subagent"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "subagent".into(),
            description: "Delegate a goal to a subagent with detailed instructions".into(),
            input_schema: schemars::schema_for!(SubagentInput),
        }]
    }

    async fn run_tool(
        &mut self,
        _ctx: &mut PluginContext,
        call: &PluginToolCall,
    ) -> Result<Value, String> {
        match call.name.as_str() {
            "subagent" => {
                let input: SubagentInput =
                    serde_json::from_value(call.arguments.clone()).map_err(|e| e.to_string())?;
                crate::tools::emit_tool_input("subagent", &json!(input));
                self.do_subagent(input).await
            }
            _ => Err(format!("Unknown subagent tool: {}", call.name)),
        }
    }

    async fn prepare_system_prompt(
        &mut self,
        _ctx: &PluginContext,
        _history: &Messages,
    ) -> Option<Cow<'static, str>> {
        let mut agents = Vec::new();

        for item in &self.registry.completions {
            if matches!(item.kind, CompletionKind::Agent) {
                agents.push(format!("- [a] {}: {}", item.label, item.description));
            }
        }

        if agents.is_empty() {
            return None;
        }

        let content = format!("{AGENTS_SECTION}\n{}", agents.join("\n"));
        Some(Cow::Owned(content))
    }
}

impl SubAgentPlugin {
    async fn do_subagent(&self, input: SubagentInput) -> Result<Value, String> {
        let tier = self
            .registry
            .agents
            .iter()
            .find(|a| a.name == input.name)
            .and_then(|a| a.model.as_deref());
        let resolved_model = CONFIG
            .get()
            .map_or(self.model.clone(), |c| c.resolve_model(tier, &self.model));

        let mut agent = PieAgent::new(
            resolved_model,
            self.registry.clone(),
            self.sandbox.clone(),
            self.pool.clone(),
            Session::create(self.pool.clone())
                .await
                .map_err(|e| format!("failed to create session: {e}"))?,
            AgentConfig::subagent(0, (!input.name.is_empty()).then_some(input.name.clone())),
        );

        let res = agent.run(&input.query).await.map_err(|e| e.to_string())?;
        Ok(json!(res))
    }
}

#[derive(JsonSchema, Deserialize, Serialize)]
struct SubagentInput {
    /// The name of the subagent to invoke (e.g., `howto`, `review`).
    pub name: String,
    /// The detailed instructions or goal for the subagent.
    pub query: String,
}

const AGENTS_SECTION: &str = r"
## Agents
Agents are specialized personas you can delegate to using `subagent` tool.
Agents have their own context and provide only the response you want, keeping your context lean.
";
