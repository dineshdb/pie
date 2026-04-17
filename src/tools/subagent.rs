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

/// State for a single subagent execution: tracks which skills/references are
/// already loaded so the LLM cannot double-load them.
pub(crate) struct Subagent {
    model: Model,
    skills: Vec<Skill>,
    agents: Vec<Agent>,
    sandbox_settings: PathBuf,
    loaded_skills: Arc<Mutex<HashSet<String>>>,
    loaded_refs: Arc<Mutex<HashSet<String>>>,
}

impl Subagent {
    /// Create a new subagent context with empty tracking state.
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

    /// Mark skills as already loaded (e.g. auto-loaded from the initial query
    /// mention) so the `load_skills` tool will skip them.
    pub fn preload_skills(&self, names: &[String]) {
        let mut loaded = self.loaded_skills.lock().unwrap();
        for name in names {
            loaded.insert(name.clone());
        }
    }

    /// Resolve the skills mentioned by the skill_name + query, preload them
    /// into tracking, and build the user message. Returns the user content.
    fn build_user_message(&self, skill_name: &str, query: &str) -> String {
        let query_with_skill = format!("/{} {}", skill_name, query);
        let resolved = prompt::resolve_mentioned(&[&query_with_skill], &self.skills);
        let resolved_names: Vec<String> = resolved.iter().map(|s| s.name.clone()).collect();
        self.preload_skills(&resolved_names);

        let (date, pwd) = prompt::context_vars();
        format!("Date: {date} Working directory: {pwd}\n\nQuery: {query}")
    }

    /// Render the system prompt with full context: loaded skills (with
    /// content), available skills, available agents, and the resolved agent
    /// name (if the skill_name matches an agent).
    fn build_system_prompt(&self, skill_name: &str) -> String {
        let loaded_names = self.loaded_skills.lock().unwrap().clone();
        let loaded: Vec<&Skill> = self
            .skills
            .iter()
            .filter(|s| loaded_names.contains(&s.name))
            .collect();
        let agent_name = self
            .agents
            .iter()
            .find(|a| a.name == skill_name)
            .map(|a| a.name.as_str());
        prompt::subagent_prompt(
            crate::utils::git_repo_root(),
            &self.skills,
            &self.agents,
            agent_name,
            &loaded,
        )
    }

    /// Build all tools for this subagent. Reuses the global `load_skills_tool`
    /// with tracking enabled.
    fn build_tools(&self) -> Vec<Tool> {
        vec![
            shell_tool(self.sandbox_settings.clone()),
            load_skills_tool(self.skills.clone(), Some(self.loaded_skills.clone())),
            load_references_tool(self.loaded_refs.clone()),
        ]
    }

    /// Run the subagent: resolve the target skill, preload it, build prompt,
    /// and execute until the step limit is reached or the model returns text.
    pub async fn execute(self, skill_name: &str, query: &str) -> Result<String, String> {
        if skill_name.is_empty() || query.is_empty() {
            return Err("skill_name and query are required".to_string());
        }
        if !self.skills.iter().any(|s| s.name == skill_name) {
            return Ok(format!("Skill '{}' not found.", skill_name));
        }

        let sys = self.build_system_prompt(skill_name);
        let user_content = self.build_user_message(skill_name, query);

        let messages = vec![aisdk::core::Message::User(aisdk::core::UserMessage::new(
            user_content,
        ))];

        tracing::debug!(skill = %skill_name, query, %sys, "subagent");

        let tools = self.build_tools();
        let mut req = LanguageModelRequest::builder()
            .model(self.model.clone())
            .system(sys)
            .messages(messages);
        for tool in tools {
            req = req.with_tool(tool);
        }
        let mut req = req.stop_when(step_count_is(10)).build();

        match req.generate_text().await {
            Ok(r) => {
                let text = r.text().unwrap_or_default();
                tracing::debug!(skill = %skill_name, len = text.len(), %text, "subagent done");
                Ok(text)
            }
            Err(e) => Err(format!("Subagent failed: {e}")),
        }
    }
}

// ── Input schemas ──────────────────────────────────────────────────

#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct SubagentInput {
    skill_name: String,
    query: String,
}

// ── Public entry point ─────────────────────────────────────────────

/// Create the outer `subagent` tool that the main agent calls.
/// Each invocation spawns a fresh `Subagent` with independent tracking state.
pub fn subagent_tool(
    model: Model,
    skills: Vec<Skill>,
    agents: Vec<Agent>,
    sandbox_settings: PathBuf,
) -> Tool {
    Tool::builder()
        .name("subagent")
        .description(
            "Delegate a task after adding more details such as /<skill-mentions>, \
             requirements, details, etc.",
        )
        .input_schema(schemars::schema_for!(SubagentInput))
        .execute(ToolExecute::from_async(move |_ctx, params| {
            let subagent = Subagent::new(
                model.clone(),
                skills.clone(),
                agents.clone(),
                sandbox_settings.clone(),
            );
            let skill_name = params["skill_name"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let query = params["query"].as_str().unwrap_or_default().to_string();
            async move { subagent.execute(&skill_name, &query).await }
        }))
        .build()
        .unwrap()
}
