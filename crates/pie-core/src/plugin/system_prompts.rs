use agentsdk::{AgentPlugin, PluginContext};
use async_trait::async_trait;
use std::borrow::Cow;
use std::path::Path;

/// Build the repo-instructions plugin for a run rooted at `cwd`: repo-level
/// AGENTS.md/GEMINI.md/CLAUDE.md discovery and `${PWD}` follow the run's
/// working directory, never the process's.
pub fn build_agentsmd_plugin(
    cwd: &Path,
) -> anyhow::Result<agentsdk_plugin_agentsmd::AgentsMdPlugin> {
    let pie_home = crate::config::pie_home().to_string_lossy().to_string();
    let mut search_paths = vec![
        format!("{pie_home}/AGENTS.md"),
        format!("{pie_home}/GEMINI.md"),
        format!("{pie_home}/CLAUDE.md"),
    ];
    let project_root = crate::utils::git_repo_root_from(cwd);
    if let Some(root) = &project_root {
        search_paths.push(format!("{root}/AGENTS.md"));
        search_paths.push(format!("{root}/GEMINI.md"));
        search_paths.push(format!("{root}/CLAUDE.md"));
    }
    search_paths.push("AGENTS.md".into());
    search_paths.push("GEMINI.md".into());
    search_paths.push("CLAUDE.md".into());
    let mut builder = agentsdk_plugin_agentsmd::AgentsMdPlugin::builder()
        .search_paths(search_paths)
        .pwd(cwd.to_path_buf());
    if let Some(root) = project_root {
        builder = builder.project_root(root);
    }
    builder
        .build()
        .map_err(|e| anyhow::anyhow!("failed to build agentsmd plugin: {e}"))
}

pub struct SystemPromptComponent(pub String);

#[derive(Debug)]
pub struct EmbeddedSystemPromptPlugin {
    prompt: String,
}

impl EmbeddedSystemPromptPlugin {
    pub fn new(prompt: &str) -> Self {
        Self {
            prompt: prompt.to_string(),
        }
    }
}

#[async_trait]
impl AgentPlugin for EmbeddedSystemPromptPlugin {
    fn name(&self) -> &'static str {
        "embedded_system_prompt"
    }

    async fn prepare_system_prompt(
        &mut self,
        ctx: &mut PluginContext,
    ) -> Option<Cow<'static, str>> {
        if let Some(comp) = ctx.get::<SystemPromptComponent>() {
            return Some(Cow::Owned(comp.0.clone()));
        }
        Some(Cow::Owned(self.prompt.clone()))
    }
}
