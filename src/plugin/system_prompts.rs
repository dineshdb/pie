use crate::config::pie_home;
use crate::utils::{find_upward_in_repo, load_file};
use agentsdk::{AgentPlugin, Messages, PluginContext};
use async_trait::async_trait;
use std::borrow::Cow;

#[derive(Debug, Default)]
pub struct SystemPromptsPlugin;

impl SystemPromptsPlugin {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl AgentPlugin for SystemPromptsPlugin {
    fn name(&self) -> &'static str {
        "system_prompts"
    }

    async fn prepare_system_prompt(
        &mut self,
        _ctx: &PluginContext,
        _history: &Messages,
    ) -> Option<Cow<'static, str>> {
        let global = load_file(pie_home().join("SYSTEM.md")).unwrap_or_default();
        let local = find_upward_in_repo("SYSTEM.md").unwrap_or_default();

        if global.is_empty() && local.is_empty() {
            return None;
        }

        let mut parts = Vec::new();
        if !global.is_empty() {
            parts.push(format!("### Global System Prompts\n\n{global}"));
        }
        if !local.is_empty() {
            parts.push(format!("### Project System Prompts\n\n{local}"));
        }

        let joined = parts.join("\n\n---\n\n");
        Some(Cow::Owned(joined))
    }
}

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
        _ctx: &PluginContext,
        _history: &Messages,
    ) -> Option<Cow<'static, str>> {
        Some(Cow::Owned(self.prompt.clone()))
    }
}
