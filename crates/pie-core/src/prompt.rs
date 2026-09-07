use crate::agent::{Agent, OutputMode};
use crate::registry::Skill;
use crate::utils::{AnonymizedPath, git_repo_root};
use anyhow::{Context, Result};
use minijinja::Environment;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::OnceLock;

const SYSTEM_PROMPT_TEMPLATE: &str = include_str!("../../../.pie/SYSTEM.md");

static TEMPLATE_ENV: OnceLock<Environment<'static>> = OnceLock::new();

fn template_env() -> &'static Environment<'static> {
    TEMPLATE_ENV.get_or_init(|| {
        let mut env = Environment::new();
        env.add_filter(
            "env_xml",
            |value: minijinja::Value| -> Result<String, minijinja::Error> {
                let json_str = serde_json::to_string(&value).map_err(|e| {
                    minijinja::Error::new(
                        minijinja::ErrorKind::BadSerialization,
                        format!("env_xml serialize: {e}"),
                    )
                })?;
                let ctx: ExtraContext = serde_json::from_str(&json_str).map_err(|e| {
                    minijinja::Error::new(
                        minijinja::ErrorKind::BadSerialization,
                        format!("env_xml deserialize: {e}"),
                    )
                })?;
                Ok(render_env_xml(&ctx))
            },
        );
        if let Err(e) = env.add_template("system_prompt", SYSTEM_PROMPT_TEMPLATE) {
            tracing::error!("invalid system prompt template: {e}");
        }
        env
    })
}

/// Context for static project and environment metadata.
#[derive(Debug, Serialize, Deserialize)]
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
    agent: Option<&'a Agent>,
    output_mode: Option<OutputMode>,
}

impl<'a> SystemPrompt<'a> {
    /// Create a new system prompt context from base registries.
    pub fn new(skills: &'a [Skill], agents: &'a [Agent]) -> Self {
        Self {
            skills,
            agents,
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

#[allow(clippy::format_push_string)]
/// Render environment context as structured XML for the LLM.
fn render_env_xml(ctx: &ExtraContext) -> String {
    let mut xml = String::from("<env>\n");

    xml.push_str(&format!("  <os>{}</os>\n", xml_escape(&ctx.os)));
    xml.push_str(&format!("  <arch>{}</arch>\n", xml_escape(&ctx.arch)));
    xml.push_str(&format!(
        "  <hostname>{}</hostname>\n",
        xml_escape(&ctx.hostname)
    ));
    xml.push_str(&format!("  <date>{}</date>\n", xml_escape(&ctx.date)));
    xml.push_str(&format!(
        "  <pwd>{}</pwd>\n",
        xml_escape(&ctx.pwd.to_string())
    ));

    match &ctx.repo_root {
        Some(root) => xml.push_str(&format!(
            "  <repo>{}</repo>\n",
            xml_escape(&root.to_string())
        )),
        None => xml.push_str("  <repo>(none)</repo>\n"),
    }

    if !ctx.project_files.is_empty() {
        xml.push_str("  <project_files>\n");
        for f in &ctx.project_files {
            xml.push_str(&format!("    <file>{}</file>\n", xml_escape(f)));
        }
        xml.push_str("  </project_files>\n");
    }

    if !ctx.environment.is_empty() {
        let mut pairs: Vec<_> = ctx.environment.iter().collect();
        pairs.sort_by(|a, b| a.0.cmp(b.0));
        xml.push_str("  <vars>\n");
        for (k, v) in pairs {
            xml.push_str(&format!(
                "    <var name=\"{}\">{}</var>\n",
                xml_escape(k),
                xml_escape(v)
            ));
        }
        xml.push_str("  </vars>\n");
    }

    xml.push_str("</env>");
    xml
}

/// Minimal XML escaping for element content and attribute values.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod test_helpers {
    use super::*;
    use crate::registry::Skill;
    use agentsdk_plugin_skills::LoadStatus;

    pub fn skill(name: &str, desc: &str, content: &str) -> Skill {
        Skill {
            name: name.to_string(),
            description: desc.to_string(),
            content: content.to_string(),
            needs: Vec::new(),
            status: LoadStatus::Unloaded,
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
}

#[cfg(test)]
mod tests {
    use super::test_helpers::*;
    use crate::agent::OutputMode;

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
