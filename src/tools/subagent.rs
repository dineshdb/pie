use crate::agent::Agent;
use crate::prompt;
use crate::providers::Model;
use crate::skill::Skill;
use crate::tools::{load_references_tool, load_skills_tool, shell_tool};
use aisdk::core::LanguageModelRequest;
use aisdk::core::tools::{Tool, ToolExecute};
use aisdk::core::utils::step_count_is;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const MAX_DEPTH: u32 = 2;

/// Execution context for a single subagent invocation.
pub(crate) struct Subagent {
    model: Model,
    skills: Vec<Skill>,
    agents: Vec<Agent>,
    sandbox_settings: PathBuf,
    loaded_skills: Arc<Mutex<HashSet<String>>>,
    loaded_refs: Arc<Mutex<HashSet<String>>>,
}

impl Subagent {
    pub fn new(
        model: Model,
        skills: Vec<Skill>,
        agents: Vec<Agent>,
        sandbox_settings: PathBuf,
    ) -> Self {
        Self {
            model,
            skills,
            agents,
            sandbox_settings,
            loaded_skills: Arc::new(Mutex::new(HashSet::new())),
            loaded_refs: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    pub fn preload_skills(&self, names: &[String]) {
        let mut loaded = crate::tools::safe_lock(&self.loaded_skills);
        for name in names {
            loaded.insert(name.clone());
        }
    }

    fn build_user_message(&self, name: &str, query: &str) -> String {
        let query_with_mention = format!("/{name} {query}");
        let resolved = prompt::resolve_mentioned(&[&query_with_mention], &self.skills);
        let resolved_names: Vec<String> = resolved.iter().map(|s| s.name.clone()).collect();
        self.preload_skills(&resolved_names);

        // Also scan agent content for skill mentions if this is an agent
        if let Some(agent) = self.agents.iter().find(|a| a.name == name) {
            let agent_skills = prompt::resolve_mentioned(&[agent.content.as_str()], &self.skills);
            let agent_skill_names: Vec<String> =
                agent_skills.iter().map(|s| s.name.clone()).collect();
            self.preload_skills(&agent_skill_names);
        }

        let (date, pwd) = prompt::context_vars();
        format!("Date: {date} Working directory: {pwd}\n\nQuery: {query}")
    }

    fn build_system_prompt(&self, name: &str) -> String {
        let loaded_names = crate::tools::safe_lock(&self.loaded_skills).clone();
        let loaded: Vec<&Skill> = self
            .skills
            .iter()
            .filter(|s| loaded_names.contains(&s.name))
            .collect();
        let agent_name = self
            .agents
            .iter()
            .find(|a| a.name == name)
            .map(|a| a.name.as_str());
        prompt::subagent_prompt(
            crate::utils::git_repo_root(),
            &self.skills,
            &self.agents,
            agent_name,
            &loaded,
        )
    }

    fn build_tools(&self, depth: u32, parent_id: Option<Uuid>) -> Vec<Tool> {
        let mut tools = vec![
            shell_tool(self.sandbox_settings.clone()),
            load_skills_tool(self.skills.clone(), Some(self.loaded_skills.clone())),
            load_references_tool(self.loaded_refs.clone()),
        ];
        if depth < MAX_DEPTH {
            tools.push(make_subagent_tool(
                self.model.clone(),
                self.skills.clone(),
                self.agents.clone(),
                self.sandbox_settings.clone(),
                parent_id,
                depth + 1,
            ));
        }
        tools
    }

    pub async fn execute(
        self,
        name: &str,
        query: &str,
        depth: u32,
        parent_id: Option<Uuid>,
    ) -> Result<String, String> {
        if name.is_empty() || query.is_empty() {
            return Err("name and query are required".to_string());
        }
        let is_agent = self.agents.iter().any(|a| a.name == name);
        let is_skill = self.skills.iter().any(|s| s.name == name);
        if !is_agent && !is_skill {
            return Ok(format!("'{name}' not found as agent or skill."));
        }

        let sys = self.build_system_prompt(name);
        let user_content = self.build_user_message(name, query);

        let messages = vec![aisdk::core::Message::User(aisdk::core::UserMessage::new(
            user_content,
        ))];

        tracing::debug!(name, query, %sys, "subagent");

        let tools = self.build_tools(depth, parent_id);
        let mut req = LanguageModelRequest::builder()
            .model(self.model.clone())
            .system(sys)
            .messages(messages);
        for tool in tools {
            req = req.with_tool(tool);
        }
        let mut req = req.stop_when(step_count_is(20)).build();

        match req.generate_text().await {
            Ok(r) => {
                let text = r.text().unwrap_or_default();
                tracing::debug!(name, len = text.len(), %text, "subagent done");
                Ok(text)
            }
            Err(e) => Err(format!("Subagent failed: {e}")),
        }
    }
}

// ── Input schemas ──────────────────────────────────────────────────

#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct SubagentInput {
    name: String,
    query: String,
}

// ── Tool creation ──────────────────────────────────────────────────

#[allow(clippy::unwrap_used)]
fn make_subagent_tool(
    model: Model,
    skills: Vec<Skill>,
    agents: Vec<Agent>,
    sandbox_settings: PathBuf,
    parent_id: Option<Uuid>,
    depth: u32,
) -> Tool {
    Tool::builder()
        .name("subagent")
        .description(if depth == 0 {
            "Delegate a task to an agent or skill by name. Add /<mentions>, \
             requirements, and details to the query."
        } else {
            "Delegate a subtask to a specialized agent. The agent will have its own \
             context and tools."
        })
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
            let subagent = Subagent::new(
                model.clone(),
                skills.clone(),
                agents.clone(),
                sandbox_settings.clone(),
            );
            async move { subagent.execute(&name, &query, depth, parent_id).await }
        }))
        .build()
        .unwrap()
}

