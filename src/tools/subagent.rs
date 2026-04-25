use crate::instructions::Instructions;
use crate::prompt::SystemPrompt;
use crate::providers::Model;
use crate::registry::Registry;
use crate::tools::{
    execute_skill_script_tool, load_references_tool, load_skills_tool, read_file_tool,
    replace_tool, shell, write_file_tool,
};
use aisdk::core::tools::{Tool, ToolExecute};
use aisdk::core::utils::step_count_is;
use aisdk::core::{LanguageModelRequest, Message, UserMessage};
use p1e_srt::SandboxConfig;
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
}

impl Subagent {
    pub fn new(
        model: Model,
        registry: Arc<Registry>,
        sandbox_settings: Arc<SandboxConfig>,
    ) -> Self {
        Self {
            model,
            registry,
            sandbox_settings,
            loaded_skills: Arc::new(Mutex::new(HashSet::new())),
            loaded_refs: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    pub fn load_skills(&self, names: &[&str]) {
        let mut loaded = crate::tools::safe_lock(&self.loaded_skills);
        for name in names {
            loaded.insert(name.to_string());
        }
    }

    fn build_tools(&self, depth: u32, parent_id: Option<Uuid>) -> Vec<Tool> {
        let mut tools = vec![
            shell(self.sandbox_settings.clone()),
            read_file_tool(),
            write_file_tool(),
            replace_tool(),
            load_skills_tool(self.registry.clone(), Some(self.loaded_skills.clone())),
            load_references_tool(self.loaded_refs.clone()),
            execute_skill_script_tool(self.sandbox_settings.clone()),
        ];
        if depth < MAX_DEPTH {
            tools.push(make_subagent_tool(
                self.model.clone(),
                self.registry.clone(),
                self.sandbox_settings.clone(),
                parent_id,
                depth + 1,
            ));
        }
        tools
    }

    pub fn build_request(
        &self,
        name: &str,
        query: &str,
        depth: u32,
        parent_id: Option<Uuid>,
    ) -> Result<LanguageModelRequest<Model>, String> {
        if name.is_empty() || query.is_empty() {
            return Err("name and query are required".to_string());
        }
        let is_agent = self.registry.agents.iter().any(|a| a.name == name);
        let is_skill = self.registry.skills.iter().any(|s| s.name == name);
        if !is_agent && !is_skill {
            return Err(format!("'{name}' not found as agent or skill."));
        }

        // Build the "needed tree" of skills and agents from current query + subagent name.
        let mut query = Instructions::new(query);
        if is_skill {
            query.mentions.insert(name.to_string());
        }
        if let Some(agent) = self.registry.agents.iter().find(|a| a.name == name) {
            query.merge_mentions(&agent.content);
        }

        let agent_name = if is_agent { Some(name) } else { None };

        let sp = SystemPrompt::new(&self.registry.skills, &self.registry.agents)
            .with_agent(agent_name)
            .resolve(&query);

        self.load_skills(&sp.loaded_skills);
        let sys = sp.render();
        let user_content = format!("Query: {query}");
        let messages = vec![Message::User(UserMessage::new(user_content))];

        tracing::debug!(name, query = %query.raw, %sys, "subagent request");
        let tools = self.build_tools(depth, parent_id);
        let mut builder = LanguageModelRequest::builder()
            .model(self.model.clone())
            .system(sys)
            .messages(messages);
        for tool in tools {
            builder = builder.with_tool(tool);
        }
        Ok(builder.stop_when(step_count_is(20)).build())
    }

    #[allow(clippy::unused_async)]
    pub async fn execute(
        self,
        name: &str,
        query: &str,
        depth: u32,
        parent_id: Option<Uuid>,
    ) -> Result<String, String> {
        let name_str = name.to_string();
        let query_str = query.to_string();

        let response = crate::utils::execute_with_retry("subagent_execute", move || {
            let subagent = self.clone();
            let name = name_str.clone();
            let query = query_str.clone();

            async move {
                let mut req = subagent
                    .build_request(&name, &query, depth, parent_id)
                    .map_err(|e| anyhow::anyhow!(e))?;
                req.generate_text().await.map_err(|e| anyhow::anyhow!(e))
            }
        })
        .await
        .map_err(|e| format!("Subagent failed: {e}"))?;

        let text = response.text().unwrap_or_default();
        tracing::debug!(name, len = text.len(), %text, "subagent done");
        Ok(text)
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
    sandbox_settings: Arc<SandboxConfig>,
    parent_id: Option<Uuid>,
    depth: u32,
) -> Tool {
    Tool::builder()
        .name("subagent")
        .description("Delegate a task to an subagent with detailed instructions.")
        .input_schema(schemars::schema_for!(SubagentInput))
        .execute(ToolExecute::from_async(move |_ctx, params| {
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
            let subagent = Subagent::new(model.clone(), registry.clone(), sandbox_settings.clone());
            async move { subagent.execute(&name, &query, depth, parent_id).await }
        }))
        .build()
        .unwrap()
}

/// Public entry point: create the `subagent` tool for the main agent.
pub fn subagent_tool(
    model: Model,
    registry: Arc<Registry>,
    sandbox_settings: Arc<SandboxConfig>,
) -> Tool {
    make_subagent_tool(model, registry, sandbox_settings, None, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Agent, Interactivity};
    use crate::skill::Skill;

    fn skill(name: &str, desc: &str, content: &str) -> Skill {
        Skill {
            name: name.to_string(),
            description: desc.to_string(),
            content: content.to_string(),
            needs: Vec::new(),
        }
    }

    fn agent(name: &str, desc: &str, content: &str) -> Agent {
        Agent {
            name: name.to_string(),
            description: desc.to_string(),
            interactivity: Interactivity::None,
            model: None,
            temperature: None,
            content: content.to_string(),
        }
    }

    fn dummy_model() -> anyhow::Result<Model> {
        Model::test_dummy()
    }

    fn new_subagent(skills: Vec<Skill>, agents: Vec<Agent>) -> anyhow::Result<Subagent> {
        Ok(Subagent::new(
            dummy_model()?,
            Arc::new(Registry {
                agents,
                skills,
                completions: Vec::new(),
            }),
            Arc::new(SandboxConfig::default()),
        ))
    }

    // ── Validation ──────────────────────────────────────────────────

    #[test]
    fn execute_rejects_empty_name() -> anyhow::Result<()> {
        let sub = new_subagent(vec![], vec![])?;
        let rt = tokio::runtime::Runtime::new()?;
        let result = rt.block_on(sub.execute("", "query", 0, None));
        assert!(result.is_err());
        let Err(e) = result else {
            anyhow::bail!("expected error")
        };
        assert!(e.contains("required"));
        Ok(())
    }

    #[test]
    fn execute_rejects_unknown_name() -> anyhow::Result<()> {
        let sub = new_subagent(
            vec![skill("bash", "commands", "content")],
            vec![agent("explore", "explorer", "content")],
        )?;
        let rt = tokio::runtime::Runtime::new()?;
        let result = rt.block_on(sub.execute("nonexistent", "query", 0, None));
        assert!(result.is_err());
        let Err(e) = result else {
            anyhow::bail!("expected error")
        };
        assert!(e.contains("not found"));
        Ok(())
    }

    #[test]
    fn execute_accepts_skill_name() -> anyhow::Result<()> {
        // Can't actually execute (no model server), but we can verify validation passes
        // by checking the error isn't "not found"
        let sub = new_subagent(vec![skill("bash", "commands", "content")], vec![])?;
        let rt = tokio::runtime::Runtime::new()?;
        let result = rt.block_on(sub.execute("bash", "run ls", 0, None));
        // Will fail because there's no model server, but NOT with "not found"
        assert!(result.is_err());
        let Err(err) = result else {
            anyhow::bail!("expected error")
        };
        assert!(
            !err.contains("not found"),
            "should not say 'not found': {err}"
        );
        Ok(())
    }

    #[test]
    fn execute_accepts_agent_name() -> anyhow::Result<()> {
        let sub = new_subagent(
            vec![],
            vec![agent("explore", "explorer", "explore content")],
        )?;
        let rt = tokio::runtime::Runtime::new()?;
        let result = rt.block_on(sub.execute("explore", "analyze this", 0, None));
        assert!(result.is_err());
        let Err(err) = result else {
            anyhow::bail!("expected error")
        };
        assert!(
            !err.contains("not found"),
            "should not say 'not found': {err}"
        );
        Ok(())
    }

    #[test]
    fn build_request_preloads_mentioned_skills() -> anyhow::Result<()> {
        let sub = new_subagent(vec![skill("bash", "commands", "content")], vec![])?;
        let _ = sub.build_request("test", "/bash run something", 0, None);
        // "test" is not found, but it should still scan the query.
        // Wait, build_request returns Err if name not found.
        let sub = new_subagent(
            vec![
                skill("bash", "commands", "content"),
                skill("test", "d", "c"),
            ],
            vec![],
        )?;
        let _ = sub.build_request("test", "/bash run something", 0, None);
        let loaded = crate::tools::safe_lock(&sub.loaded_skills);
        assert!(
            loaded.contains("bash"),
            "skill mentioned via /bash must be preloaded"
        );
        Ok(())
    }

    #[test]
    fn build_request_preloads_self_if_skill() -> anyhow::Result<()> {
        let sub = new_subagent(vec![skill("bash", "commands", "content")], vec![])?;
        let _ = sub.build_request("bash", "run something", 0, None);
        let loaded = crate::tools::safe_lock(&sub.loaded_skills);
        assert!(
            loaded.contains("bash"),
            "subagent that is a skill must preload itself"
        );
        Ok(())
    }

    #[test]
    fn build_request_preloads_skills_from_agent_content() -> anyhow::Result<()> {
        let skills = vec![
            skill("explore", "explorer", "explore content"),
            skill("filesystem", "files", "fs content"),
        ];
        let agents = vec![agent(
            "review",
            "reviewer",
            "Use /explore and /filesystem to analyze code.",
        )];
        let sub = new_subagent(skills, agents)?;
        let _ = sub.build_request("review", "check this code", 0, None);
        let loaded = crate::tools::safe_lock(&sub.loaded_skills);
        assert!(
            loaded.contains("explore"),
            "skills from agent content must be preloaded"
        );
        assert!(
            loaded.contains("filesystem"),
            "skills from agent content must be preloaded"
        );
        Ok(())
    }

    // ── Tool building ──────────────────────────────────────────────

    #[test]
    fn build_tools_has_core_tools_at_all_depths() -> anyhow::Result<()> {
        let sub = new_subagent(vec![], vec![])?;
        for depth in 0..=2 {
            let tools = sub.build_tools(depth, None);
            let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
            assert!(names.contains(&"shell"), "depth {depth}: must have shell");
            assert!(
                names.contains(&"load_skills"),
                "depth {depth}: must have load_skills"
            );
            assert!(
                names.contains(&"load_references"),
                "depth {depth}: must have load_references"
            );
            assert!(
                names.contains(&"execute_skill_script"),
                "depth {depth}: must have execute_skill_script"
            );
        }
        Ok(())
    }

    #[test]
    fn build_tools_includes_subagent_below_max_depth() -> anyhow::Result<()> {
        let sub = new_subagent(vec![], vec![])?;
        let tools_0 = sub.build_tools(0, None);
        let tools_1 = sub.build_tools(1, None);
        let tools_2 = sub.build_tools(2, None);
        assert!(
            tools_0.iter().any(|t| t.name == "subagent"),
            "depth 0 must have subagent"
        );
        assert!(
            tools_1.iter().any(|t| t.name == "subagent"),
            "depth 1 must have subagent"
        );
        assert!(
            !tools_2.iter().any(|t| t.name == "subagent"),
            "depth 2 must NOT have subagent"
        );
        Ok(())
    }

    #[test]
    fn make_subagent_tool_description_is_set() -> anyhow::Result<()> {
        let tool = make_subagent_tool(
            dummy_model()?,
            Arc::new(Registry {
                agents: Vec::new(),
                skills: Vec::new(),
                completions: Vec::new(),
            }),
            Arc::new(SandboxConfig::default()),
            None,
            0,
        );
        assert!(
            !tool.description.is_empty(),
            "subagent tool must have a description"
        );
        Ok(())
    }
}
