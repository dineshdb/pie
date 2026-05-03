use crate::agent::Agent;
use crate::config::pie_home;
use crate::db::DbPool;
use crate::instructions::Instructions;
use crate::skill::Skill;
use crate::tools::plan::PlanRepo;
use crate::utils::{find_upward_in_repo, git_repo_root, load_file};
use minijinja::Environment;
use std::sync::Arc;

const SYSTEM_PROMPT_TEMPLATE: &str = include_str!("../.pie/SYSTEM.md");

#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunMode {
    Cli,
    Tui,
}

/// A structured builder for rendering the system prompt with its full context.
pub struct SystemPrompt<'a> {
    skills: &'a [Skill],
    agents: &'a [Agent],
    pub loaded_skills: Vec<&'a str>,
    agent: Option<&'a Agent>,
    json_output: bool,
    mode: RunMode,
    pool: Option<Arc<DbPool>>,
    session_id: Option<String>,
}

impl<'a> SystemPrompt<'a> {
    /// Create a new system prompt context from base registries.
    pub fn new(skills: &'a [Skill], agents: &'a [Agent]) -> Self {
        Self {
            skills,
            agents,
            loaded_skills: Vec::new(),
            agent: None,
            json_output: false,
            mode: RunMode::Tui,
            pool: None,
            session_id: None,
        }
    }

    /// Add plan repository for context.
    pub fn with_plan(mut self, pool: Arc<DbPool>, session_id: String) -> Self {
        self.pool = Some(pool);
        self.session_id = Some(session_id);
        self
    }

    /// Use a specific agent persona.
    pub fn with_agent(mut self, name: Option<&str>) -> Self {
        self.agent = name.and_then(|n| self.agents.iter().find(|a| a.name == n));
        self
    }

    /// Set whether JSON output mode is enabled.
    pub fn with_json(mut self, json_output: bool) -> Self {
        self.json_output = json_output;
        self
    }

    /// Set the run mode (CLI or TUI).
    pub fn with_mode(mut self, mode: RunMode) -> Self {
        self.mode = mode;
        self
    }

    /// Resolve all requirements (skills and their dependencies) from instructions.
    pub fn resolve(mut self, instructions: &Instructions) -> Self {
        let mentions: Vec<String> = instructions.mentions.iter().cloned().collect();
        self.loaded_skills = Skill::resolve(self.skills, &mentions)
            .into_iter()
            .map(|s| s.name.as_str())
            .collect();
        self
    }

    /// Render the final system prompt string.
    pub fn render(&self) -> String {
        let (global_agents_md, local_agents_md) = {
            (
                load_file(pie_home().join("AGENTS.md")).unwrap_or_default(),
                find_upward_in_repo("AGENTS.md").unwrap_or_default(),
            )
        };

        let agent_name = self.agent.map(|a| a.name.as_str());
        let agent_content = self.agent.map(|a| a.content.as_str());
        let interactivity = self.agent.map_or("none", |a| a.interactivity.as_ref());

        let steps = if let (Some(pool), Some(session_id)) = (&self.pool, &self.session_id) {
            pool.load_steps(session_id).unwrap_or_default()
        } else {
            Vec::new()
        };

        let (date, pwd) = Self::context_vars();
        render_template(
            "system_prompt",
            SYSTEM_PROMPT_TEMPLATE,
            minijinja::context! {
                agent_name,
                agent_content,
                skills => self.skills,
                agents => self.agents,
                loaded_skills => self.loaded_skills,
                global_agents_md,
                local_agents_md,
                date,
                pwd,
                repo_root => git_repo_root(),
                json_output => self.json_output,
                run_mode => self.mode,
                interactivity,
                steps,
            },
        )
    }

    pub fn context_vars() -> (String, String) {
        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        let pwd = std::env::var("PWD").unwrap_or_else(|_| {
            std::env::current_dir()
                .unwrap_or_default()
                .display()
                .to_string()
        });
        (date, pwd)
    }
}

