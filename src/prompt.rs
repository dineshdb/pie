use crate::agent::{Agent, OutputMode};
use crate::instructions::Instructions;
use crate::registry::Skill;
use crate::utils::{AnonymizedPath, git_repo_root};
use anyhow::{Context, Result};
use minijinja::Environment;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::OnceLock;

const SYSTEM_PROMPT_TEMPLATE: &str = include_str!("../.pie/SYSTEM.md");

static TEMPLATE_ENV: OnceLock<Environment<'static>> = OnceLock::new();

fn template_env() -> &'static Environment<'static> {
    TEMPLATE_ENV.get_or_init(|| {
        let mut env = Environment::new();
        if let Err(e) = env.add_template("system_prompt", SYSTEM_PROMPT_TEMPLATE) {
            tracing::error!("invalid system prompt template: {e}");
        }
        env
    })
}

/// Context for static project and environment metadata.
#[derive(Debug, Serialize)]
pub struct ExtraContext {
    pub os: String,
    pub arch: String,
    pub hostname: String,
    pub pwd: AnonymizedPath,
    pub repo_root: Option<AnonymizedPath>,
    pub project_files: Vec<String>,
    pub date: String,
    pub environment: HashMap<String, String>,
}

/// Context for rendering the system prompt.
#[derive(Debug, Serialize)]
pub struct SystemPromptCtx<'a> {
    pub agent_name: Option<&'a str>,
    pub agent_content: Option<&'a str>,
    pub output_mode: OutputMode,
    pub skills: &'a [Skill],
    pub agents: &'a [Agent],
    pub loaded_skills: &'a [Skill],
    pub extra_context: ExtraContext,
}

impl<'a> SystemPromptCtx<'a> {
    pub fn new(sp: &'a SystemPrompt<'a>) -> Self {
        let agent_name = sp.agent.map(|a| a.name.as_str());
        let agent_content = sp.agent.map(|a| a.content.as_str());

        let output_mode = sp
            .output_mode
            .unwrap_or_else(|| sp.agent.map_or(OutputMode::Md, |a| a.output_mode));

        let (date, pwd, os, arch, hostname) = SystemPrompt::env_vars();
        let pwd = AnonymizedPath::from(pwd);

        let repo_root_real = git_repo_root();
        let project_files = if let Some(ref root) = repo_root_real {
            discover_project_files(root)
        } else {
            Vec::new()
        };
        let repo_root = repo_root_real.map(AnonymizedPath::from);

        let mut environment = HashMap::new();
        for (k, v) in std::env::vars() {
            if k == "PATH" || k == "USER" || k == "SHELL" || k == "TERM" || k.starts_with("PIE_") {
                environment.insert(k, v);
            }
        }

        let extra_context = ExtraContext {
            os,
            arch,
            hostname,
            pwd,
            repo_root,
            project_files,
            date,
            environment,
        };

        Self {
            agent_name,
            agent_content,
            output_mode,
            skills: sp.skills,
            agents: sp.agents,
            loaded_skills: &sp.loaded_skills,
            extra_context,
        }
    }
}

fn discover_project_files(root: &str) -> Vec<String> {
    let important = [
        "README.md",
        "Justfile",
        "Cargo.toml",
        "package.json",
        "Makefile",
        "pyproject.toml",
        "go.mod",
        "TASKS.md",
        "MEMORY.md",
        "GEMINI.md",
    ];
    let root_path = std::path::Path::new(root);
    important
        .iter()
        .filter(|f| root_path.join(f).exists())
        .map(ToString::to_string)
        .collect()
}

/// A structured builder for rendering the system prompt with its full context.
pub struct SystemPrompt<'a> {
    skills: &'a [Skill],
    agents: &'a [Agent],
    pub loaded_skills: Vec<Skill>,
    agent: Option<&'a Agent>,
    output_mode: Option<OutputMode>,
}

impl<'a> SystemPrompt<'a> {
    /// Create a new system prompt context from base registries.
    pub fn new(skills: &'a [Skill], agents: &'a [Agent]) -> Self {
        Self {
            skills,
            agents,
            loaded_skills: Vec::new(),
            agent: None,
            output_mode: None,
        }
    }

    /// Use a specific agent persona.
    pub fn with_agent(mut self, name: Option<&str>) -> Self {
        self.agent = name.and_then(|n| self.agents.iter().find(|a| a.name == n));
        self
    }

    #[cfg(test)]
    /// Set the output mode.
    pub fn with_output_mode(mut self, output_mode: OutputMode) -> Self {
        self.output_mode = Some(output_mode);
        self
    }

