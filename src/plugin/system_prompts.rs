use agentsdk::{AgentPlugin, Messages, PluginContext};
use async_trait::async_trait;
use std::borrow::Cow;

pub fn build_agentsmd_plugin() -> anyhow::Result<agentsdk_plugin_agentsmd::AgentsMdPlugin> {
    let mut search_paths = vec![format!(
        "{}/AGENTS.md",
        crate::config::pie_home().to_string_lossy()
    )];
    if let Some(root) = crate::utils::git_repo_root() {
        search_paths.push(format!("{root}/AGENTS.md"));
    }
    search_paths.push("AGENTS.md".into());
    agentsdk_plugin_agentsmd::AgentsMdPlugin::builder()
        .search_paths(search_paths)
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
        _history: &Messages,
    ) -> Option<Cow<'static, str>> {
        if let Some(comp) = ctx.get::<SystemPromptComponent>() {
            return Some(Cow::Owned(comp.0.clone()));
        }
        Some(Cow::Owned(self.prompt.clone()))
    }
}
