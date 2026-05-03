use crate::db::DbPool;
use crate::instructions::Instructions;
use crate::prompt::SystemPrompt;
use crate::providers::Model;
use crate::registry::Registry;
use crate::tools::plan::plan_tools;
use crate::tools::{
    execute_skill_script_tool, load_references_tool, load_skills_tool, read_file_tool,
    replace_tool, shell, write_file_tool,
};
use agentsdk::core::tools::{Tool, ToolExecute};
use agentsdk::core::utils::step_count_is;
use agentsdk::core::{LanguageModelRequest, Message, UserMessage};
use p1e_sandbox::SandboxConfig;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const MAX_DEPTH: u32 = 2;

/// Execution context for a single subagent invocation.
#[derive(Clone)]
pub(crate) struct Subagent {
    model: Model,
    registry: Arc<Registry>,
    sandbox_settings: Arc<SandboxConfig>,
    loaded_skills: Arc<Mutex<HashSet<String>>>,
    loaded_refs: Arc<Mutex<HashSet<String>>>,
    pool: Arc<DbPool>,
    session_id: String,
}

impl Subagent {
    pub fn new(
        model: Model,
        registry: Arc<Registry>,
        sandbox_settings: Arc<SandboxConfig>,
        pool: Arc<DbPool>,
        session_id: String,
    ) -> Self {
        Self {
            model,
            registry,
            sandbox_settings,
            loaded_skills: Arc::new(Mutex::new(HashSet::new())),
            loaded_refs: Arc::new(Mutex::new(HashSet::new())),
            pool,
            session_id,
        }
    }

    pub fn load_skills(&self, names: &[&str]) {
        let mut loaded = crate::tools::safe_lock(&self.loaded_skills);
        for name in names {
            loaded.insert(name.to_string());
        }
    }

    fn build_tools(&self, depth: u32) -> anyhow::Result<Vec<Tool>> {
        let mut tools = vec![
            shell(
                self.sandbox_settings.clone(),
                self.pool.clone(),
                self.session_id.clone(),
            ),
            read_file_tool(),
            write_file_tool(self.pool.clone(), self.session_id.clone()),
            replace_tool(self.pool.clone(), self.session_id.clone()),
            load_skills_tool(self.registry.clone(), Some(self.loaded_skills.clone())),
            load_references_tool(self.loaded_refs.clone()),
            execute_skill_script_tool(self.sandbox_settings.clone()),
        ];

        tools.extend(plan_tools(self.pool.clone(), self.session_id.clone())?);

        if depth < MAX_DEPTH {
            tools.push(make_subagent_tool(
                self.model.clone(),
                self.registry.clone(),
                self.sandbox_settings.clone(),
                self.pool.clone(),
            ));
        }
        Ok(tools)
    }

    pub fn build_request(
        &self,
        name: &str,
        query: &str,
        depth: u32,
    ) -> Result<LanguageModelRequest<Model>, String> {
        let is_agent = self.registry.agents.iter().any(|a| a.name == name);
        let is_skill = self.registry.skills.iter().any(|s| s.name == name);
        if !is_agent && !is_skill {
            return Err(format!("'{name}' not found as agent or skill."));
        }

        let mut query_instr = Instructions::new(query);
        if is_skill {
            query_instr.mentions.insert(name.to_string());
        }
        if let Some(agent) = self.registry.agents.iter().find(|a| a.name == name) {
            query_instr.merge_mentions(&agent.content);
        }

        let agent_name = if is_agent { Some(name) } else { None };

        let sp = SystemPrompt::new(&self.registry.skills, &self.registry.agents)
            .with_plan(self.pool.clone(), self.session_id.clone())
            .with_agent(agent_name)
            .resolve(&query_instr);

        self.load_skills(&sp.loaded_skills);
        let sys = sp.render();
        let user_content = format!("Query: {query}");
        let messages = vec![Message::User(UserMessage::new(user_content))];

        let tools = self.build_tools(depth).map_err(|e| e.to_string())?;
        let mut builder = LanguageModelRequest::builder()
            .model(self.model.clone())
            .system(sys)
            .messages(messages);
        for tool in tools {
            builder = builder.with_tool(tool);
        }
        Ok(builder.stop_when(step_count_is(20)).build())
    }

    pub async fn execute(self, name: &str, query: &str, depth: u32) -> Result<String, String> {
        let name_str = name.to_string();
        let query_str = query.to_string();

        let response = crate::utils::execute_with_retry("subagent_execute", move || {
            let subagent = self.clone();
            let name = name_str.clone();
            let query = query_str.clone();

            async move {
                let mut req = subagent
                    .build_request(&name, &query, depth)
                    .map_err(|e| anyhow::anyhow!(e))?;
                req.generate_text().await.map_err(|e| anyhow::anyhow!(e))
            }
        })
        .await
        .map_err(|e| format!("Subagent failed: {e}"))?;

        Ok(response.text().unwrap_or_default())
    }
}

#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct SubagentInput {
    name: String,
    query: String,
}

#[allow(clippy::unwrap_used)]
fn make_subagent_tool(
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
                let sub_id = Uuid::new_v4().to_string();
                let subagent = Subagent::new(model.clone(), registry, sandbox, pool, sub_id);
                subagent.execute(&name, &query, 0).await
            }
        }))
        .build()
        .unwrap()
}

pub fn subagent_tool(
    model: Model,
    registry: Arc<Registry>,
    sandbox: Arc<SandboxConfig>,
    pool: Arc<DbPool>,
) -> Tool {
    make_subagent_tool(model, registry, sandbox, pool)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;
    use crate::skill::Skill;

    fn dummy_model() -> anyhow::Result<Model> {
        Model::test_dummy()
    }

    fn new_subagent(skills: Vec<Skill>, agents: Vec<Agent>) -> anyhow::Result<Subagent> {
        let pool = Arc::new(crate::db::create_test_pool()?);
        Ok(Subagent::new(
            dummy_model()?,
            Arc::new(Registry {
                agents,
                skills,
                completions: Vec::new(),
            }),
            Arc::new(SandboxConfig::default()),
            pool,
            Uuid::now_v7().to_string(),
        ))
    }

    #[test]
    fn execute_rejects_empty_name() -> anyhow::Result<()> {
        let sub = new_subagent(vec![], vec![])?;
        let rt = tokio::runtime::Runtime::new()?;
        let result = rt.block_on(sub.execute("", "query", 0));
        assert!(result.is_err());
        Ok(())
    }

    #[test]
    fn make_subagent_tool_description_is_set() -> anyhow::Result<()> {
        let pool = Arc::new(crate::db::create_test_pool()?);
        let tool = make_subagent_tool(
            dummy_model()?,
            Arc::new(Registry {
                agents: Vec::new(),
                skills: Vec::new(),
                completions: Vec::new(),
            }),
            Arc::new(SandboxConfig::default()),
            pool,
        );
        assert!(
            !tool.description.is_empty(),
            "subagent tool must have a description"
        );
        Ok(())
    }
}