/// Render a `MiniJinja` template with context, falling back to raw template on error.
#[allow(clippy::expect_used)]
fn render_template(template_name: &str, template: &str, ctx: minijinja::Value) -> String {
    let mut env = Environment::new();
    env.add_template(template_name, template)
        .expect("invalid template");
    env.get_template(template_name)
        .expect("template just added")
        .render(ctx)
        .unwrap_or_else(|e| {
            tracing::warn!("{template_name} template render error: {e}, using raw template");
            template.to_string()
        })
}

#[cfg(test)]
mod test_helpers {
    use super::*;
    use crate::agent::Agent;
    use crate::skill::Skill;

    pub fn skill(name: &str, desc: &str, content: &str) -> Skill {
        Skill {
            name: name.to_string(),
            description: desc.to_string(),
            content: content.to_string(),
            needs: Vec::new(),
        }
    }

    /// Render the main agent prompt with deterministic values.
    pub fn render_main(skills: &[Skill], json_output: bool) -> String {
        SystemPrompt::new(skills, &[])
            .with_json(json_output)
            .render()
    }

    /// Render the subagent prompt with deterministic values.
    pub fn render_sub() -> String {
        let agent = Agent {
            name: "test-agent".to_string(),
            description: "test".to_string(),
            interactivity: crate::agent::Interactivity::None,
            model: None,
            temperature: None,
            content: "You are a test agent.".to_string(),
        };
        let agents = vec![agent];

        SystemPrompt::new(&[], &agents)
            .with_agent(Some("test-agent"))
            .render()
    }
}

#[cfg(test)]
mod tests {
    use super::test_helpers::*;

    #[test]
    fn subagent_with_agent_name_includes_persona() {
        let result = render_sub();
        let role = result.split("Agent Role").nth(1).unwrap_or("");
        assert!(
            role.contains("You are a test agent."),
            "subagent with agent_name must include agent_content persona"
        );
    }

    // ── Repo-awareness ─────────────────────────────────────────

    #[test]
    fn main_agent_does_not_hardcode_repo_instructions() {
        let result = render_main(&[], false);
        assert!(
            !result.contains("/my/project"),
            "repo root must not be hardcoded in system prompt"
        );
    }

    #[test]
    fn main_agent_outside_repo_has_no_repo_instructions() {
        let result = render_main(&[], false);
        assert!(
            !result.contains("git repo"),
            "should not mention git repo when not in one"
        );
    }

    #[test]
    fn subagent_in_repo_cannot_delegate_further() {
        let result = render_sub();
        let repo_section = result.split("git repo").nth(1).unwrap_or("");
        assert!(
            !repo_section.contains("subagent"),
            "subagent repo section must not reference subagent spawning"
        );
    }

    // ── Config layering ─────────────────────────────────────────

    #[test]
    fn skills_appear_only_when_provided() {
        let with = render_main(&[skill("my-skill", "desc", "content")], false);
        let without = render_main(&[], false);
        assert!(with.contains("my-skill"), "provided skill must appear");
        assert!(
            !without.contains("my-skill"),
            "missing skill must not appear"
        );
    }

    #[test]
    fn runtime_context_includes_date_and_working_directory() {
        unsafe { std::env::set_var("PWD", "/test/project") };
        let result = render_main(&[], false);
        assert!(result.contains('-'), "date must appear"); // Simple check for date format
        assert!(result.contains("/test/project"), "pwd must appear");
    }

    #[test]
    fn json_output_mode_injected_when_enabled() {
        let with = render_main(&[], true);
        let without = render_main(&[], false);
        assert!(
            with.contains("JSON Output Mode"),
            "json output mode must appear when enabled"
        );
        assert!(
            !without.contains("JSON Output Mode"),
            "json output mode must not appear when disabled"
        );
    }

    // ── Template integrity ─────────────────────────────────────────

    #[test]
    fn all_template_variables_resolve() {
        let result = render_main(&[skill("bash", "commands", "content")], false);
        assert!(!result.contains("{%"), "unrendered Jinja block tag");
        assert!(!result.contains("{{"), "unrendered Jinja expression");
    }
}