/// Public entry point: create the `subagent` tool for the main agent.
pub fn subagent_tool(
    model: Model,
    skills: Vec<Skill>,
    agents: Vec<Agent>,
    sandbox_settings: PathBuf,
) -> Tool {
    make_subagent_tool(model, skills, agents, sandbox_settings, None, 0)
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
            skills,
            agents,
            PathBuf::from("/tmp"),
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
        let val = result.map_err(|e| anyhow::anyhow!("{e}"))?;
        assert!(val.contains("not found"));
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

    // ── User message building ──────────────────────────────────────

    #[test]
    fn build_user_message_includes_query() -> anyhow::Result<()> {
        let sub = new_subagent(vec![], vec![])?;
        let msg = sub.build_user_message("explore", "analyze this");
        assert!(msg.contains("analyze this"));
        assert!(msg.contains("Date:"));
        assert!(msg.contains("Working directory:"));
        Ok(())
    }

    #[test]
    fn build_user_message_preloads_mentioned_skills() -> anyhow::Result<()> {
        let sub = new_subagent(vec![skill("bash", "commands", "content")], vec![])?;
        sub.build_user_message("bash", "run something");
        let loaded = crate::tools::safe_lock(&sub.loaded_skills);
        assert!(
            loaded.contains("bash"),
            "skill mentioned via /bash must be preloaded"
        );
        Ok(())
    }

    #[test]
    fn build_user_message_preloads_skills_from_agent_content() -> anyhow::Result<()> {
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
        sub.build_user_message("review", "check this code");
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

    // ── System prompt building ─────────────────────────────────────

    #[test]
    fn build_system_prompt_with_agent_name() -> anyhow::Result<()> {
        let agents = vec![agent("explore", "explorer", "You explore code.")];
        let sub = new_subagent(vec![], agents)?;
        let prompt = sub.build_system_prompt("explore");
        assert!(
            prompt.contains("explore"),
            "agent name must appear in prompt"
        );
        assert!(
            prompt.contains("You explore code."),
            "agent content must appear in prompt"
        );
        Ok(())
    }

    #[test]
    fn build_system_prompt_without_agent_name() -> anyhow::Result<()> {
        let sub = new_subagent(vec![], vec![])?;
        let prompt = sub.build_system_prompt("bash");
        assert!(
            !prompt.contains("specialized agent running as"),
            "no agent persona for skill-only"
        );
        Ok(())
    }

    #[test]
    fn build_system_prompt_includes_preloaded_skills() -> anyhow::Result<()> {
        let skills = vec![skill("bash", "commands", "run commands")];
        let sub = new_subagent(skills, vec![])?;
        sub.preload_skills(&["bash".to_string()]);
        let prompt = sub.build_system_prompt("something");
        assert!(prompt.contains("bash"), "preloaded skill must appear");
        assert!(
            prompt.contains("run commands"),
            "preloaded skill content must appear"
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
            assert!(
                names.contains(&"shell_tool"),
                "depth {depth}: must have shell_tool"
            );
            assert!(
                names.contains(&"load_skills"),
                "depth {depth}: must have load_skills"
            );
            assert!(
                names.contains(&"load_references"),
                "depth {depth}: must have load_references"
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
    fn make_subagent_tool_description_changes_with_depth() -> anyhow::Result<()> {
        let tool_0 = make_subagent_tool(
            dummy_model()?,
            vec![],
            vec![],
            PathBuf::from("/tmp"),
            None,
            0,
        );
        let tool_1 = make_subagent_tool(
            dummy_model()?,
            vec![],
            vec![],
            PathBuf::from("/tmp"),
            None,
            1,
        );
        assert_ne!(
            tool_0.description, tool_1.description,
            "descriptions should differ by depth"
        );
        assert!(
            tool_0.description.contains("agent or skill"),
            "depth 0 mentions agent or skill"
        );
        Ok(())
    }
}