    /// Resolve all requirements (skills and their dependencies) from instructions.
    pub fn resolve(mut self, instructions: &Instructions) -> Self {
        let mut mentions: Vec<String> = instructions.mentions.iter().cloned().collect();

        if let Some(agent) = self.agent {
            for need in &agent.needs {
                if !mentions.contains(need) {
                    mentions.push(need.clone());
                }
            }
        }

        let resolved = crate::registry::resolve_skills(self.skills, &mentions);
        self.loaded_skills = resolved.iter().map(|&s| s.clone()).collect();

        self
    }

    /// Render the final system prompt string.
    pub fn render(&self) -> Result<String> {
        let ctx = SystemPromptCtx::new(self);
        render_template(&ctx)
    }

    pub fn env_vars() -> (String, String, String, String, String) {
        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        let pwd = std::env::var("PWD").unwrap_or_else(|_| {
            std::env::current_dir()
                .unwrap_or_default()
                .display()
                .to_string()
        });
        let os = std::env::consts::OS.to_string();
        let arch = std::env::consts::ARCH.to_string();
        let hostname = hostname::get().map_or_else(
            |_| "unknown".to_string(),
            |h| h.to_string_lossy().to_string(),
        );

        (date, pwd, os, arch, hostname)
    }
}

/// Render a `MiniJinja` template with context.
fn render_template<T: Serialize>(ctx: &T) -> Result<String> {
    let env = template_env();
    let template_obj = env
        .get_template("system_prompt")
        .context("system prompt template missing")?;

    template_obj
        .render(ctx)
        .map_err(|e| anyhow::anyhow!("Template render error: {e}"))
}

#[cfg(test)]
mod test_helpers {
    use super::*;
    use crate::agent::Agent;
    use crate::registry::Skill;

    pub fn skill(name: &str, desc: &str, content: &str) -> Skill {
        Skill {
            name: name.to_string(),
            description: desc.to_string(),
            content: content.to_string(),
            needs: Vec::new(),
            references: Vec::new(),
            extra: HashMap::new(),
            path: std::path::PathBuf::new(),
        }
    }

    /// Render the main agent prompt with deterministic values.
    #[allow(clippy::expect_used)]
    pub fn render_main(skills: &[Skill], output_mode: OutputMode) -> String {
        SystemPrompt::new(skills, &[])
            .with_output_mode(output_mode)
            .render()
            .expect("test render main")
    }

    /// Render the subagent prompt with deterministic values.
    pub fn render_sub() -> String {
        let agent = Agent {
            name: "test-agent".to_string(),
            description: "test".to_string(),
            output_mode: OutputMode::Md,
            model: None,
            temperature: None,
            content: "You are a test agent.".to_string(),
            needs: vec![],
            tools: vec![],
            sandbox: None,
            grants: vec![],
        };
        let agents = vec![agent];

        SystemPrompt::new(&[], &agents)
            .with_agent(Some("test-agent"))
            .render()
            .expect("test render sub")
    }
}

#[cfg(test)]
mod tests {
    use super::test_helpers::*;
    use crate::agent::OutputMode;

    #[tokio::test]
    async fn subagent_with_agent_name_includes_persona() {
        let result = render_sub();
        let role = result.split("Agent Role").nth(1).unwrap_or("");
        assert!(
            role.contains("You are a test agent."),
            "subagent with agent_name must include agent_content persona"
        );
    }

    // ── Repo-awareness ─────────────────────────────────────────

    #[tokio::test]
    async fn main_agent_does_not_hardcode_repo_instructions() {
        let result = render_main(&[], OutputMode::Md);
        assert!(
            !result.contains("/my/project"),
            "repo root must not be hardcoded in system prompt"
        );
    }

    #[tokio::test]
    async fn main_agent_outside_repo_has_no_repo_instructions() {
        let result = render_main(&[], OutputMode::Md);
        assert!(
            !result.contains("git repo"),
            "should not mention git repo when not in one"
        );
    }

    #[tokio::test]
    async fn subagent_in_repo_cannot_delegate_further() {
        let result = render_sub();
        let repo_section = result.split("git repo").nth(1).unwrap_or("");
        assert!(
            !repo_section.contains("subagent"),
            "subagent repo section must not reference subagent spawning"
        );
    }

    #[tokio::test]
    async fn runtime_context_includes_date_and_working_directory() {
        unsafe { std::env::set_var("PWD", "/test/project") };
        let result = render_main(&[], OutputMode::Md);
        assert!(result.contains('-'), "date must appear");
        assert!(result.contains("/test/project"), "pwd must appear");
    }

    #[tokio::test]
    async fn all_template_variables_resolve() {
        let result = render_main(&[skill("bash", "commands", "content")], OutputMode::Md);
        assert!(!result.contains("{%"), "unrendered Jinja block tag");
        assert!(!result.contains("{{"), "unrendered Jinja expression");
    }
}
